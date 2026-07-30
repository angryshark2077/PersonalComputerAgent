import Foundation

public enum BridgeMessageKind: String, Codable, Sendable {
    case request
    case response
    case event
}

public struct BridgeEnvelope: Codable, Sendable {
    public let protocolVersion: Int
    public let requestID: UUID
    public let messageKind: BridgeMessageKind
    public let capability: String
    public let deadlineMilliseconds: Int
    public let payload: Data

    public init(
        protocolVersion: Int,
        requestID: UUID,
        messageKind: BridgeMessageKind,
        capability: String,
        deadlineMilliseconds: Int,
        payload: Data
    ) {
        self.protocolVersion = protocolVersion
        self.requestID = requestID
        self.messageKind = messageKind
        self.capability = capability
        self.deadlineMilliseconds = deadlineMilliseconds
        self.payload = payload
    }
}
