import BridgeProtocol
import Darwin
import Foundation
@testable import PlatformBridge
import XCTest

final class HandshakeTests: XCTestCase {
    private let secret = Data(repeating: 0x5a, count: 32)
    private let nonce = Data(repeating: 0x11, count: 32)

    func testProofMatchesRustTranscriptGoldenVector() throws {
        let proof = try BridgeProof.make(
            secret: secret,
            nonce: nonce,
            protocolVersion: 0x0102_0304,
            agentVersion: "v1.β"
        )

        XCTAssertEqual(proof, "ZzHI3PgX7xuVBQpbtbnGsqP8Tvcu9WBICkuw1YUGwmc=")
        XCTAssertTrue(BridgeProof.verify(
            proof,
            secret: secret,
            nonce: nonce,
            protocolVersion: 0x0102_0304,
            agentVersion: "v1.β"
        ))
    }

    func testInvalidProofIsRejectedBySharedVerifier() {
        XCTAssertFalse(BridgeProof.verify(
            Data(repeating: 0, count: 32).base64EncodedString(),
            secret: secret,
            nonce: nonce,
            protocolVersion: 0x0102_0304,
            agentVersion: "v1.β"
        ))
    }

    func testChallengeProducesStrictCorrelatedAuthenticatedResponse() throws {
        let requestID = UUID(uuidString: "018f3f4a-2d9b-7d21-a310-2c49d9b43c12")!
        let challenge = try challengeData(protocolVersion: 1, requestID: requestID)
        let handler = HandshakeHandler(
            credentialProvider: FixedCredentialProvider(secret: secret),
            bridgeVersion: "0.0.0-s1a"
        )

        let result = try handler.respond(to: challenge)
        let response = try JSONDecoder().decode(BridgeEnvelope.self, from: result.responseJSON)
        let payloadData = try JSONEncoder().encode(response.payload)
        let payload = try JSONDecoder().decode(HandshakeResponse.self, from: payloadData)

        XCTAssertTrue(result.protocolCompatible)
        XCTAssertEqual(response.protocolVersion, 1)
        XCTAssertEqual(response.requestID, requestID)
        XCTAssertEqual(response.messageKind, .response)
        XCTAssertEqual(response.capability, "bridge.handshake")
        XCTAssertEqual(response.deadlineMilliseconds, 1_000)
        XCTAssertNil(response.error)
        XCTAssertEqual(payload.phase, .response)
        XCTAssertEqual(payload.nonce, nonce.base64EncodedString())
        XCTAssertEqual(payload.bridgeVersion, "0.0.0-s1a")
        XCTAssertTrue(BridgeProof.verify(
            payload.proof,
            secret: secret,
            nonce: nonce,
            protocolVersion: 1,
            agentVersion: "v1.β"
        ))
    }

    func testUnsupportedInboundVersionReturnsAuthenticatedV1ThenRequiresClose() throws {
        let challenge = try challengeData(protocolVersion: 999, requestID: UUID())
        let handler = HandshakeHandler(
            credentialProvider: FixedCredentialProvider(secret: secret),
            bridgeVersion: "0.0.0-s1a"
        )

        let result = try handler.respond(to: challenge)
        let response = try JSONDecoder().decode(BridgeEnvelope.self, from: result.responseJSON)
        let payloadData = try JSONEncoder().encode(response.payload)
        let payload = try JSONDecoder().decode(HandshakeResponse.self, from: payloadData)

        XCTAssertFalse(result.protocolCompatible)
        XCTAssertEqual(response.protocolVersion, 1)
        XCTAssertTrue(BridgeProof.verify(
            payload.proof,
            secret: secret,
            nonce: nonce,
            protocolVersion: 1,
            agentVersion: "v1.β"
        ))
        XCTAssertFalse(BridgeProof.verify(
            payload.proof,
            secret: secret,
            nonce: nonce,
            protocolVersion: 999,
            agentVersion: "v1.β"
        ))
    }

    func testUnknownAndDuplicateHandshakeFieldsAreRejected() throws {
        let requestID = UUID().uuidString.lowercased()
        let nonce = nonce.base64EncodedString()
        let handler = HandshakeHandler(
            credentialProvider: FixedCredentialProvider(secret: secret),
            bridgeVersion: "0.0.0-s1a"
        )
        let unknown = Data("""
        {"protocol_version":1,"request_id":"\(requestID)","message_kind":"request","capability":"bridge.handshake","deadline_ms":1000,"payload":{"phase":"challenge","nonce":"\(nonce)","agent_version":"v1.β","extra":true}}
        """.utf8)
        let duplicate = Data("""
        {"protocol_version":1,"protocol_version":1,"request_id":"\(requestID)","message_kind":"request","capability":"bridge.handshake","deadline_ms":1000,"payload":{"phase":"challenge","nonce":"\(nonce)","agent_version":"v1.β"}}
        """.utf8)

        XCTAssertThrowsError(try handler.respond(to: unknown))
        XCTAssertThrowsError(try handler.respond(to: duplicate))
    }

