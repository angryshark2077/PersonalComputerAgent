import BridgeProtocol
import Darwin
import Foundation

enum BridgeServerError: Error, Equatable, Sendable {
    case invalidSocketPath
    case unsafeRunDirectory
    case unsafeSocketEntry
    case socketPathTooLong
    case socketOperationFailed
    case notStarted
    case alreadyStarted
    case timedOut
    case invalidRequest
}

struct SocketPathValidator: Sendable {
    static var productionRunRoot: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library", isDirectory: true)
            .appendingPathComponent("Application Support", isDirectory: true)
            .appendingPathComponent("PersonalComputerAgent", isDirectory: true)
            .appendingPathComponent("Run", isDirectory: true)
    }

    let approvedRunRoot: URL

    init(approvedRunRoot: URL = Self.productionRunRoot) {
        self.approvedRunRoot = approvedRunRoot
    }

    func prepareRunDirectory() throws {
        guard approvedRunRoot.path.hasPrefix("/"),
              !containsParentTraversal(approvedRunRoot.path) else {
            throw BridgeServerError.unsafeRunDirectory
        }

        var info = stat()
        if lstat(approvedRunRoot.path, &info) == 0 {
            guard info.st_mode & mode_t(S_IFMT) == mode_t(S_IFDIR),
                  info.st_uid == geteuid(),
                  info.st_mode & 0o777 == 0o700 else {
                throw BridgeServerError.unsafeRunDirectory
            }
            return
        }
        guard errno == ENOENT else { throw BridgeServerError.unsafeRunDirectory }

        let parent = approvedRunRoot.deletingLastPathComponent()
        guard lstat(parent.path, &info) == 0,
              info.st_mode & mode_t(S_IFMT) == mode_t(S_IFDIR),
              info.st_uid == geteuid() else {
            throw BridgeServerError.unsafeRunDirectory
        }
        guard mkdir(approvedRunRoot.path, 0o700) == 0 else {
            throw BridgeServerError.socketOperationFailed
        }
        guard chmod(approvedRunRoot.path, 0o700) == 0 else {
            throw BridgeServerError.socketOperationFailed
        }
    }

    func validate(socketURL: URL) throws {
        let path = socketURL.path
        guard path.hasPrefix("/"),
              !containsParentTraversal(path),
              socketURL.deletingLastPathComponent().path == approvedRunRoot.path,
              !socketURL.lastPathComponent.isEmpty,
              socketURL.lastPathComponent != ".",
              socketURL.lastPathComponent != ".." else {
            throw BridgeServerError.invalidSocketPath
        }
        let capacity = MemoryLayout.size(ofValue: sockaddr_un().sun_path)
        guard path.utf8.count < capacity else { throw BridgeServerError.socketPathTooLong }
    }

    func removeStaleSocketIfSafe(at socketURL: URL) throws {
        try validate(socketURL: socketURL)
        var info = stat()
        guard lstat(socketURL.path, &info) == 0 else {
            guard errno == ENOENT else { throw BridgeServerError.unsafeSocketEntry }
            return
        }
        guard info.st_mode & mode_t(S_IFMT) == mode_t(S_IFSOCK),
              info.st_uid == geteuid() else {
            throw BridgeServerError.unsafeSocketEntry
        }
        guard unlink(socketURL.path) == 0 else { throw BridgeServerError.socketOperationFailed }
    }

    func removeBoundSocketIfSafe(at socketURL: URL) {
        try? removeStaleSocketIfSafe(at: socketURL)
    }

    private func containsParentTraversal(_ path: String) -> Bool {
        (path as NSString).pathComponents.contains("..")
    }
}

