import BridgeProtocol
import CryptoKit
import Foundation

protocol BridgeCredentialProviding: Sendable {
    func loadSecret() throws -> Data?
}

struct KeychainBridgeCredentialProvider: BridgeCredentialProviding {
    private let store = KeychainCredentialStore()

    func loadSecret() throws -> Data? {
        try store.load()
    }
}

enum CredentialLoader {
    static func load(
        from provider: any BridgeCredentialProviding,
        timeoutMilliseconds: UInt64
    ) async throws -> Data {
        let result: Result<Data, any Error> = await withCheckedContinuation { continuation in
            let gate = CredentialLoadGate(continuation: continuation)
            Task.detached {
                let result: Result<Data, any Error>
                do {
                    guard let secret = try provider.loadSecret() else {
                        throw BridgeHandshakeError.credentialMissing
                    }
                    guard secret.count == KeychainCredentialStore.sharedSecretLength else {
                        throw BridgeHandshakeError.credentialInvalid
                    }
                    result = .success(secret)
                } catch {
                    result = .failure(error)
                }
                await gate.finish(result)
            }
            Task.detached {
                try? await Task.sleep(for: .milliseconds(timeoutMilliseconds))
                await gate.finish(.failure(BridgeHandshakeError.timedOut))
            }
        }
        return try result.get()
    }
}

private actor CredentialLoadGate {
    private var continuation: CheckedContinuation<Result<Data, any Error>, Never>?

    init(continuation: CheckedContinuation<Result<Data, any Error>, Never>) {
        self.continuation = continuation
    }

    func finish(_ result: Result<Data, any Error>) {
        continuation?.resume(returning: result)
        continuation = nil
    }
}

enum BridgeHandshakeError: Error, Equatable, Sendable {
    case malformedJSON
    case duplicateField
    case unknownField
    case invalidEnvelope
    case invalidNonce
    case credentialMissing
    case credentialInvalid
    case proofFailure
    case timedOut
}

enum BridgeWireLimits {
    static let maximumDeadlineMilliseconds: UInt64 = 30_000
}

enum BridgeProof {
    private static let agentProofContext = Data("pca-agent-proof-v1\0".utf8)

    static func make(
        secret: Data,
        nonce: Data,
        protocolVersion: UInt32,
        agentVersion: String
    ) throws -> String {
        guard secret.count == KeychainCredentialStore.sharedSecretLength else {
            throw BridgeHandshakeError.credentialInvalid
        }
        guard nonce.count == 32 else { throw BridgeHandshakeError.invalidNonce }

        var version = protocolVersion.bigEndian
        var transcript = nonce
        transcript.append(withUnsafeBytes(of: &version) { Data($0) })
        transcript.append(Data(agentVersion.utf8))
        let code = HMAC<SHA256>.authenticationCode(
            for: transcript,
            using: SymmetricKey(data: secret)
        )
        return Data(code).base64EncodedString()
    }

    static func verify(
        _ proof: String,
        secret: Data,
        nonce: Data,
        protocolVersion: UInt32,
        agentVersion: String
    ) -> Bool {
        guard secret.count == KeychainCredentialStore.sharedSecretLength,
              nonce.count == 32,
              let decoded = Data(base64Encoded: proof),
              decoded.count == 32 else {
            return false
        }
        return HMAC<SHA256>.isValidAuthenticationCode(
            decoded,
            authenticating: transcript(
                nonce: nonce,
                protocolVersion: protocolVersion,
                agentVersion: agentVersion
            ),
            using: SymmetricKey(data: secret)
        )
    }

    static func makeAgentProof(
        secret: Data,
        nonce: Data,
        protocolVersion: UInt32,
        agentVersion: String
    ) throws -> String {
        guard secret.count == KeychainCredentialStore.sharedSecretLength else {
            throw BridgeHandshakeError.credentialInvalid
        }
        guard nonce.count == 32 else { throw BridgeHandshakeError.invalidNonce }
        let code = HMAC<SHA256>.authenticationCode(
            for: agentProofContext + transcript(
                nonce: nonce,
                protocolVersion: protocolVersion,
                agentVersion: agentVersion
            ),
            using: SymmetricKey(data: secret)
        )
        return Data(code).base64EncodedString()
    }

    static func verifyAgentProof(
        _ proof: String,
        secret: Data,
        nonce: Data,
        protocolVersion: UInt32,
        agentVersion: String
    ) -> Bool {
        guard secret.count == KeychainCredentialStore.sharedSecretLength,
              nonce.count == 32,
              let decoded = Data(base64Encoded: proof),
              decoded.count == 32 else {
            return false
        }
        return HMAC<SHA256>.isValidAuthenticationCode(
            decoded,
            authenticating: agentProofContext + transcript(
                nonce: nonce,
                protocolVersion: protocolVersion,
                agentVersion: agentVersion
            ),
            using: SymmetricKey(data: secret)
        )
    }