    func testInvalidEnvelopeFieldsNonceAndCredentialAreRejected() throws {
        let valid = try challengeData(protocolVersion: 1, requestID: UUID())
        let handler = HandshakeHandler(
            credentialProvider: FixedCredentialProvider(secret: secret),
            bridgeVersion: "0.0.0-s1a"
        )
        let wrongKind = try replacing(valid, key: "message_kind", with: "event")
        let zeroDeadline = try replacing(valid, key: "deadline_ms", with: 0)
        let badNonce = try replacingPayload(valid, key: "nonce", with: "AA==")
        let emptyAgent = try replacingPayload(valid, key: "agent_version", with: "")

        for invalid in [wrongKind, zeroDeadline, badNonce, emptyAgent] {
            XCTAssertThrowsError(try handler.respond(to: invalid))
        }
        XCTAssertThrowsError(try HandshakeHandler(
            credentialProvider: FixedCredentialProvider(secret: Data(repeating: 1, count: 31)),
            bridgeVersion: "0.0.0-s1a"
        ).respond(to: valid))
    }

    func testSocketPathValidatorAllowsOnlyDirectChildAndRejectsUnsafeEntries() throws {
        let parent = URL(fileURLWithPath: "/tmp/pca-bt-\(UUID().uuidString.prefix(8))", isDirectory: true)
        try FileManager.default.createDirectory(at: parent, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: parent) }
        let runRoot = parent.appendingPathComponent("Run", isDirectory: true)
        let validator = SocketPathValidator(approvedRunRoot: runRoot)
        try validator.prepareRunDirectory()
        var runInfo = stat()
        XCTAssertEqual(lstat(runRoot.path, &runInfo), 0)
        XCTAssertEqual(runInfo.st_mode & mode_t(S_IFMT), mode_t(S_IFDIR))
        XCTAssertEqual(runInfo.st_mode & 0o777, 0o700)
        XCTAssertEqual(runInfo.st_uid, geteuid())

        let socket = runRoot.appendingPathComponent("bridge.sock")
        XCTAssertNoThrow(try validator.validate(socketURL: socket))
        XCTAssertThrowsError(try validator.validate(socketURL: runRoot.appendingPathComponent("nested/bridge.sock")))
        XCTAssertThrowsError(try validator.validate(socketURL: parent.appendingPathComponent("bridge.sock")))

        let regular = runRoot.appendingPathComponent("regular")
        XCTAssertTrue(FileManager.default.createFile(atPath: regular.path, contents: Data()))
        XCTAssertThrowsError(try validator.removeStaleSocketIfSafe(at: regular))
        XCTAssertTrue(FileManager.default.fileExists(atPath: regular.path))

        let target = runRoot.appendingPathComponent("target")
        XCTAssertTrue(FileManager.default.createFile(atPath: target.path, contents: Data()))
        let link = runRoot.appendingPathComponent("link")
        XCTAssertEqual(symlink(target.path, link.path), 0)
        XCTAssertThrowsError(try validator.removeStaleSocketIfSafe(at: link))
        XCTAssertEqual(lstatExists(link.path), 0)
    }

    func testServerBindsPrivateSocketAndRemovesItOnShutdown() async throws {
        let parent = URL(fileURLWithPath: "/tmp/pca-bs-\(UUID().uuidString.prefix(8))", isDirectory: true)
        try FileManager.default.createDirectory(at: parent, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: parent) }
        let runRoot = parent.appendingPathComponent("Run", isDirectory: true)
        let socket = runRoot.appendingPathComponent("bridge.sock")
        let validator = SocketPathValidator(approvedRunRoot: runRoot)
        let server = BridgeServer(
            socketURL: socket,
            pathValidator: validator,
            handshakeHandler: HandshakeHandler(
                credentialProvider: FixedCredentialProvider(secret: secret),
                bridgeVersion: "0.0.0-s1a"
            )
        )

        try await server.start()
        var info = stat()
        XCTAssertEqual(lstat(socket.path, &info), 0)
        XCTAssertEqual(info.st_mode & mode_t(S_IFMT), mode_t(S_IFSOCK))
        XCTAssertEqual(info.st_mode & 0o777, 0o600)
        XCTAssertEqual(info.st_uid, geteuid())

        await server.shutdown()
        XCTAssertNotEqual(lstat(socket.path, &info), 0)
    }

    private func challengeData(protocolVersion: Int, requestID: UUID) throws -> Data {
        let envelope = BridgeEnvelope(
            protocolVersion: protocolVersion,
            requestID: requestID,
            messageKind: .request,
            capability: "bridge.handshake",
            deadlineMilliseconds: 1_000,
            payload: [
                "phase": .string("challenge"),
                "nonce": .string(nonce.base64EncodedString()),
                "agent_version": .string("v1.β"),
            ]
        )
        return try JSONEncoder().encode(envelope)
    }

    private func replacing(_ data: Data, key: String, with value: Any) throws -> Data {
        var object = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        object[key] = value
        return try JSONSerialization.data(withJSONObject: object)
    }

    private func replacingPayload(_ data: Data, key: String, with value: Any) throws -> Data {
        var object = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        var payload = try XCTUnwrap(object["payload"] as? [String: Any])
        payload[key] = value
        object["payload"] = payload
        return try JSONSerialization.data(withJSONObject: object)
    }

    private func lstatExists(_ path: String) -> Int32 {
        var info = stat()
        return lstat(path, &info)
    }
}

private struct FixedCredentialProvider: BridgeCredentialProviding {
    let secret: Data?

    func loadSecret() throws -> Data? {
        secret
    }
}