actor BridgeServer {
    private let socketURL: URL
    private let pathValidator: SocketPathValidator
    private let handshakeHandler: HandshakeHandler
    private let handshakeTimeoutMilliseconds: UInt64
    private let idleTimeoutMilliseconds: UInt64
    private var listener: Int32 = -1
    private var connection: Int32 = -1
    private var shutdownRequested = false

    init(
        socketURL: URL,
        pathValidator: SocketPathValidator = SocketPathValidator(),
        handshakeHandler: HandshakeHandler,
        handshakeTimeoutMilliseconds: UInt64 = 1_000,
        idleTimeoutMilliseconds: UInt64 = 30_000
    ) {
        self.socketURL = socketURL
        self.pathValidator = pathValidator
        self.handshakeHandler = handshakeHandler
        self.handshakeTimeoutMilliseconds = max(handshakeTimeoutMilliseconds, 1)
        self.idleTimeoutMilliseconds = max(idleTimeoutMilliseconds, 1)
    }

    func start() throws {
        guard !shutdownRequested else { throw BridgeServerError.notStarted }
        guard listener == -1 else { throw BridgeServerError.alreadyStarted }
        try pathValidator.prepareRunDirectory()
        try pathValidator.validate(socketURL: socketURL)
        try pathValidator.removeStaleSocketIfSafe(at: socketURL)

        let descriptor = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard descriptor >= 0 else { throw BridgeServerError.socketOperationFailed }
        do {
            try configure(descriptor)
            var address = try unixAddress(for: socketURL.path)
            let addressLength = socklen_t(MemoryLayout.offset(of: \sockaddr_un.sun_path)! + socketURL.path.utf8.count + 1)
            address.sun_len = UInt8(addressLength)
            let result = withUnsafePointer(to: &address) { pointer in
                pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                    Darwin.bind(descriptor, $0, addressLength)
                }
            }
            guard result == 0,
                  chmod(socketURL.path, 0o600) == 0,
                  Darwin.listen(descriptor, 1) == 0 else {
                throw BridgeServerError.socketOperationFailed
            }
            var info = stat()
            guard lstat(socketURL.path, &info) == 0,
                  info.st_mode & mode_t(S_IFMT) == mode_t(S_IFSOCK),
                  info.st_uid == geteuid(),
                  info.st_mode & 0o777 == 0o600 else {
                throw BridgeServerError.unsafeSocketEntry
            }
            listener = descriptor
        } catch {
            Darwin.close(descriptor)
            pathValidator.removeBoundSocketIfSafe(at: socketURL)
            throw error
        }
    }

    func serve() async throws {
        guard listener >= 0 else { throw BridgeServerError.notStarted }
        while !Task.isCancelled, listener >= 0 {
            let accepted = Darwin.accept(listener, nil, nil)
            if accepted < 0 {
                if errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR {
                    try await Task.sleep(for: .milliseconds(10))
                    continue
                }
                if listener < 0 { return }
                throw BridgeServerError.socketOperationFailed
            }
            do {
                try configure(accepted)
            } catch {
                Darwin.close(accepted)
                continue
            }
            connection = accepted
            do {
                try await handleConnection(accepted)
            } catch {
                // Malformed, unauthenticated, timed-out, and disconnected peers are all closed.
            }
            closeCurrentConnection(if: accepted)
        }
    }

    func shutdown() {
        shutdownRequested = true
        if connection >= 0 {
            Darwin.shutdown(connection, SHUT_RDWR)
            Darwin.close(connection)
            connection = -1
        }
        if listener >= 0 {
            Darwin.close(listener)
            listener = -1
        }
        pathValidator.removeBoundSocketIfSafe(at: socketURL)
    }

    private func handleConnection(_ descriptor: Int32) async throws {
        var reader = SocketFrameReader()
        let challengeJSON = try await readFrame(
            descriptor,
            reader: &reader,
            timeoutMilliseconds: handshakeTimeoutMilliseconds
        )
        let handshake = try handshakeHandler.respond(to: challengeJSON)
        try await writeFrame(
            descriptor,
            payload: handshake.responseJSON,
            timeoutMilliseconds: min(handshakeTimeoutMilliseconds, handshake.deadlineMilliseconds)
        )
        guard handshake.protocolCompatible else { return }

        while !Task.isCancelled, connection == descriptor {
            let request = try await readFrame(
                descriptor,
                reader: &reader,
                timeoutMilliseconds: idleTimeoutMilliseconds
            )
            let response = try CapabilityRequestHandler.respond(to: request)
            try await writeFrame(
                descriptor,
                payload: response.payload,
                timeoutMilliseconds: response.deadlineMilliseconds
            )
        }
    }

    private func readFrame(
        _ descriptor: Int32,
        reader: inout SocketFrameReader,
        timeoutMilliseconds: UInt64
    ) async throws -> Data {
        let deadline = deadlineNanoseconds(after: timeoutMilliseconds)
        while !Task.isCancelled, connection == descriptor {
            if let frame = reader.nextFrame() { return frame }
            guard DispatchTime.now().uptimeNanoseconds < deadline else { throw BridgeServerError.timedOut }
            var bytes = [UInt8](repeating: 0, count: 16 * 1024)
            let count = Darwin.recv(descriptor, &bytes, bytes.count, 0)
            if count > 0 {
                try reader.append(Data(bytes.prefix(Int(count))))
                continue
            }
            if count == 0 { throw FrameCodecError.disconnected }
            if errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR {
                try await Task.sleep(for: .milliseconds(5))
                continue
            }
            throw FrameCodecError.inputOutput
        }
        throw FrameCodecError.disconnected
    }

    private func writeFrame(
        _ descriptor: Int32,
        payload: Data,
        timeoutMilliseconds: UInt64
    ) async throws {
        let frame = try FrameCodec.encode(payload)
        let deadline = deadlineNanoseconds(after: timeoutMilliseconds)
        var offset = 0
        while offset < frame.count, !Task.isCancelled, connection == descriptor {
            guard DispatchTime.now().uptimeNanoseconds < deadline else { throw BridgeServerError.timedOut }
            let count = frame.withUnsafeBytes { rawBuffer -> Int in
                guard let base = rawBuffer.baseAddress else { return -1 }
                return Darwin.send(descriptor, base.advanced(by: offset), frame.count - offset, 0)
            }
            if count > 0 {
                offset += count
            } else if count < 0, errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR {
                try await Task.sleep(for: .milliseconds(5))
            } else {
                throw FrameCodecError.inputOutput
            }
        }
        guard offset == frame.count else { throw FrameCodecError.disconnected }
    }

    private func closeCurrentConnection(if descriptor: Int32) {
        guard connection == descriptor else { return }
        Darwin.shutdown(descriptor, SHUT_RDWR)
        Darwin.close(descriptor)
        connection = -1
    }

    private func configure(_ descriptor: Int32) throws {
        let flags = fcntl(descriptor, F_GETFL)
        guard flags >= 0,
              fcntl(descriptor, F_SETFL, flags | O_NONBLOCK) == 0 else {
            throw BridgeServerError.socketOperationFailed
        }
        var enabled: Int32 = 1
        guard setsockopt(
            descriptor,
            SOL_SOCKET,
            SO_NOSIGPIPE,
            &enabled,
            socklen_t(MemoryLayout<Int32>.size)
        ) == 0 else {
            throw BridgeServerError.socketOperationFailed
        }
    }

    private func unixAddress(for path: String) throws -> sockaddr_un {
        let pathBytes = Array(path.utf8)
        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let capacity = MemoryLayout.size(ofValue: address.sun_path)
        guard pathBytes.count < capacity else { throw BridgeServerError.socketPathTooLong }
        withUnsafeMutablePointer(to: &address.sun_path) { pointer in
            pointer.withMemoryRebound(to: CChar.self, capacity: capacity) { target in
                for (index, byte) in pathBytes.enumerated() {
                    target[index] = CChar(bitPattern: byte)
                }
                target[pathBytes.count] = 0
            }
        }
        return address
    }

    private func deadlineNanoseconds(after milliseconds: UInt64) -> UInt64 {
        let delta = milliseconds.multipliedReportingOverflow(by: 1_000_000)
        if delta.overflow { return UInt64.max }
        let deadline = DispatchTime.now().uptimeNanoseconds.addingReportingOverflow(delta.partialValue)
        return deadline.overflow ? UInt64.max : deadline.partialValue
    }
}