    private static func transcript(nonce: Data, protocolVersion: UInt32, agentVersion: String) -> Data {
        var version = protocolVersion.bigEndian
        var value = nonce
        value.append(withUnsafeBytes(of: &version) { Data($0) })
        value.append(Data(agentVersion.utf8))
        return value
    }
}

struct HandshakeResult: Sendable {
    let responseJSON: Data
    let protocolCompatible: Bool
    let deadlineMilliseconds: UInt64
}

struct ValidatedHandshakeChallenge: Sendable {
    let protocolVersion: UInt32
    let requestID: UUID
    let capability: String
    let deadlineMilliseconds: UInt64
    let nonce: Data
    let encodedNonce: String
    let agentVersion: String
    let clientProof: String
}

struct HandshakeHandler: Sendable {
    static let protocolVersion: UInt32 = 2
    static let minimumProtocolVersion: UInt32 = 1
    private static let envelopeKeys: Set<String> = [
        "protocol_version", "request_id", "message_kind", "capability", "deadline_ms", "payload", "error",
    ]
    private static let challengeKeys: Set<String> = ["phase", "nonce", "agent_version", "client_proof"]

    let bridgeVersion: String

    func validate(_ challengeJSON: Data) throws -> ValidatedHandshakeChallenge {
        guard !bridgeVersion.isEmpty else { throw BridgeHandshakeError.invalidEnvelope }
        let object = try StrictJSON.object(challengeJSON)
        try StrictJSON.requireOnlyKeys(object, allowed: Self.envelopeKeys)
        guard let payloadObject = object["payload"] as? [String: Any] else {
            throw BridgeHandshakeError.invalidEnvelope
        }
        try StrictJSON.requireExactKeys(payloadObject, expected: Self.challengeKeys)

        let decoder = JSONDecoder()
        let challenge: StrictHandshakeEnvelope
        do {
            challenge = try decoder.decode(StrictHandshakeEnvelope.self, from: challengeJSON)
        } catch {
            throw BridgeHandshakeError.invalidEnvelope
        }
        guard challenge.messageKind == .request,
              challenge.capability == "bridge.handshake",
              challenge.deadlineMilliseconds > 0,
              challenge.deadlineMilliseconds <= UInt64(Int.max),
              challenge.deadlineMilliseconds <= BridgeWireLimits.maximumDeadlineMilliseconds,
              challenge.error == nil,
              challenge.payload.phase == .challenge,
              !challenge.payload.agentVersion.isEmpty else {
            throw BridgeHandshakeError.invalidEnvelope
        }
        guard let nonce = Data(base64Encoded: challenge.payload.nonce),
              nonce.count == 32,
              nonce.base64EncodedString() == challenge.payload.nonce else {
            throw BridgeHandshakeError.invalidNonce
        }
        return ValidatedHandshakeChallenge(
            protocolVersion: challenge.protocolVersion,
            requestID: challenge.requestID,
            capability: challenge.capability,
            deadlineMilliseconds: challenge.deadlineMilliseconds,
            nonce: nonce,
            encodedNonce: challenge.payload.nonce,
            agentVersion: challenge.payload.agentVersion,
            clientProof: challenge.payload.clientProof
        )
    }

    func respond(to challengeJSON: Data, secret: Data) throws -> HandshakeResult {
        try respond(to: validate(challengeJSON), secret: secret)
    }

    func respond(to challenge: ValidatedHandshakeChallenge, secret: Data) throws -> HandshakeResult {
        guard secret.count == KeychainCredentialStore.sharedSecretLength else {
            throw BridgeHandshakeError.credentialInvalid
        }
        guard BridgeProof.verifyAgentProof(
            challenge.clientProof,
            secret: secret,
            nonce: challenge.nonce,
            protocolVersion: challenge.protocolVersion,
            agentVersion: challenge.agentVersion
        ) else {
            throw BridgeHandshakeError.proofFailure
        }

        let protocolCompatible = (Self.minimumProtocolVersion...Self.protocolVersion)
            .contains(challenge.protocolVersion)
        let responseVersion = protocolCompatible
            ? challenge.protocolVersion
            : Self.protocolVersion
        let proof = try BridgeProof.make(
            secret: secret,
            nonce: challenge.nonce,
            protocolVersion: responseVersion,
            agentVersion: challenge.agentVersion
        )
        let response = BridgeEnvelope(
            protocolVersion: Int(responseVersion),
            requestID: challenge.requestID,
            messageKind: .response,
            capability: challenge.capability,
            deadlineMilliseconds: Int(challenge.deadlineMilliseconds),
            payload: [
                "phase": .string("response"),
                "nonce": .string(challenge.encodedNonce),
                "proof": .string(proof),
                "bridge_version": .string(bridgeVersion),
            ]
        )
        return HandshakeResult(
            responseJSON: try JSONEncoder().encode(response),
            protocolCompatible: protocolCompatible,
            deadlineMilliseconds: challenge.deadlineMilliseconds
        )
    }
}

