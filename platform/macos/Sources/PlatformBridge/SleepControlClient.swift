import CryptoKit
import Darwin
import Foundation
import Security
import BridgeProtocol

/// Sends the sole authenticated Bridge-to-Agent command that is safe to wait for during sleep.
final class SleepControlClient: @unchecked Sendable {
    private static let protocolVersion: UInt32 = 1
    private static let timeoutSeconds: Int = 25

    private let socketURL: URL
    private let credentialProvider: any BridgeCredentialProviding

    init(socketURL: URL, credentialProvider: any BridgeCredentialProviding) {
        self.socketURL = socketURL
        self.credentialProvider = credentialProvider
    }

    func prepareSleep() -> Bool {
        guard var secret = try? credentialProvider.loadSecret(),
              secret.count == KeychainCredentialStore.sharedSecretLength,
              let nonce = randomNonce() else {
            return false
        }
        defer { secret.resetBytes(in: 0..<secret.count) }

        let requestID = UUID()
        let operation = "prepare_sleep"
        let context = "pca-bridge-sleep-v1:\(requestID.uuidString.lowercased()):\(operation)"
        guard let proof = try? BridgeProof.make(
            secret: secret,
            nonce: nonce,
            protocolVersion: Self.protocolVersion,
            agentVersion: context
        ) else {
            return false
        }
        let request: [String: Any] = [
            "protocol_version": Self.protocolVersion,
            "request_id": requestID.uuidString.lowercased(),
            "operation": operation,
            "nonce": nonce.base64URLEncodedString(),
            "proof": proof,
        ]
        guard JSONSerialization.isValidJSONObject(request),
              let payload = try? JSONSerialization.data(withJSONObject: request),
              let descriptor = connect() else {
            return false
        }
        defer { Darwin.close(descriptor) }
        guard let frame = try? FrameCodec.encode(payload),
              writeAll(frame, to: descriptor),
              let response = readFrame(from: descriptor),
              let object = try? JSONSerialization.jsonObject(with: response) as? [String: Any],
              object.count == 1,
              object["ok"] as? Bool == true else {
            return false
        }
        return true
    }

    private func connect() -> Int32? {
        let validator = SocketPathValidator()
        guard (try? validator.validate(socketURL: socketURL)) != nil else { return nil }
        let descriptor = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard descriptor >= 0 else { return nil }
        guard configureTimeout(descriptor) else {
            Darwin.close(descriptor)
            return nil
        }
        guard var address = try? unixAddress(for: socketURL.path) else {
            Darwin.close(descriptor)
            return nil
        }
        let length = socklen_t(MemoryLayout.offset(of: \sockaddr_un.sun_path)! + socketURL.path.utf8.count + 1)
        address.sun_len = UInt8(length)
        let connected = withUnsafePointer(to: &address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(descriptor, $0, length)
            }
        }
        guard connected == 0 else {
            Darwin.close(descriptor)
            return nil
        }
        return descriptor
    }

    private func configureTimeout(_ descriptor: Int32) -> Bool {
        var timeout = timeval(tv_sec: Self.timeoutSeconds, tv_usec: 0)
        return withUnsafePointer(to: &timeout) {
            setsockopt(descriptor, SOL_SOCKET, SO_SNDTIMEO, $0, socklen_t(MemoryLayout<timeval>.size)) == 0
                && setsockopt(descriptor, SOL_SOCKET, SO_RCVTIMEO, $0, socklen_t(MemoryLayout<timeval>.size)) == 0
        }
    }

    private func writeAll(_ data: Data, to descriptor: Int32) -> Bool {
        var offset = 0
        while offset < data.count {
            let count = data.withUnsafeBytes { rawBuffer -> Int in
                guard let base = rawBuffer.baseAddress else { return -1 }
                return Darwin.send(descriptor, base.advanced(by: offset), data.count - offset, 0)
            }
            guard count > 0 else { return false }
            offset += count
        }
        return true
    }

    private func readFrame(from descriptor: Int32) -> Data? {
        guard let header = readExactly(4, from: descriptor) else { return nil }
        let length = header.reduce(UInt32(0)) { ($0 << 8) | UInt32($1) }
        guard length > 0, length <= UInt32(FrameCodec.maximumFrameBytes) else { return nil }
        return readExactly(Int(length), from: descriptor)
    }

    private func readExactly(_ length: Int, from descriptor: Int32) -> Data? {
        var output = Data()
        output.reserveCapacity(length)
        while output.count < length {
            var bytes = [UInt8](repeating: 0, count: min(16_384, length - output.count))
            let count = Darwin.recv(descriptor, &bytes, bytes.count, 0)
            guard count > 0 else { return nil }
            output.append(contentsOf: bytes.prefix(Int(count)))
        }
        return output
    }

    private func randomNonce() -> Data? {
        var bytes = [UInt8](repeating: 0, count: 32)
        guard SecRandomCopyBytes(kSecRandomDefault, bytes.count, &bytes) == errSecSuccess else { return nil }
        return Data(bytes)
    }
}

private func unixAddress(for path: String) throws -> sockaddr_un {
    var address = sockaddr_un()
    address.sun_family = sa_family_t(AF_UNIX)
    let bytes = Array(path.utf8) + [0]
    guard bytes.count <= MemoryLayout.size(ofValue: address.sun_path) else {
        throw BridgeServerError.socketPathTooLong
    }
    withUnsafeMutableBytes(of: &address.sun_path) { destination in
        destination.copyBytes(from: bytes)
    }
    return address
}

private extension Data {
    func base64URLEncodedString() -> String {
        base64EncodedString()
            .replacingOccurrences(of: "+", with: "-")
            .replacingOccurrences(of: "/", with: "_")
            .replacingOccurrences(of: "=", with: "")
    }
}
