import type { EventEnvelope } from "@pca/contracts/src/types.js";
import { validateContract } from "@pca/contracts/src/validate.js";
import type {
  CommunicationAttachmentProjection,
  CommunicationConversationProjection,
  CommunicationEventRecord,
  CommunicationMessageProjection,
  CommunicationMessageSenderProjection,
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

type LifecycleEventType =
  | "agent.started"
  | "agent.stopped"
  | "agent.crash_recovered"
  | "system.sleep"
  | "system.wake"
  | "network.offline"
  | "network.online";

const lifecycleEventTypes: readonly LifecycleEventType[] = [
  "agent.started",
  "agent.stopped",
  "agent.crash_recovered",
  "system.sleep",
  "system.wake",
  "network.offline",
  "network.online",
];

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
  } else if (isLifecycleEventType(event.event_type)) {
    if (
      event.source !== "runtime.lifecycle"
      || !isRecord(event.payload)
      || Object.keys(event.payload).length !== 0
    ) return null;
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

function isLifecycleEventType(value: string): value is LifecycleEventType {
  return (lifecycleEventTypes as readonly string[]).includes(value);
}

function parseCommunicationEvent(event: EventEnvelope): CommunicationEventRecord | null {
  if (
    event.schema_version !== 1
    || event.sensitivity !== "high"
    || event.source !== "communication.wechat"
  ) {
    return null;
  }
  const occurredAt = new Date(event.occurred_at);
  const createdAt = new Date(event.created_at);
  if (Number.isNaN(occurredAt.getTime()) || Number.isNaN(createdAt.getTime())) return null;
  if (event.event_type === "communication.conversation_observed") {
    if (
      (event.attachment_refs?.length ?? 0) !== 0
      || !validateContract("communication-conversation-observed", event.payload).valid
    ) return null;
    const conversation = parseCommunicationConversation(event.payload, occurredAt);
    if (conversation === null || event.idempotency_key === undefined) return null;
    return {
      eventId: event.event_id,
      workspaceId: event.workspace_id,
      deviceId: event.device_id,
      eventType: "communication.conversation_observed",
      source: "communication.wechat",
      schemaVersion: 1,
      occurredAt,
      createdAt,
      sensitivity: "high",
      payload: event.payload as unknown as Record<string, unknown>,
      attachmentRefs: [],
      idempotencyKey: event.idempotency_key,
      conversation,
    };
  }
  if (event.event_type === "communication.message_sender_observed") {
    if (
      (event.attachment_refs?.length ?? 0) !== 0
      || !validateContract("communication-message-sender-observed", event.payload).valid
    ) return null;
    const sender = parseCommunicationMessageSender(event.payload, occurredAt);
    if (sender === null || event.idempotency_key === undefined) return null;
    return {
      eventId: event.event_id,
      workspaceId: event.workspace_id,
      deviceId: event.device_id,
      eventType: "communication.message_sender_observed",
      source: "communication.wechat",
      schemaVersion: 1,
      occurredAt,
      createdAt,
      sensitivity: "high",
      payload: event.payload as unknown as Record<string, unknown>,
      attachmentRefs: [],
      idempotencyKey: event.idempotency_key,
      sender,
    };
  }
  if (
    event.event_type !== "communication.message_recorded"
    || !validateContract("communication-message-recorded", event.payload).valid
  ) return null;
  const message = parseCommunicationMessage(event.payload, occurredAt, event.attachment_refs ?? []);
  if (message === null || event.idempotency_key !== message.sourceKey) return null;
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
    message,
  };
}

function parseCommunicationConversation(
  payload: unknown,
  eventOccurredAt: Date,
): CommunicationConversationProjection | null {
  const value = payload as {
    conversation_id: string;
    display_name: string;
    avatar_url?: string;
    observed_at: string;
    conversation: { scope: "direct" | "group"; member_count?: number };
  };
  const observedAt = new Date(value.observed_at);
  if (Number.isNaN(observedAt.getTime()) || observedAt.getTime() !== eventOccurredAt.getTime()) {
    return null;
  }
  return {
    conversationId: value.conversation_id,
    displayName: value.display_name.trim(),
    avatarUrl: value.avatar_url ?? null,
    observedAt,
    scope: value.conversation.scope,
    memberCount: value.conversation.scope === "group" ? value.conversation.member_count ?? null : null,
  };
}

function parseCommunicationMessage(
  payload: unknown,
  eventOccurredAt: Date,
  attachmentRefs: string[],
): CommunicationMessageProjection | null {
  const message = payload as {
    message_id: string;
    conversation_id: string;
    sender_id: string;
    sender_display_name: string;
    source_key: string;
    occurred_at: string;
    direction: "incoming" | "outgoing";
    kind: "text" | "audio" | "image" | "video" | "file";
    conversation: { scope: "direct" | "group"; member_count?: number };
    text?: string;
    attachments?: Array<{
      attachment_id: string;
      kind: "audio" | "image" | "video" | "file";
      sha256: string;
      size_bytes: number;
      mime_type: string;
      file_name?: string;
    }>;
  };
  const occurredAt = new Date(message.occurred_at);
  if (Number.isNaN(occurredAt.getTime()) || occurredAt.getTime() !== eventOccurredAt.getTime()) return null;
  const attachments = (message.attachments ?? []).map<CommunicationAttachmentProjection>((attachment) => ({
    attachmentId: attachment.attachment_id,
    kind: attachment.kind,
    sha256: attachment.sha256,
    sizeBytes: attachment.size_bytes,
    mimeType: attachment.mime_type,
    fileName: attachment.file_name ?? null,
  }));
  if (
    attachments.length !== attachmentRefs.length
    || new Set(attachments.map((attachment) => attachment.attachmentId)).size !== attachments.length
    || attachments.some((attachment) => !attachmentRefs.includes(attachment.attachmentId))
  ) {
    return null;
  }
  return {
    messageId: message.message_id,
    conversationId: message.conversation_id,
    senderId: message.sender_id,
    senderDisplayName: message.sender_display_name.trim(),
    senderAvatarUrl: null,
    sourceKey: message.source_key,
    occurredAt,
    direction: message.direction,
    kind: message.kind,
    conversation: {
      scope: message.conversation.scope,
      memberCount: message.conversation.scope === "group" ? message.conversation.member_count ?? null : null,
    },
    text: message.kind === "text" ? message.text ?? null : null,
    attachments,
  };
}

function parseCommunicationMessageSender(
  payload: unknown,
  eventOccurredAt: Date,
): CommunicationMessageSenderProjection | null {
  const value = payload as {
    message_id: string;
    source_key: string;
    sender_id: string;
    sender_display_name: string;
    avatar_url?: string;
    observed_at: string;
  };
  const observedAt = new Date(value.observed_at);
  if (Number.isNaN(observedAt.getTime()) || observedAt.getTime() !== eventOccurredAt.getTime()) {
    return null;
  }
  return {
    messageId: value.message_id,
    sourceKey: value.source_key,
    senderId: value.sender_id,
    senderDisplayName: value.sender_display_name.trim(),
    avatarUrl: value.avatar_url ?? null,
    observedAt,
  };
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
