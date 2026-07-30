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

enum BridgeHandshakeError: Error, Equatable, Sendable {
    case malformedJSON
    case duplicateField
    case unknownField
    case invalidEnvelope
    case invalidNonce
    case credentialMissing
    case credentialInvalid
    case proofFailure
}

enum BridgeProof {
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

struct HandshakeHandler: Sendable {
    static let protocolVersion: UInt32 = 1
    private static let envelopeKeys: Set<String> = [
        "protocol_version", "request_id", "message_kind", "capability", "deadline_ms", "payload", "error",
    ]
    private static let challengeKeys: Set<String> = ["phase", "nonce", "agent_version"]

    let credentialProvider: any BridgeCredentialProviding
    let bridgeVersion: String

    func respond(to challengeJSON: Data) throws -> HandshakeResult {
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
        guard let secret = try credentialProvider.loadSecret() else {
            throw BridgeHandshakeError.credentialMissing
        }
        guard secret.count == KeychainCredentialStore.sharedSecretLength else {
            throw BridgeHandshakeError.credentialInvalid
        }

        let responseVersion = Self.protocolVersion
        let proof = try BridgeProof.make(
            secret: secret,
            nonce: nonce,
            protocolVersion: responseVersion,
            agentVersion: challenge.payload.agentVersion
        )
        let response = BridgeEnvelope(
            protocolVersion: Int(responseVersion),
            requestID: challenge.requestID,
            messageKind: .response,
            capability: challenge.capability,
            deadlineMilliseconds: challenge.deadlineMilliseconds,
            payload: [
                "phase": .string("response"),
                "nonce": .string(challenge.payload.nonce),
                "proof": .string(proof),
                "bridge_version": .string(bridgeVersion),
            ]
        )
        return HandshakeResult(
            responseJSON: try JSONEncoder().encode(response),
            protocolCompatible: challenge.protocolVersion == Self.protocolVersion,
            deadlineMilliseconds: UInt64(challenge.deadlineMilliseconds)
        )
    }
}

private struct StrictHandshakeEnvelope: Decodable {
    let protocolVersion: UInt32
    let requestID: UUID
    let messageKind: BridgeMessageKind
    let capability: String
    let deadlineMilliseconds: Int
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

    private enum CodingKeys: String, CodingKey {
        case phase
        case nonce
        case agentVersion = "agent_version"
    }
}

enum StrictJSON {
    static func object(_ data: Data) throws -> [String: Any] {
        do {
            var scanner = DuplicateKeyScanner(bytes: Array(data))
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
    var index = 0

    mutating func validate() throws {
        skipWhitespace()
        try parseValue()
        skipWhitespace()
        guard index == bytes.count else { throw BridgeHandshakeError.malformedJSON }
    }

    private mutating func parseValue() throws {
        skipWhitespace()
        guard index < bytes.count else { throw BridgeHandshakeError.malformedJSON }
        switch bytes[index] {
        case 0x7b: try parseObject()
        case 0x5b: try parseArray()
        case 0x22: _ = try parseString()
        default: try parsePrimitive()
        }
    }

    private mutating func parseObject() throws {
        index += 1
        skipWhitespace()
        if consume(0x7d) { return }
        var keys = Set<String>()
        while true {
            let key = try parseString()
            guard keys.insert(key).inserted else { throw BridgeHandshakeError.duplicateField }
            skipWhitespace()
            guard consume(0x3a) else { throw BridgeHandshakeError.malformedJSON }
            try parseValue()
            skipWhitespace()
            if consume(0x7d) { return }
            guard consume(0x2c) else { throw BridgeHandshakeError.malformedJSON }
            skipWhitespace()
        }
    }

    private mutating func parseArray() throws {
        index += 1
        skipWhitespace()
        if consume(0x5d) { return }
        while true {
            try parseValue()
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
