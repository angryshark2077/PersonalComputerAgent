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
    location: {
      latitude: number;
      longitude: number;
      horizontalAccuracyMeters: number;
      observedAt: Date;
    } | null;
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
    "location",
  ])) return undefined;
  const interfaceType = value.interface_type;
  const ssid = value.ssid;
  const bssid = value.bssid;
  const localIpv4 = value.local_ipv4;
  const localIpv6 = value.local_ipv6;
  const location = value.location === undefined ? null : parseDeviceLocation(value.location);
  if (
    !isInterfaceType(interfaceType)
    || typeof value.wifi_identity_available !== "boolean"
    || !(ssid === null || (typeof ssid === "string" && ssid.length > 0 && ssid.length <= 128))
    || !(bssid === null || (typeof bssid === "string" && /^[0-9A-F]{2}(:[0-9A-F]{2}){5}$/.test(bssid)))
    || !(localIpv4 === null || (typeof localIpv4 === "string" && isIP(localIpv4) === 4))
    || !(localIpv6 === null || (typeof localIpv6 === "string" && isIP(localIpv6) === 6))
    || location === undefined
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
    location,
  };
}

function parseDeviceLocation(value: unknown): NonNullable<HeartbeatRequest["network"]>["location"] | undefined {
  if (value === null) return null;
  if (!isRecord(value) || !hasOnly(value, [
    "latitude",
    "longitude",
    "horizontal_accuracy_meters",
    "observed_at",
  ])) return undefined;
  const latitude = value.latitude;
  const longitude = value.longitude;
  const horizontalAccuracyMeters = value.horizontal_accuracy_meters;
  const observedAt = value.observed_at;
  if (
    typeof latitude !== "number" || !Number.isFinite(latitude) || latitude < -90 || latitude > 90
    || typeof longitude !== "number" || !Number.isFinite(longitude) || longitude < -180 || longitude > 180
    || typeof horizontalAccuracyMeters !== "number" || !Number.isFinite(horizontalAccuracyMeters)
    || horizontalAccuracyMeters < 0 || horizontalAccuracyMeters > 100_000
    || typeof observedAt !== "string" || observedAt.length === 0 || observedAt.length > 64
    || Number.isNaN(Date.parse(observedAt))
  ) return undefined;
  return { latitude, longitude, horizontalAccuracyMeters, observedAt: new Date(observedAt) };
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
    !hasOnly(value, ["network", "screen.capture", "communication.wechat", "communication.messages", "photos.library"]) ||
    !("network" in value) ||
    !("screen.capture" in value) ||
    !("communication.wechat" in value) ||
    !("communication.messages" in value) ||
    !("photos.library" in value)
  ) {
    return null;
  }
  const network = value.network;
  const screen = value["screen.capture"];
  const wechat = value["communication.wechat"];
  const messages = value["communication.messages"];
  const photos = value["photos.library"];
  if (!isRecord(network) || !hasOnly(network, ["enabled"]) || typeof network.enabled !== "boolean") {
    return null;
  }
  if (
    !isRecord(screen)
    || !hasOnly(screen, [
      "enabled",
      "scheduled_enabled",
      "interval_seconds",
      "activity_enabled",
      "activity_min_interval_seconds",
      "excluded_bundle_ids",
    ])
    || typeof screen.enabled !== "boolean"
    || typeof screen.scheduled_enabled !== "boolean"
    || !Number.isInteger(screen.interval_seconds)
    || (screen.interval_seconds as number) < 60
    || (screen.interval_seconds as number) > 86_400
    || typeof screen.activity_enabled !== "boolean"
    || !Number.isInteger(screen.activity_min_interval_seconds)
    || (screen.activity_min_interval_seconds as number) < 10
    || (screen.activity_min_interval_seconds as number) > 3_600
    || !Array.isArray(screen.excluded_bundle_ids)
    || screen.excluded_bundle_ids.length > 100
    || screen.excluded_bundle_ids.some((item) =>
      typeof item !== "string" || item.length === 0 || item.length > 255 || !/^[A-Za-z0-9.-]+$/.test(item))
    || new Set(screen.excluded_bundle_ids).size !== screen.excluded_bundle_ids.length
  ) return null;
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
  if (
    !isRecord(messages) ||
    !hasOnly(messages, [
      "enabled", "directions", "message_types", "conversation_scope", "initial_lookback_days",
      "sync_mode", "attachments_enabled", "attachment_retention_days",
    ]) ||
    typeof messages.enabled !== "boolean" ||
    !isExact(messages.directions, ["incoming", "outgoing"]) ||
    !isExact(messages.message_types, ["text"]) ||
    messages.conversation_scope !== "all" ||
    messages.initial_lookback_days !== 7 ||
    messages.sync_mode !== "full" ||
    messages.attachments_enabled !== false ||
    messages.attachment_retention_days !== 7
  ) return null;
  if (
    !isRecord(photos) ||
    !hasOnly(photos, [
      "enabled", "media_types", "include_originals", "include_album_names",
      "initial_lookback_days", "cloud_retention",
    ]) ||
    typeof photos.enabled !== "boolean" ||
    !isExact(photos.media_types, ["image", "video"]) ||
    photos.include_originals !== true ||
    photos.include_album_names !== true ||
    photos.initial_lookback_days !== 60 ||
    photos.cloud_retention !== "permanent"
  ) return null;
  return {
    networkEnabled: network.enabled,
    wechatEnabled: wechat.enabled,
    messagesEnabled: messages.enabled,
    photosEnabled: photos.enabled,
    screenCaptureEnabled: screen.enabled,
    screenCaptureScheduledEnabled: screen.scheduled_enabled,
    screenCaptureIntervalSeconds: screen.interval_seconds as number,
    screenCaptureActivityEnabled: screen.activity_enabled,
    screenCaptureActivityMinIntervalSeconds: screen.activity_min_interval_seconds as number,
    screenCaptureExcludedBundleIds: [...screen.excluded_bundle_ids] as string[],
  };
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
