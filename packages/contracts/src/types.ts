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
  callback_state: string;
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
  local_media_cleanup?: { request_id: string } | null;
  screenshot_request?: { request_id: string } | null;
  collectors: {
    network: { enabled: boolean };
    "screen.capture": {
      enabled: boolean;
      scheduled_enabled: boolean;
      interval_seconds: number;
      activity_enabled: boolean;
      activity_min_interval_seconds: number;
      excluded_bundle_ids: string[];
    };
    "communication.wechat": {
      enabled: boolean;
      directions: ["incoming", "outgoing"];
      message_types: ["text", "audio", "image", "video"];
      conversation_scope: "direct_and_group_at_most_fifteen_members";
      max_group_members: 15;
      sync_mode: "full";
      retention_days: 180;
    };
    "communication.messages": {
      enabled: boolean;
      directions: ["incoming", "outgoing"];
      message_types: ["text"];
      conversation_scope: "all";
      initial_lookback_days: 7;
      sync_mode: "full";
      attachments_enabled: false;
      attachment_retention_days: 7;
    };
    "photos.library": {
      enabled: boolean;
      media_types: ["image", "video"];
      include_originals: true;
      include_album_names: true;
      initial_lookback_days: 60;
      cloud_retention: "permanent";
    };
  };
}

export type CommunicationDirection = "incoming" | "outgoing";
export type CommunicationMessageKind = "text" | "audio" | "image" | "video" | "file";

export type CommunicationConversation =
  | { scope: "direct" }
  | { scope: "group"; member_count: number };

export interface CommunicationAttachment {
  attachment_id: string;
  kind: Exclude<CommunicationMessageKind, "text">;
  sha256: string;
  size_bytes: number;
  mime_type: string;
  file_name?: string;
}

export type CommunicationMessageRecorded =
  | {
      message_id: string;
      conversation_id: string;
      source_key: string;
      occurred_at: string;
      direction: CommunicationDirection;
      kind: "text";
      conversation: CommunicationConversation;
      text: string;
    }
  | {
      message_id: string;
      conversation_id: string;
      source_key: string;
      occurred_at: string;
      direction: CommunicationDirection;
      kind: Exclude<CommunicationMessageKind, "text">;
      conversation: CommunicationConversation;
      attachments: CommunicationAttachment[];
    };

export interface DashboardDeviceStatus {
  presence: "online" | "stale" | "offline" | "sleeping";
  agent_version: string;
  outbox_depth: number;
  collector_health: DashboardCollectorHealth[];
  observed_at: string;
}

export interface DashboardCollectorHealth {
  collector_key: string;
  collector_version: string;
  status: "disabled" | "permission_required" | "initializing" | "running" | "paused" | "degraded" | "unsupported" | "error";
  desired_config_revision: number;
  applied_config_revision: number;
  last_event_at: string | null;
  last_health_at: string | null;
  error_code: string | null;
  reported_at: string;
  agent_version: string;
}

export interface DashboardDeviceDetail extends AgentControlSnapshot {
  platform: "macos";
  paired_at: string;
  status: DashboardDeviceStatus | null;
}
