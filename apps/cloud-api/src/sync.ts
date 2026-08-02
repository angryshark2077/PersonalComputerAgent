import type { EventEnvelope } from "@pca/contracts/src/types.js";
import { validateContract } from "@pca/contracts/src/validate.js";
import type {
  CommunicationEventRecord,
  SystemEventRecord,
} from "@pca/db-cloud/src/repository.js";

export interface SyncBatchRequest {
  batchId: string;
  deviceId: string;
  events: SystemEventRecord[];
}

export interface CommunicationSyncBatchRequest {
  batchId: string;
  deviceId: string;
  events: CommunicationEventRecord[];
}

export function parseSyncBatch(value: unknown): SyncBatchRequest | null {
  if (!validateContract("sync-batch-request", value).valid || !isRecord(value)) return null;
  if (value.compressed === true) return null;
  const events = value.events as EventEnvelope[];
  const parsed = events.map(parseSystemEvent);
  if (parsed.some((event) => event === null)) return null;
  return {
    batchId: value.batch_id as string,
    deviceId: value.device_id as string,
    events: parsed as SystemEventRecord[],
  };
}

export function parseCommunicationSyncBatch(value: unknown): CommunicationSyncBatchRequest | null {
  if (!validateContract("sync-batch-request", value).valid || !isRecord(value)) return null;
  if (value.compressed === true) return null;
  const events = (value.events as EventEnvelope[]).map(parseCommunicationEvent);
  if (events.some((event) => event === null)) return null;
  return {
    batchId: value.batch_id as string,
    deviceId: value.device_id as string,
    events: events as CommunicationEventRecord[],
  };
}

function parseSystemEvent(event: EventEnvelope): SystemEventRecord | null {
  if ((event.attachment_refs?.length ?? 0) !== 0 || event.schema_version !== 1 || event.sensitivity !== "normal") {
    return null;
  }
  if (event.event_type === "system.metric_sampled") {
    if (event.source !== "system" || !validateContract("system-metric-sampled", event.payload).valid) return null;
  } else if (event.event_type === "system.health_changed") {
    if (event.source !== "system" || !validateContract("system-health-changed", event.payload).valid) return null;
  } else if (event.event_type === "collector.status_changed") {
    if (event.source !== "collector.registry" || !validateContract("collector-status-changed", event.payload).valid) return null;
  } else {
    return null;
  }
  const occurredAt = new Date(event.occurred_at);
  const createdAt = new Date(event.created_at);
  if (Number.isNaN(occurredAt.getTime()) || Number.isNaN(createdAt.getTime())) return null;
  return {
    eventId: event.event_id,
    workspaceId: event.workspace_id,
    deviceId: event.device_id,
    eventType: event.event_type,
    source: event.source,
    schemaVersion: event.schema_version,
    occurredAt,
    createdAt,
    sensitivity: event.sensitivity,
    payload: event.payload as unknown as Record<string, unknown>,
    idempotencyKey: event.idempotency_key ?? null,
  };
}

function parseCommunicationEvent(event: EventEnvelope): CommunicationEventRecord | null {
  if (
    event.schema_version !== 1
    || event.sensitivity !== "high"
    || event.event_type !== "communication.message_recorded"
    || event.source !== "communication.wechat"
    || !validateContract("communication-message-recorded", event.payload).valid
  ) {
    return null;
  }
  const occurredAt = new Date(event.occurred_at);
  const createdAt = new Date(event.created_at);
  if (Number.isNaN(occurredAt.getTime()) || Number.isNaN(createdAt.getTime())) return null;
  return {
    eventId: event.event_id,
    workspaceId: event.workspace_id,
    deviceId: event.device_id,
    eventType: "communication.message_recorded",
    source: "communication.wechat",
    schemaVersion: 1,
    occurredAt,
    createdAt,
    sensitivity: "high",
    payload: event.payload as unknown as Record<string, unknown>,
    attachmentRefs: event.attachment_refs ?? [],
    idempotencyKey: event.idempotency_key ?? null,
  };
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
