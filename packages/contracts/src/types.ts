export type JsonValue =
  | null
  | boolean
  | number
  | string
  | JsonValue[]
  | { [key: string]: JsonValue };

export interface ErrorEnvelope {
  error_code: string;
  message: string;
  retryable: boolean;
  request_id?: string | null;
  details?: Record<string, JsonValue>;
}

export interface BridgeEnvelope {
  protocol_version: number;
  request_id: string;
  message_kind: "request" | "response" | "event";
  capability: string;
  deadline_ms: number;
  payload: Record<string, JsonValue>;
  error?: ErrorEnvelope | null;
}

export type AgentStatus =
  | "unpaired"
  | "initializing"
  | "waiting_permission"
  | "running"
  | "degraded"
  | "sleeping"
  | "updating"
  | "repair"
  | "stopped";

export type BridgeStatus =
  | "disconnected"
  | "handshaking"
  | "ready"
  | "degraded"
  | "incompatible"
  | "stopped";

export interface RuntimeStatusEnvelope {
  agent_status: AgentStatus;
  bridge_status: BridgeStatus;
  local_healthy: boolean;
  heartbeat_at: string;
  process_id: number;
  app_version: string;
  schema_version: number;
}

export interface HandshakeChallenge {
  phase: "challenge";
  nonce: string;
  agent_version: string;
}

export interface HandshakeResponse {
  phase: "response";
  nonce: string;
  proof: string;
  bridge_version: string;
}

export type Sensitivity = "public" | "normal" | "medium" | "high" | "secret";

export interface EventEnvelope {
  event_id: string;
  workspace_id: string;
  device_id: string;
  event_type: string;
  source: string;
  schema_version: number;
  occurred_at: string;
  created_at: string;
  sensitivity: Sensitivity;
  payload: Record<string, JsonValue>;
  attachment_refs?: string[];
  idempotency_key?: string;
}

export interface CpuMemoryMetricPayload {
  metric_group: "cpu_memory";
  sample_window_ms: number;
  logical_cpu_count: number;
  host: {
    cpu_usage_percent: number;
    memory_total_bytes: number;
    memory_used_bytes: number;
  };
  agent: {
    cpu_usage_percent: number;
    memory_resident_bytes: number;
  };
}

export interface DiskMetricPayload {
  metric_group: "disk";
  scope: "pca_data_volume";
  total_bytes: number;
  available_bytes: number;
  used_percent: number;
  low_space: boolean;
  low_space_threshold_bytes: 2147483648;
  warning_code: "DISK_SPACE_LOW" | null;
}

export type SystemMetricPayload = CpuMemoryMetricPayload | DiskMetricPayload;

export type CollectorStatus =
  | "disabled"
  | "permission_required"
  | "initializing"
  | "running"
  | "paused"
  | "degraded"
  | "unsupported"
  | "error";

export interface CollectorStatusChangedPayload {
  collector_key: string;
  previous_status: CollectorStatus;
  status: CollectorStatus;
  desired_config_revision: number;
  applied_config_revision: number;
  reason: string;
  error_code: string | null;
}

export interface SystemHealthChangedPayload {
  condition: "disk_space_low";
  active: boolean;
  error_code: "DISK_SPACE_LOW";
  available_bytes: number;
  threshold_bytes: 2147483648;
}

export interface DevicePairingStart {
  device_public_key: string;
  code_challenge: string;
  callback_uri: string;
}

export interface DevicePairingExchange {
  session_id: string;
  authorization_code: string;
  code_verifier: string;
}

export interface AgentControlSnapshot {
  device_id: string;
  workspace_id: string;
  revoked: boolean;
  configuration_revision: number;
  collectors: {
    network: { enabled: boolean };
    "communication.wechat": {
      enabled: boolean;
      direction: "outgoing";
      message_type: "text";
      sync_mode: "full";
    };
  };
}
