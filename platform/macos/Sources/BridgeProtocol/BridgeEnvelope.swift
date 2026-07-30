import Foundation

public enum BridgeMessageKind: String, Codable, Sendable {
    case request
    case response
    case event
}

public struct BridgeErrorEnvelope: Codable, Sendable, Equatable {
    public let errorCode: String
    public let message: String
    public let retryable: Bool
    public let requestID: UUID?
    public let details: [String: JSONValue]?

    private enum CodingKeys: String, CodingKey {
        case errorCode = "error_code"
        case message
        case retryable
        case requestID = "request_id"
        case details
    }
}

public struct BridgeEnvelope: Codable, Sendable {
    public let protocolVersion: Int
    public let requestID: UUID
    public let messageKind: BridgeMessageKind
    public let capability: String
    public let deadlineMilliseconds: Int
    public let payload: [String: JSONValue]
    public let error: BridgeErrorEnvelope?

    private enum CodingKeys: String, CodingKey {
        case protocolVersion = "protocol_version"
        case requestID = "request_id"
        case messageKind = "message_kind"
        case capability
        case deadlineMilliseconds = "deadline_ms"
        case payload
        case error
    }

    public init(
        protocolVersion: Int,
        requestID: UUID,
        messageKind: BridgeMessageKind,
        capability: String,
        deadlineMilliseconds: Int,
        payload: [String: JSONValue],
        error: BridgeErrorEnvelope? = nil
    ) {
        self.protocolVersion = protocolVersion
        self.requestID = requestID
        self.messageKind = messageKind
        self.capability = capability
        self.deadlineMilliseconds = deadlineMilliseconds
        self.payload = payload
        self.error = error
    }
}
