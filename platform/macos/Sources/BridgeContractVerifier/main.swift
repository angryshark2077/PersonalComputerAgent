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

    print("Swift Bridge contract fixture passed")
} catch {
    fail(error.localizedDescription)
}
