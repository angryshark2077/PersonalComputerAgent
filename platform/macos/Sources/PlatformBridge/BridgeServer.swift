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
    case shutdownRequested
    case alreadyStarted
    case timedOut
    case invalidRequest
    case socketIdentityMismatch
    case cleanupFailed
}

struct SocketIdentity: Equatable, Sendable {
    let device: dev_t
    let inode: ino_t
    let owner: uid_t
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
        var createdInfo = stat()
        let createdIdentity = lstat(approvedRunRoot.path, &createdInfo) == 0
            ? SocketIdentity(device: createdInfo.st_dev, inode: createdInfo.st_ino, owner: createdInfo.st_uid)
            : nil
        guard chmod(approvedRunRoot.path, 0o700) == 0 else {
            if let createdIdentity {
                var current = stat()
                if lstat(approvedRunRoot.path, &current) == 0,
                   SocketIdentity(device: current.st_dev, inode: current.st_ino, owner: current.st_uid) == createdIdentity {
                    _ = rmdir(approvedRunRoot.path)
                }
            }
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
        guard let entry = try quarantine(socketURL) else { return }
        guard entry.fileType == mode_t(S_IFSOCK), entry.identity.owner == geteuid() else {
            try restore(entry, to: socketURL)
            throw BridgeServerError.unsafeSocketEntry
        }
        guard unlink(entry.url.path) == 0 else {
            try restore(entry, to: socketURL)
            throw BridgeServerError.cleanupFailed
        }
    }

    func removeBoundSocketIfSafe(at socketURL: URL, expectedIdentity: SocketIdentity) throws {
        try validate(socketURL: socketURL)
        guard let entry = try quarantine(socketURL) else { return }
        guard entry.fileType == mode_t(S_IFSOCK),
              entry.identity == expectedIdentity,
              entry.identity.owner == geteuid() else {
            try restore(entry, to: socketURL)
            throw BridgeServerError.socketIdentityMismatch
        }
        guard unlink(entry.url.path) == 0 else {
            try restore(entry, to: socketURL)
            throw BridgeServerError.cleanupFailed
        }
    }

    private func containsParentTraversal(_ path: String) -> Bool {
        (path as NSString).pathComponents.contains("..")
    }

    private func quarantine(_ socketURL: URL) throws -> QuarantinedEntry? {
        let quarantineURL = approvedRunRoot.appendingPathComponent(
            ".pca-quarantine-\(UUID().uuidString.lowercased())"
        )
        guard quarantineURL.deletingLastPathComponent().path == approvedRunRoot.path,
              quarantineURL.lastPathComponent.hasPrefix(".pca-quarantine-") else {
            throw BridgeServerError.cleanupFailed
        }
        guard renamex_np(socketURL.path, quarantineURL.path, UInt32(RENAME_EXCL)) == 0 else {
            if errno == ENOENT { return nil }
            throw BridgeServerError.cleanupFailed
        }
        var info = stat()
        guard lstat(quarantineURL.path, &info) == 0 else {
            throw BridgeServerError.cleanupFailed
        }
        return QuarantinedEntry(
            url: quarantineURL,
            identity: SocketIdentity(device: info.st_dev, inode: info.st_ino, owner: info.st_uid),
            fileType: info.st_mode & mode_t(S_IFMT)
        )
    }

    private func restore(_ entry: QuarantinedEntry, to socketURL: URL) throws {
        if renamex_np(entry.url.path, socketURL.path, UInt32(RENAME_EXCL)) == 0 { return }
        let preservedURL = approvedRunRoot.appendingPathComponent(
            ".pca-preserved-\(UUID().uuidString.lowercased())"
        )
        guard renamex_np(entry.url.path, preservedURL.path, UInt32(RENAME_EXCL)) == 0 else {
            throw BridgeServerError.cleanupFailed
        }
        throw BridgeServerError.cleanupFailed
    }
}

private struct QuarantinedEntry {
    let url: URL
    let identity: SocketIdentity
    let fileType: mode_t
}

