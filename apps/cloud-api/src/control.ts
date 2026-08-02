import type { StoredCollectorConfig } from "@pca/db-cloud/src/schema.js";

export interface HeartbeatRequest {
  heartbeatId: string;
  agentVersion: string;
  presence: "online" | "stale" | "offline" | "sleeping";
  outboxDepth: number;
}

export function parseHeartbeat(value: unknown): HeartbeatRequest | null {
  if (!isRecord(value) || !hasOnly(value, ["heartbeat_id", "agent_version", "presence", "outbox_depth"])) {
    return null;
  }
  const heartbeatId = value.heartbeat_id;
  const agentVersion = value.agent_version;
  const presence = value.presence;
  const outboxDepth = value.outbox_depth;
  if (
    typeof heartbeatId !== "string" ||
    typeof agentVersion !== "string" ||
    !isPresence(presence) ||
    typeof outboxDepth !== "number" ||
    !Number.isSafeInteger(outboxDepth) ||
    outboxDepth < 0
  ) {
    return null;
  }
  return { heartbeatId, agentVersion, presence, outboxDepth };
}

export function parseCollectorConfig(value: unknown): StoredCollectorConfig | null {
  if (
    !isRecord(value) ||
    !hasOnly(value, ["network", "communication.wechat"]) ||
    !("network" in value) ||
    !("communication.wechat" in value)
  ) {
    return null;
  }
  const network = value.network;
  const wechat = value["communication.wechat"];
  if (!isRecord(network) || !hasOnly(network, ["enabled"]) || typeof network.enabled !== "boolean") {
    return null;
  }
  if (
    !isRecord(wechat) ||
    !hasOnly(wechat, [
      "enabled",
      "directions",
      "message_types",
      "conversation_scope",
      "max_group_members",
      "sync_mode",
      "retention_days",
    ]) ||
    typeof wechat.enabled !== "boolean" ||
    !isExact(wechat.directions, ["incoming", "outgoing"]) ||
    !isExact(wechat.message_types, ["text", "audio", "image", "video"]) ||
    wechat.conversation_scope !== "direct_and_group_at_most_fifteen_members" ||
    wechat.max_group_members !== 15 ||
    wechat.sync_mode !== "full" ||
    wechat.retention_days !== 180
  ) {
    return null;
  }
  return { networkEnabled: network.enabled, wechatEnabled: wechat.enabled };
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function hasOnly(value: Record<string, unknown>, allowed: readonly string[]): boolean {
  return Object.keys(value).every((key) => allowed.includes(key));
}

function isExact(value: unknown, expected: readonly string[]): boolean {
  return Array.isArray(value) && value.length === expected.length && value.every((item, index) => item === expected[index]);
}

function isPresence(value: unknown): value is HeartbeatRequest["presence"] {
  return value === "online" || value === "stale" || value === "offline" || value === "sleeping";
}
