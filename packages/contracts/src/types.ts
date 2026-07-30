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
