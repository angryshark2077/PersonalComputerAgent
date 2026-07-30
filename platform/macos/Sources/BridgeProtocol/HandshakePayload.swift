import Foundation

public enum AgentStatus: String, Codable, Sendable, Equatable {
    case unpaired
    case initializing
    case waitingPermission = "waiting_permission"
    case running
    case degraded
    case sleeping
    case updating
    case repair
    case stopped
}

public enum BridgeStatus: String, Codable, Sendable, Equatable {
    case disconnected
    case handshaking
    case ready
    case degraded
    case incompatible
    case stopped
}

public struct RuntimeStatusEnvelope: Codable, Sendable, Equatable {
    public let agentStatus: AgentStatus
    public let bridgeStatus: BridgeStatus
    public let localHealthy: Bool
    public let heartbeatAt: String
    public let processID: Int
    public let appVersion: String
    public let schemaVersion: Int

    private enum CodingKeys: String, CodingKey {
        case agentStatus = "agent_status"
        case bridgeStatus = "bridge_status"
        case localHealthy = "local_healthy"
        case heartbeatAt = "heartbeat_at"
        case processID = "process_id"
        case appVersion = "app_version"
        case schemaVersion = "schema_version"
    }
}

public struct HandshakeChallenge: Codable, Sendable, Equatable {
    public let phase: String
    public let nonce: String
    public let agentVersion: String

    private enum CodingKeys: String, CodingKey {
        case phase
        case nonce
        case agentVersion = "agent_version"
    }
}

public struct HandshakeResponse: Codable, Sendable, Equatable {
    public let phase: String
    public let nonce: String
    public let proof: String
    public let bridgeVersion: String

    private enum CodingKeys: String, CodingKey {
        case phase
        case nonce
        case proof
        case bridgeVersion = "bridge_version"
    }
}
