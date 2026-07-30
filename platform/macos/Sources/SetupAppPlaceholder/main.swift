import BridgeProtocol
import Foundation

let envelope = BridgeEnvelope(
    protocolVersion: 1,
    requestID: UUID(),
    messageKind: .event,
    capability: "scaffold.ready",
    deadlineMilliseconds: 1_000,
    payload: Data()
)
print("SetupApp placeholder: \(envelope.capability)")