private struct SocketFrameReader {
    private var decoder = FrameDecoder()
    private var frames: [Data] = []

    mutating func append(_ data: Data) throws {
        frames.append(contentsOf: try decoder.append(data))
    }

    mutating func nextFrame() -> Data? {
        guard !frames.isEmpty else { return nil }
        return frames.removeFirst()
    }
}

private enum CapabilityRequestHandler {
    private static let envelopeKeys: Set<String> = [
        "protocol_version", "request_id", "message_kind", "capability", "deadline_ms", "payload", "error",
    ]
    private static let payloadKeys: Set<String> = ["include_permissions"]

    struct Response {
        let payload: Data
        let deadlineMilliseconds: UInt64
    }

    static func respond(to requestJSON: Data) throws -> Response {
        let object = try StrictJSON.object(requestJSON)
        try StrictJSON.requireOnlyKeys(object, allowed: envelopeKeys)
        guard let payloadObject = object["payload"] as? [String: Any] else {
            throw BridgeServerError.invalidRequest
        }
        try StrictJSON.requireExactKeys(payloadObject, expected: payloadKeys)

        let request: StrictCapabilityEnvelope
        do {
            request = try JSONDecoder().decode(StrictCapabilityEnvelope.self, from: requestJSON)
        } catch {
            throw BridgeServerError.invalidRequest
        }
        guard request.protocolVersion == HandshakeHandler.protocolVersion,
              request.messageKind == .request,
              request.capability == "system.capabilities",
              request.deadlineMilliseconds > 0,
              request.error == nil else {
            throw BridgeServerError.invalidRequest
        }
        let response = BridgeEnvelope(
            protocolVersion: Int(HandshakeHandler.protocolVersion),
            requestID: request.requestID,
            messageKind: .response,
            capability: request.capability,
            deadlineMilliseconds: Int(request.deadlineMilliseconds),
            payload: ["screen_capture": .string("available")]
        )
        return Response(
            payload: try JSONEncoder().encode(response),
            deadlineMilliseconds: request.deadlineMilliseconds
        )
    }
}

private struct StrictCapabilityEnvelope: Decodable {
    let protocolVersion: UInt32
    let requestID: UUID
    let messageKind: BridgeMessageKind
    let capability: String
    let deadlineMilliseconds: UInt64
    let payload: StrictCapabilityPayload
    let error: JSONValue?

    private enum CodingKeys: String, CodingKey {
        case protocolVersion = "protocol_version"
        case requestID = "request_id"
        case messageKind = "message_kind"
        case capability
        case deadlineMilliseconds = "deadline_ms"
        case payload
        case error
    }
}

private struct StrictCapabilityPayload: Decodable {
    let includePermissions: Bool

    private enum CodingKeys: String, CodingKey {
        case includePermissions = "include_permissions"
    }
}
