import type { StoredCollectorConfig } from "@pca/db-cloud/src/schema.js";

export interface HeartbeatRequest {
  heartbeatId: string;
  agentVersion: string;
  presence: "online" | "stale" | "offline" | "sleeping";
  outboxDepth: number;
  localMedia: {
    completedFileCount: number;
    completedBytes: number;
    protectedFileCount: number;
    protectedBytes: number;
  };
  cleanupResult: {
    requestId: string;
    status: "succeeded" | "failed";
    deletedFileCount: number;
    freedBytes: number;
    errorCode: string | null;
  } | null;
  network: {
    interfaceType: "wifi" | "wired" | "other" | "none";
    wifiIdentityAvailable: boolean;
    ssid: string | null;
    bssid: string | null;
    localIpv4: string | null;
    localIpv6: string | null;
  } | null;
}

export function parseHeartbeat(value: unknown): HeartbeatRequest | null {
  if (!isRecord(value) || !hasOnly(value, [
    "heartbeat_id",
    "agent_version",
    "presence",
    "outbox_depth",
    "local_media",
    "cleanup_result",
    "network",
  ])) {
    return null;
  }
  const heartbeatId = value.heartbeat_id;
  const agentVersion = value.agent_version;
  const presence = value.presence;
  const outboxDepth = value.outbox_depth;
  const localMedia = parseLocalMedia(value.local_media);
  const cleanupResult = parseCleanupResult(value.cleanup_result);
  const network = value.network === undefined ? null : parseNetwork(value.network);
  if (
    typeof heartbeatId !== "string" ||
    typeof agentVersion !== "string" ||
    !isPresence(presence) ||
    typeof outboxDepth !== "number" ||
    !Number.isSafeInteger(outboxDepth) ||
    outboxDepth < 0
    || localMedia === null
    || cleanupResult === undefined
    || network === undefined
  ) {
    return null;
  }
  return { heartbeatId, agentVersion, presence, outboxDepth, localMedia, cleanupResult, network };
}

function parseNetwork(value: unknown): HeartbeatRequest["network"] | undefined {
  if (value === null) return null;
  if (!isRecord(value) || !hasOnly(value, [
    "interface_type",
    "wifi_identity_available",
    "ssid",
    "bssid",
    "local_ipv4",
    "local_ipv6",
  ])) return undefined;
  const interfaceType = value.interface_type;
  const ssid = value.ssid;
  const bssid = value.bssid;
  const localIpv4 = value.local_ipv4;
  const localIpv6 = value.local_ipv6;
  if (
    !isInterfaceType(interfaceType)
    || typeof value.wifi_identity_available !== "boolean"
    || !(ssid === null || (typeof ssid === "string" && ssid.length > 0 && ssid.length <= 128))
    || !(bssid === null || (typeof bssid === "string" && /^[0-9A-F]{2}(:[0-9A-F]{2}){5}$/.test(bssid)))
    || !(localIpv4 === null || (typeof localIpv4 === "string" && isIP(localIpv4) === 4))
    || !(localIpv6 === null || (typeof localIpv6 === "string" && isIP(localIpv6) === 6))
    || (interfaceType !== "wifi" && (ssid !== null || bssid !== null))
    || value.wifi_identity_available !== (interfaceType === "wifi" && ssid !== null && bssid !== null)
  ) return undefined;
  return {
    interfaceType,
    wifiIdentityAvailable: value.wifi_identity_available,
    ssid,
    bssid,
    localIpv4,
    localIpv6,
  };
}

function parseLocalMedia(value: unknown): HeartbeatRequest["localMedia"] | null {
  if (!isRecord(value) || !hasOnly(value, [
    "completed_file_count",
    "completed_bytes",
    "protected_file_count",
    "protected_bytes",
  ])) return null;
  const values = [
    value.completed_file_count,
    value.completed_bytes,
    value.protected_file_count,
    value.protected_bytes,
  ];
  if (!values.every((candidate) => typeof candidate === "number" && Number.isSafeInteger(candidate) && candidate >= 0)) {
    return null;
  }
  return {
    completedFileCount: value.completed_file_count as number,
    completedBytes: value.completed_bytes as number,
    protectedFileCount: value.protected_file_count as number,
    protectedBytes: value.protected_bytes as number,
  };
}

function parseCleanupResult(value: unknown): HeartbeatRequest["cleanupResult"] | undefined {
  if (value === null) return null;
  if (!isRecord(value) || !hasOnly(value, [
    "request_id",
    "status",
    "deleted_file_count",
    "freed_bytes",
    "error_code",
  ])) return undefined;
  if (
    typeof value.request_id !== "string"
    || !/^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(value.request_id)
    || (value.status !== "succeeded" && value.status !== "failed")
    || typeof value.deleted_file_count !== "number"
    || !Number.isSafeInteger(value.deleted_file_count)
    || value.deleted_file_count < 0
    || typeof value.freed_bytes !== "number"
    || !Number.isSafeInteger(value.freed_bytes)
    || value.freed_bytes < 0
    || !((value.status === "succeeded" && value.error_code === null)
      || (value.status === "failed" && typeof value.error_code === "string" && value.error_code.length > 0))
  ) return undefined;
  return {
    requestId: value.request_id,
    status: value.status,
    deletedFileCount: value.deleted_file_count,
    freedBytes: value.freed_bytes,
    errorCode: value.error_code,
  };
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

function isInterfaceType(value: unknown): value is NonNullable<HeartbeatRequest["network"]>["interfaceType"] {
  return value === "wifi" || value === "wired" || value === "other" || value === "none";
}
import { isIP } from "node:net";