private struct StrictHandshakeEnvelope: Decodable {
    let protocolVersion: UInt32
    let requestID: UUID
    let messageKind: BridgeMessageKind
    let capability: String
    let deadlineMilliseconds: UInt64
    let payload: StrictHandshakeChallenge
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

private struct StrictHandshakeChallenge: Decodable {
    let phase: HandshakeChallengePhase
    let nonce: String
    let agentVersion: String
    let clientProof: String

    private enum CodingKeys: String, CodingKey {
        case phase
        case nonce
        case agentVersion = "agent_version"
        case clientProof = "client_proof"
    }
}

enum StrictJSON {
    static let maximumNestingDepth = 64

    static func object(_ data: Data) throws -> [String: Any] {
        do {
            var scanner = DuplicateKeyScanner(
                bytes: Array(data),
                maximumNestingDepth: maximumNestingDepth
            )
            try scanner.validate()
            guard let object = try JSONSerialization.jsonObject(with: data) as? [String: Any] else {
                throw BridgeHandshakeError.malformedJSON
            }
            return object
        } catch let error as BridgeHandshakeError {
            throw error
        } catch {
            throw BridgeHandshakeError.malformedJSON
        }
    }

    static func requireOnlyKeys(_ object: [String: Any], allowed: Set<String>) throws {
        guard Set(object.keys).isSubset(of: allowed) else { throw BridgeHandshakeError.unknownField }
    }

    static func requireExactKeys(_ object: [String: Any], expected: Set<String>) throws {
        guard Set(object.keys) == expected else {
            if Set(object.keys).isSubset(of: expected) {
                throw BridgeHandshakeError.invalidEnvelope
            }
            throw BridgeHandshakeError.unknownField
        }
    }
}

private struct DuplicateKeyScanner {
    let bytes: [UInt8]
    let maximumNestingDepth: Int
    var index = 0

    mutating func validate() throws {
        skipWhitespace()
        try parseValue(depth: 0)
        skipWhitespace()
        guard index == bytes.count else { throw BridgeHandshakeError.malformedJSON }
    }

    private mutating func parseValue(depth: Int) throws {
        skipWhitespace()
        guard index < bytes.count else { throw BridgeHandshakeError.malformedJSON }
        switch bytes[index] {
        case 0x7b:
            guard depth < maximumNestingDepth else { throw BridgeHandshakeError.malformedJSON }
            try parseObject(depth: depth + 1)
        case 0x5b:
            guard depth < maximumNestingDepth else { throw BridgeHandshakeError.malformedJSON }
            try parseArray(depth: depth + 1)
        case 0x22: _ = try parseString()
        default: try parsePrimitive()
        }
    }

    private mutating func parseObject(depth: Int) throws {
        index += 1
        skipWhitespace()
        if consume(0x7d) { return }
        var keys = Set<String>()
        while true {
            let key = try parseString()
            guard keys.insert(key).inserted else { throw BridgeHandshakeError.duplicateField }
            skipWhitespace()
            guard consume(0x3a) else { throw BridgeHandshakeError.malformedJSON }
            try parseValue(depth: depth)
            skipWhitespace()
            if consume(0x7d) { return }
            guard consume(0x2c) else { throw BridgeHandshakeError.malformedJSON }
            skipWhitespace()
        }
    }

    private mutating func parseArray(depth: Int) throws {
        index += 1
        skipWhitespace()
        if consume(0x5d) { return }
        while true {
            try parseValue(depth: depth)
            skipWhitespace()
            if consume(0x5d) { return }
            guard consume(0x2c) else { throw BridgeHandshakeError.malformedJSON }
        }
    }

    private mutating func parseString() throws -> String {
        guard consume(0x22) else { throw BridgeHandshakeError.malformedJSON }
        let start = index - 1
        var escaped = false
        while index < bytes.count {
            let byte = bytes[index]
            index += 1
            if escaped {
                escaped = false
            } else if byte == 0x5c {
                escaped = true
            } else if byte == 0x22 {
                let encoded = Data(bytes[start..<index])
                guard let value = try? JSONDecoder().decode(String.self, from: encoded) else {
                    throw BridgeHandshakeError.malformedJSON
                }
                return value
            } else if byte < 0x20 {
                throw BridgeHandshakeError.malformedJSON
            }
        }
        throw BridgeHandshakeError.malformedJSON
    }

    private mutating func parsePrimitive() throws {
        let start = index
        while index < bytes.count, ![0x2c, 0x5d, 0x7d, 0x20, 0x09, 0x0a, 0x0d].contains(bytes[index]) {
            index += 1
        }
        guard index > start else { throw BridgeHandshakeError.malformedJSON }
    }

    private mutating func skipWhitespace() {
        while index < bytes.count, [0x20, 0x09, 0x0a, 0x0d].contains(bytes[index]) { index += 1 }
    }

    private mutating func consume(_ byte: UInt8) -> Bool {
        guard index < bytes.count, bytes[index] == byte else { return false }
        index += 1
        return true
    }
}
