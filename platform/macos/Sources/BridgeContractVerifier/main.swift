import BridgeProtocol
import Darwin
import Foundation

private func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data("Swift Bridge contract verification failed: \(message)\n".utf8))
    exit(1)
}

let sourceFile = URL(fileURLWithPath: #filePath)
let repositoryRoot = (0..<5).reduce(sourceFile) { url, _ in
    url.deletingLastPathComponent()
}
let fixtureURL = repositoryRoot
    .appendingPathComponent("packages/contracts/fixtures/bridge-request.valid.json")
let runtimeStatusFixtureURL = repositoryRoot
    .appendingPathComponent("packages/contracts/fixtures/runtime-status.local-healthy.json")
let handshakeChallengeFixtureURL = repositoryRoot
    .appendingPathComponent("packages/contracts/fixtures/bridge-handshake.challenge.json")
let handshakeResponseFixtureURL = repositoryRoot
    .appendingPathComponent("packages/contracts/fixtures/bridge-handshake.response.json")

do {
    let fixtureData = try Data(contentsOf: fixtureURL)
    let envelope = try JSONDecoder().decode(BridgeEnvelope.self, from: fixtureData)

    guard envelope.protocolVersion == 1 else {
        fail("expected protocol_version 1")
    }
    guard envelope.deadlineMilliseconds == 1_000 else {
        fail("expected deadline_ms 1000")
    }
    guard envelope.payload["include_permissions"] == .bool(true) else {
        fail("expected payload.include_permissions true")
    }

    let encodedData = try JSONEncoder().encode(envelope)
    guard let encodedObject = try JSONSerialization.jsonObject(with: encodedData) as? [String: Any] else {
        fail("encoded envelope is not a JSON object")
    }
    guard encodedObject["protocol_version"] != nil,
          encodedObject["request_id"] != nil,
          encodedObject["message_kind"] != nil,
          encodedObject["deadline_ms"] != nil else {
        fail("encoded envelope is missing snake_case wire keys")
    }
    guard encodedObject["protocolVersion"] == nil,
          encodedObject["requestID"] == nil,
          encodedObject["messageKind"] == nil,
          encodedObject["deadlineMilliseconds"] == nil else {
        fail("encoded envelope contains Swift camelCase keys")
    }
    guard encodedObject["payload"] is [String: Any] else {
        fail("encoded payload is not a JSON object")
    }

    let runtimeStatusData = try Data(contentsOf: runtimeStatusFixtureURL)
    let runtimeStatus = try JSONDecoder().decode(RuntimeStatusEnvelope.self, from: runtimeStatusData)
    guard runtimeStatus.agentStatus == .unpaired,
          runtimeStatus.bridgeStatus == .ready,
          runtimeStatus.localHealthy,
          runtimeStatus.heartbeatAt == "2026-07-31T00:00:00Z",
          runtimeStatus.processID == 4242,
          runtimeStatus.appVersion == "0.0.0-s1a",
          runtimeStatus.schemaVersion == 2 else {
        fail("runtime status fixture does not match the canonical fields")
    }

    let challengeData = try Data(contentsOf: handshakeChallengeFixtureURL)
    let challengeEnvelope = try JSONDecoder().decode(BridgeEnvelope.self, from: challengeData)
    let challengePayload = try JSONEncoder().encode(challengeEnvelope.payload)
    let challenge = try JSONDecoder().decode(HandshakeChallenge.self, from: challengePayload)
    guard challengeEnvelope.protocolVersion == 1,
          challengeEnvelope.messageKind == .request,
          challengeEnvelope.capability == "bridge.handshake",
          challengeEnvelope.deadlineMilliseconds == 1_000,
          challengeEnvelope.error == nil,
          challenge.phase == .challenge,
          challenge.nonce == "c2VjcmV0LWZyZWUtbm9uY2UtMDE=",
          challenge.agentVersion == "0.0.0-s1a" else {
        fail("handshake challenge fixture does not match the canonical fields")
    }

    let responseData = try Data(contentsOf: handshakeResponseFixtureURL)
    let responseEnvelope = try JSONDecoder().decode(BridgeEnvelope.self, from: responseData)
    let responsePayload = try JSONEncoder().encode(responseEnvelope.payload)
    let response = try JSONDecoder().decode(HandshakeResponse.self, from: responsePayload)
    guard responseEnvelope.protocolVersion == 1,
          responseEnvelope.messageKind == .response,
          responseEnvelope.capability == "bridge.handshake",
          responseEnvelope.deadlineMilliseconds == 1_000,
          responseEnvelope.error == nil,
          response.phase == .response,
          response.nonce == challenge.nonce,
          response.proof == "c3ludGhldGljLWhtYWMtc2hhMjU2LXByb29m",
          response.bridgeVersion == "0.0.0-s1a" else {
        fail("handshake response fixture does not match the canonical fields")
    }

    let malformedChallengePayload = Data(
        "{\"phase\":\"response\",\"nonce\":\"c2VjcmV0LWZyZWUtbm9uY2UtMDE=\",\"agent_version\":\"0.0.0-s1a\"}".utf8
    )
    guard (try? JSONDecoder().decode(HandshakeChallenge.self, from: malformedChallengePayload)) == nil else {
        fail("handshake challenge accepted a mismatched phase")
    }

    let malformedResponsePayload = Data(
        "{\"phase\":\"challenge\",\"nonce\":\"c2VjcmV0LWZyZWUtbm9uY2UtMDE=\",\"proof\":\"c3ludGhldGljLWhtYWMtc2hhMjU2LXByb29m\",\"bridge_version\":\"0.0.0-s1a\"}".utf8
    )
    guard (try? JSONDecoder().decode(HandshakeResponse.self, from: malformedResponsePayload)) == nil else {
        fail("handshake response accepted a mismatched phase")
    }

    print("Swift Bridge contract fixture passed")
} catch {
    fail(error.localizedDescription)
}
