import BridgeProtocol
import CryptoKit
import Darwin
import Foundation
import Security

struct PairingIPCConfiguration: Sendable {
    static let productionCloudAPIOrigin = URL(string: "https://pca-cloud-api-production.up.railway.app/")!

    let socketURL: URL
    let cloudAPIOrigin: URL

    static func production(rootURL: URL) throws -> PairingIPCConfiguration {
        let root = rootURL.standardizedFileURL
        guard root.isFileURL, root.path.hasPrefix("/") else { throw PairingError.unavailable }
        return PairingIPCConfiguration(
            socketURL: root.appendingPathComponent("Run/pairing.sock"),
            cloudAPIOrigin: productionCloudAPIOrigin
        )
    }
}

@MainActor
final class InstalledPairingAgentBridge: PairingAgentHandingOff {
    private let configuration: PairingIPCConfiguration
    private let credentialStore: KeychainCredentialStore

    init(
        configuration: PairingIPCConfiguration,
        credentialStore: KeychainCredentialStore = KeychainCredentialStore()
    ) {
        self.configuration = configuration
        self.credentialStore = credentialStore
    }

    func isPaired() async throws -> Bool {
        let response: PairingStatusResponse = try await request(operation: .status, payload: Optional<EmptyPayload>.none)
        return response.paired
    }

    func beginPairing(_ handoff: PairingStartHandoff) async throws -> PairingSessionHandoff {
        let response: PairingBeginResponse = try await request(
            operation: .begin,
            payload: PairingBeginPayload(
                callbackURI: handoff.callbackURI.absoluteString,
                cloudAPIOrigin: configuration.cloudAPIOrigin.absoluteString
            )
        )
        guard let authorizationURL = URL(string: response.authorizationURL), authorizationURL.scheme == "https" else {
            throw PairingError.agentRejected
        }
        return PairingSessionHandoff(
            sessionID: response.sessionID,
            authorizationURL: authorizationURL,
            callbackState: response.callbackState
        )
    }

    func completePairing(_ handoff: PairingCallbackHandoff) async throws -> PairingResult {
        let response: PairingCompleteResponse = try await request(
            operation: .complete,
            payload: PairingCompletePayload(
                sessionID: handoff.sessionID,
                authorizationCode: handoff.authorizationCode
            )
        )
        return .paired(deviceID: response.deviceID, workspaceID: response.workspaceID)
    }

    func cancelPairing(sessionID: String) async {
        let _: PairingCancelResponse? = try? await request(
            operation: .cancel,
            payload: PairingCancelPayload(sessionID: sessionID)
        )
    }

    private func request<Payload: Encodable & Sendable, Response: Decodable>(
        operation: PairingIPCOperation,
        payload: Payload?
    ) async throws -> Response {
        guard let secret = try credentialStore.load() else { throw PairingError.unavailable }
        let request = try PairingIPCRequest.make(operation: operation, payload: payload, secret: secret)
        let responseData = try await PairingIPCTransport.exchange(
            socketURL: configuration.socketURL,
            request: request
        )
        if (try? JSONDecoder().decode(PairingErrorResponse.self, from: responseData)) != nil {
            throw PairingError.agentRejected
        }
        guard let response = try? JSONDecoder().decode(Response.self, from: responseData) else {
            throw PairingError.agentRejected
        }
        return response
    }
}

enum PairingIPCOperation: String, Encodable, Sendable {
    case status
    case begin
    case complete
    case cancel
}

struct PairingIPCRequest<Payload: Encodable>: Encodable {
    let protocolVersion: UInt32
    let requestID: String
    let operation: PairingIPCOperation
    let nonce: String
    let proof: String
    let payload: Payload?

    enum CodingKeys: String, CodingKey {
        case protocolVersion = "protocol_version"
        case requestID = "request_id"
        case operation
        case nonce
        case proof
        case payload
    }

    static func make(
        operation: PairingIPCOperation,
        payload: Payload?,
        secret: Data,
        nonce: Data? = nil,
        requestID: UUID = UUID()
    ) throws -> Data {
        guard secret.count == KeychainCredentialStore.sharedSecretLength else { throw PairingError.unavailable }
        let nonce = try nonce ?? randomNonce()
        guard nonce.count == KeychainCredentialStore.sharedSecretLength else { throw PairingError.unavailable }
        let requestID = requestID.uuidString.lowercased()
        let context = "pca-setup-pairing-v1:\(requestID):\(operation.rawValue)"
        let proof = PairingIPCAuthentication.proof(secret: secret, nonce: nonce, context: context)
        return try JSONEncoder().encode(PairingIPCRequest(
            protocolVersion: 1,
            requestID: requestID,
            operation: operation,
            nonce: base64URLEncoded(nonce),
            proof: proof,
            payload: payload
        ))
    }

}

enum PairingIPCAuthentication {
    static func proof(secret: Data, nonce: Data, context: String) -> String {
        var transcript = nonce
        var protocolVersion = UInt32(1).bigEndian
        transcript.append(Data(bytes: &protocolVersion, count: MemoryLayout<UInt32>.size))
        transcript.append(contentsOf: context.utf8)
        let key = SymmetricKey(data: secret)
        return Data(HMAC<SHA256>.authenticationCode(for: transcript, using: key)).base64EncodedString()
    }
}