actor BridgeServer {
    static let defaultIdleTimeoutMilliseconds: UInt64 = 90_000

    private let socketURL: URL
    private let pathValidator: SocketPathValidator
    private let handshakeHandler: HandshakeHandler
    private let credentialProvider: any BridgeCredentialProviding
    private let capabilityProbe: CapabilityProbe
    private let networkSource: NetworkObservationSource
    private let lifecycleSource: PlatformLifecycleEventBuffer
    private let screenSource: ScreenCaptureSource
    private let photoSource: PhotoLibrarySource
    private let handshakeTimeoutMilliseconds: UInt64
    private let credentialTimeoutMilliseconds: UInt64
    private let idleTimeoutMilliseconds: UInt64
    private var listener: Int32 = -1
    private var connection: Int32 = -1
    private var shutdownRequested = false
    private var boundSocketIdentity: SocketIdentity?
    private var shutdownFailure: BridgeServerError?

    init(
        socketURL: URL,
        pathValidator: SocketPathValidator = SocketPathValidator(),
        handshakeHandler: HandshakeHandler,
        credentialProvider: any BridgeCredentialProviding,
        capabilityProbe: CapabilityProbe = CapabilityProbe(),
        networkSource: NetworkObservationSource? = nil,
        lifecycleSource: PlatformLifecycleEventBuffer = PlatformLifecycleEventBuffer(),
        screenSource: ScreenCaptureSource = ScreenCaptureSource(),
        photoSource: PhotoLibrarySource = PhotoLibrarySource(),
        handshakeTimeoutMilliseconds: UInt64 = 1_000,
        credentialTimeoutMilliseconds: UInt64 = 1_000,
        idleTimeoutMilliseconds: UInt64 = BridgeServer.defaultIdleTimeoutMilliseconds
    ) {
        self.socketURL = socketURL
        self.pathValidator = pathValidator
        self.handshakeHandler = handshakeHandler
        self.credentialProvider = credentialProvider
        self.capabilityProbe = capabilityProbe
        self.lifecycleSource = lifecycleSource
        self.screenSource = screenSource
        self.photoSource = photoSource
        self.networkSource = networkSource ?? NetworkObservationSource(lifecycleSource: lifecycleSource)
        self.handshakeTimeoutMilliseconds = max(handshakeTimeoutMilliseconds, 1)
        self.credentialTimeoutMilliseconds = max(credentialTimeoutMilliseconds, 1)
        self.idleTimeoutMilliseconds = max(idleTimeoutMilliseconds, 1)
    }

    nonisolated func recordPowerLifecycleEvent(_ event: PowerLifecycleEvent) {
        lifecycleSource.record(event == .systemSleep ? .systemSleep : .systemWake)
    }

    func start() throws {
        guard !shutdownRequested else { throw BridgeServerError.shutdownRequested }
        guard listener == -1 else { throw BridgeServerError.alreadyStarted }
        try pathValidator.prepareRunDirectory()
        try pathValidator.validate(socketURL: socketURL)
        try pathValidator.removeStaleSocketIfSafe(at: socketURL)

        let descriptor = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard descriptor >= 0 else { throw BridgeServerError.socketOperationFailed }
        var createdIdentity: SocketIdentity?
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
            guard result == 0 else {
                throw BridgeServerError.socketOperationFailed
            }
            var info = stat()
            guard lstat(socketURL.path, &info) == 0,
                  info.st_mode & mode_t(S_IFMT) == mode_t(S_IFSOCK),
                  info.st_uid == geteuid() else {
                throw BridgeServerError.unsafeSocketEntry
            }
            createdIdentity = SocketIdentity(
                device: info.st_dev,
                inode: info.st_ino,
                owner: info.st_uid
            )
            guard chmod(socketURL.path, 0o600) == 0,
                  Darwin.listen(descriptor, 1) == 0,
                  lstat(socketURL.path, &info) == 0,
                  info.st_mode & mode_t(S_IFMT) == mode_t(S_IFSOCK),
                  info.st_uid == geteuid(),
                  info.st_mode & 0o777 == 0o600,
                  SocketIdentity(device: info.st_dev, inode: info.st_ino, owner: info.st_uid) == createdIdentity else {
                throw BridgeServerError.socketOperationFailed
            }
            boundSocketIdentity = createdIdentity
            listener = descriptor
        } catch let startupError {
            Darwin.close(descriptor)
            if let createdIdentity {
                try pathValidator.removeBoundSocketIfSafe(
                    at: socketURL,
                    expectedIdentity: createdIdentity
                )
            }
            throw startupError
        }
    }

    func serve() async throws {
        if shutdownRequested { return }
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

    func shutdown() throws {
        if shutdownRequested {
            if let shutdownFailure { throw shutdownFailure }
            return
        }
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
        if let boundSocketIdentity {
            do {
                try pathValidator.removeBoundSocketIfSafe(
                    at: socketURL,
                    expectedIdentity: boundSocketIdentity
                )
            } catch let error as BridgeServerError {
                shutdownFailure = error
                throw error
            }
        }
    }

    private func handleConnection(_ descriptor: Int32) async throws {
        var reader = SocketFrameReader()
        let challengeJSON = try await readFrame(
            descriptor,
            reader: &reader,
            timeoutMilliseconds: handshakeTimeoutMilliseconds
        )
        let challenge = try handshakeHandler.validate(challengeJSON)
        var secret = try await CredentialLoader.load(
            from: credentialProvider,
            timeoutMilliseconds: min(
                credentialTimeoutMilliseconds,
                challenge.deadlineMilliseconds
            )
        )
        let handshake: HandshakeResult
        do {
            handshake = try handshakeHandler.respond(to: challenge, secret: secret)
        } catch {
            secret.resetBytes(in: 0..<secret.count)
            throw error
        }
        secret.resetBytes(in: 0..<secret.count)
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
            let response = try CapabilityRequestHandler.respond(
                to: request,
                negotiatedProtocolVersion: challenge.protocolVersion,
                capabilityProbe: capabilityProbe,
                networkSource: networkSource,
                lifecycleSource: lifecycleSource,
                screenSource: screenSource,
                photoSource: photoSource
            )
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

enum CapabilityRequestHandler {
    private static let envelopeKeys: Set<String> = [
        "protocol_version", "request_id", "message_kind", "capability", "deadline_ms", "payload", "error",
    ]
    private static let capabilityPayloadKeys: Set<String> = ["include_permissions"]
    private static let networkPayloadKeys: Set<String> = ["include_wifi_identity"]
    private static let lifecyclePayloadKeys: Set<String> = ["after_sequence"]
    private static let screenContextPayloadKeys: Set<String> = []
    private static let screenCapturePayloadKeys: Set<String> = ["excluded_bundle_ids"]
    private static let messageDecodePayloadKeys: Set<String> = ["encoded_bodies"]
    private static let photoAuthorizationPayloadKeys: Set<String> = []
    private static let photoListPayloadKeys: Set<String> = ["after_created_at", "after_local_identifier", "cutoff", "limit"]
    private static let photoExportPayloadKeys: Set<String> = ["local_identifier", "file_name"]

    struct Response {
        let payload: Data
        let deadlineMilliseconds: UInt64
    }

    static func respond(
        to requestJSON: Data,
        negotiatedProtocolVersion: UInt32? = nil,
        capabilityProbe: CapabilityProbe = CapabilityProbe(),
        networkSource: NetworkObservationSource = NetworkObservationSource(),
        lifecycleSource: PlatformLifecycleEventBuffer = PlatformLifecycleEventBuffer(),
        screenSource: ScreenCaptureSource = ScreenCaptureSource(),
        photoSource: PhotoLibrarySource = PhotoLibrarySource()
    ) throws -> Response {
        let object = try StrictJSON.object(requestJSON)
        try StrictJSON.requireOnlyKeys(object, allowed: envelopeKeys)
        guard let payloadObject = object["payload"] as? [String: Any] else {
            throw BridgeServerError.invalidRequest
        }
        guard let capability = object["capability"] as? String else {
            throw BridgeServerError.invalidRequest
        }
        if capability == "system.capabilities" {
            try StrictJSON.requireExactKeys(payloadObject, expected: capabilityPayloadKeys)
            guard payloadObject["include_permissions"] as? Bool == true else {
                throw BridgeServerError.invalidRequest
            }
        } else if capability == "network.observe" {
            try StrictJSON.requireExactKeys(payloadObject, expected: networkPayloadKeys)
        } else if capability == "system.lifecycle.poll" {
            try StrictJSON.requireExactKeys(payloadObject, expected: lifecyclePayloadKeys)
        } else if capability == "screen.context" {
            try StrictJSON.requireExactKeys(payloadObject, expected: screenContextPayloadKeys)
        } else if capability == "screen.capture" {
            try StrictJSON.requireExactKeys(payloadObject, expected: screenCapturePayloadKeys)
        } else if capability == "messages.decode_text" {
            try StrictJSON.requireExactKeys(payloadObject, expected: messageDecodePayloadKeys)
        } else if capability == "photos.authorization" {
            try StrictJSON.requireExactKeys(payloadObject, expected: photoAuthorizationPayloadKeys)
        } else if capability == "photos.list" {
            try StrictJSON.requireExactKeys(payloadObject, expected: photoListPayloadKeys)
        } else if capability == "photos.export" {
            try StrictJSON.requireExactKeys(payloadObject, expected: photoExportPayloadKeys)
        } else {
            throw BridgeServerError.invalidRequest
        }

        let request: StrictCapabilityEnvelope
        do {
            request = try JSONDecoder().decode(StrictCapabilityEnvelope.self, from: requestJSON)
        } catch {
            throw BridgeServerError.invalidRequest
        }
        guard (HandshakeHandler.minimumProtocolVersion...HandshakeHandler.protocolVersion)
                  .contains(request.protocolVersion),
              negotiatedProtocolVersion.map({ $0 == request.protocolVersion }) ?? true,
              request.messageKind == .request,
              request.capability == capability,
              request.deadlineMilliseconds > 0,
              request.deadlineMilliseconds <= UInt64(Int.max),
              request.deadlineMilliseconds <= BridgeWireLimits.maximumDeadlineMilliseconds,
              request.error == nil else {
            throw BridgeServerError.invalidRequest
        }
        let responsePayload: [String: JSONValue]
        if capability == "network.observe" {
            responsePayload = networkSource.capture().payload
        } else if capability == "system.lifecycle.poll" {
            guard let afterSequence = request.payload.afterSequence else {
                throw BridgeServerError.invalidRequest
            }
            let snapshot = lifecycleSource.snapshot(after: afterSequence)
            responsePayload = [
                "events": .array(snapshot.events.map(\.payload)),
                "latest_sequence": .number(Double(snapshot.latestSequence)),
            ]
        } else if capability == "screen.context" {
            responsePayload = screenSource.context().payload
        } else if capability == "screen.capture" {
            guard let excludedBundleIDs = request.payload.excludedBundleIDs,
                  excludedBundleIDs.count <= 100,
                  excludedBundleIDs.allSatisfy({ !$0.isEmpty && $0.count <= 255 }) else {
                throw BridgeServerError.invalidRequest
            }
            responsePayload = screenSource.capture(excludedBundleIDs: Set(excludedBundleIDs)).payload
        } else if capability == "messages.decode_text" {
            guard let encodedBodies = request.payload.encodedBodies,
                  encodedBodies.count <= 100,
                  encodedBodies.allSatisfy({ !$0.isEmpty && $0.count <= 6 * 1024 * 1024 }) else {
                throw BridgeServerError.invalidRequest
            }
            responsePayload = [
                "texts": .array(MessageBodyDecoder.decode(encodedBodies).map { value in
                    value.map(JSONValue.string) ?? .null
                }),
            ]
        } else if capability == "photos.authorization" {
            responsePayload = photoSource.authorizationPayload()
        } else if capability == "photos.list" {
            guard let cutoffValue = request.payload.cutoff,
                  let cutoff = PhotoLibrarySource.parseDate(cutoffValue),
                  let limit = request.payload.limit,
                  (1...PhotoLibrarySource.maximumBatchSize).contains(limit),
                  request.payload.afterCreatedAt == nil
                    || PhotoLibrarySource.parseDate(request.payload.afterCreatedAt!) != nil else {
                throw BridgeServerError.invalidRequest
            }
            responsePayload = photoSource.list(
                afterDate: request.payload.afterCreatedAt.flatMap(PhotoLibrarySource.parseDate),
                afterIdentifier: request.payload.afterLocalIdentifier,
                cutoff: cutoff,
                limit: limit
            )
        } else if capability == "photos.export" {
            guard let localIdentifier = request.payload.localIdentifier,
                  !localIdentifier.isEmpty,
                  localIdentifier.count <= 1024,
                  let fileName = request.payload.fileName else {
                throw BridgeServerError.invalidRequest
            }
            responsePayload = photoSource.export(localIdentifier: localIdentifier, fileName: fileName)
        } else {
            let permissions = capabilityProbe.permissionSnapshot().mapValues { status in
                JSONValue.string(status.rawValue)
            }
            responsePayload = [
                "screen_capture": .string(capabilityProbe.screenCaptureAvailability()),
                "permissions": .object(permissions),
            ]
        }
        let response = BridgeEnvelope(
            protocolVersion: Int(request.protocolVersion),
            requestID: request.requestID,
            messageKind: .response,
            capability: request.capability,
            deadlineMilliseconds: Int(request.deadlineMilliseconds),
            payload: responsePayload
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
    let includePermissions: Bool?
    let includeWifiIdentity: Bool?
    let afterSequence: UInt64?
    let excludedBundleIDs: [String]?
    let encodedBodies: [String]?
    let afterCreatedAt: String?
    let afterLocalIdentifier: String?
    let cutoff: String?
    let limit: Int?
    let localIdentifier: String?
    let fileName: String?

    private enum CodingKeys: String, CodingKey {
        case includePermissions = "include_permissions"
        case includeWifiIdentity = "include_wifi_identity"
        case afterSequence = "after_sequence"
        case excludedBundleIDs = "excluded_bundle_ids"
        case encodedBodies = "encoded_bodies"
        case afterCreatedAt = "after_created_at"
        case afterLocalIdentifier = "after_local_identifier"
        case cutoff
        case limit
        case localIdentifier = "local_identifier"
        case fileName = "file_name"
    }
}