private extension PairingIPCRequest {
    private static func randomNonce() throws -> Data {
        var nonce = Data(count: KeychainCredentialStore.sharedSecretLength)
        let status = nonce.withUnsafeMutableBytes { buffer -> OSStatus in
            guard let baseAddress = buffer.baseAddress else { return errSecParam }
            return SecRandomCopyBytes(kSecRandomDefault, buffer.count, baseAddress)
        }
        guard status == errSecSuccess else { throw PairingError.unavailable }
        return nonce
    }

    private static func base64URLEncoded(_ data: Data) -> String {
        data.base64EncodedString()
            .replacingOccurrences(of: "+", with: "-")
            .replacingOccurrences(of: "/", with: "_")
            .replacingOccurrences(of: "=", with: "")
    }
}

private struct EmptyPayload: Encodable, Sendable {}

private struct PairingBeginPayload: Encodable, Sendable {
    let callbackURI: String
    let cloudAPIOrigin: String

    enum CodingKeys: String, CodingKey {
        case callbackURI = "callback_uri"
        case cloudAPIOrigin = "cloud_api_origin"
    }
}

private struct PairingCompletePayload: Encodable, Sendable {
    let sessionID: String
    let authorizationCode: String

    enum CodingKeys: String, CodingKey {
        case sessionID = "session_id"
        case authorizationCode = "authorization_code"
    }
}

private struct PairingCancelPayload: Encodable, Sendable {
    let sessionID: String

    enum CodingKeys: String, CodingKey {
        case sessionID = "session_id"
    }
}

private struct PairingStatusResponse: Decodable {
    let paired: Bool
}

private struct PairingBeginResponse: Decodable {
    let sessionID: String
    let authorizationURL: String
    let callbackState: String

    enum CodingKeys: String, CodingKey {
        case sessionID = "session_id"
        case authorizationURL = "authorization_url"
        case callbackState = "callback_state"
    }
}

private struct PairingCompleteResponse: Decodable {
    let deviceID: String
    let workspaceID: String

    enum CodingKeys: String, CodingKey {
        case deviceID = "device_id"
        case workspaceID = "workspace_id"
    }
}

private struct PairingCancelResponse: Decodable {
    let cancelled: Bool
}

private struct PairingErrorResponse: Decodable {
    let error: PairingErrorBody
}

private struct PairingErrorBody: Decodable {
    let code: String
}

private enum PairingIPCTransport {
    private static let maximumFrameLength = 1_048_576

    static func exchange(socketURL: URL, request: Data) async throws -> Data {
        try await Task.detached(priority: .userInitiated) {
            try exchangeBlocking(socketURL: socketURL, request: request)
        }.value
    }

    private static func exchangeBlocking(socketURL: URL, request: Data) throws -> Data {
        guard socketURL.isFileURL, socketURL.path.hasPrefix("/") else { throw PairingError.unavailable }
        guard request.count <= maximumFrameLength else { throw PairingError.unavailable }
        let descriptor = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard descriptor >= 0 else { throw PairingError.unavailable }
        defer { _ = Darwin.close(descriptor) }

        var timeout = timeval(tv_sec: 15, tv_usec: 0)
        guard setsockopt(descriptor, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size)) == 0,
              setsockopt(descriptor, SOL_SOCKET, SO_SNDTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size)) == 0
        else { throw PairingError.unavailable }

        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let pathBytes = Array(socketURL.path.utf8) + [0]
        guard pathBytes.count <= MemoryLayout.size(ofValue: address.sun_path) else { throw PairingError.unavailable }
        withUnsafeMutableBytes(of: &address.sun_path) { destination in
            destination.copyBytes(from: pathBytes)
        }
        let pathOffset = MemoryLayout<sockaddr_un>.size - MemoryLayout.size(ofValue: address.sun_path)
        let addressLength = socklen_t(pathOffset + pathBytes.count)
        address.sun_len = UInt8(addressLength)
        let connectStatus = withUnsafePointer(to: &address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(descriptor, $0, addressLength)
            }
        }
        guard connectStatus == 0 else { throw PairingError.unavailable }

        var length = UInt32(request.count).bigEndian
        var frame = Data(bytes: &length, count: MemoryLayout<UInt32>.size)
        frame.append(request)
        try writeAll(frame, to: descriptor)

        let responseLengthData = try readExactly(MemoryLayout<UInt32>.size, from: descriptor)
        let responseLength = responseLengthData.withUnsafeBytes { buffer in
            buffer.load(as: UInt32.self).bigEndian
        }
        guard responseLength > 0, responseLength <= maximumFrameLength else { throw PairingError.unavailable }
        return try readExactly(Int(responseLength), from: descriptor)
    }

    private static func writeAll(_ data: Data, to descriptor: Int32) throws {
        var offset = 0
        while offset < data.count {
            let written = data.withUnsafeBytes { buffer in
                Darwin.write(descriptor, buffer.baseAddress!.advanced(by: offset), data.count - offset)
            }
            guard written > 0 else { throw PairingError.unavailable }
            offset += written
        }
    }

    private static func readExactly(_ byteCount: Int, from descriptor: Int32) throws -> Data {
        var data = Data(count: byteCount)
        var offset = 0
        while offset < byteCount {
            let readCount = data.withUnsafeMutableBytes { buffer in
                Darwin.read(descriptor, buffer.baseAddress!.advanced(by: offset), byteCount - offset)
            }
            guard readCount > 0 else { throw PairingError.unavailable }
            offset += readCount
        }
        return data
    }
}
