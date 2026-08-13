export interface CollectorConfig {
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
    message_types: ["text", "audio", "image", "video"] | ["text", "audio", "image", "video", "file"];
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
}

export interface DashboardScreenshot {
  screenshot_id: string;
  request_id: string | null;
  trigger: "manual" | "scheduled" | "activity";
  captured_at: string;
  app_bundle_id: string | null;
  pixel_width: number;
  pixel_height: number;
  size_bytes: number;
  mime_type: "image/jpeg";
}

export interface DashboardPhoto {
  photo_id: string;
  captured_at: string;
  media_type: "image" | "video";
  original_filename: string;
  mime_type: string;
  pixel_width: number;
  pixel_height: number;
  duration_seconds: number;
  album_names: string[];
  size_bytes: number;
}

export const PHOTO_PAGE_SIZE = 20;

export interface DashboardPhotoPage {
  photos: DashboardPhoto[];
  pagination: {
    page: number;
    page_size: number;
    total_count: number;
    total_pages: number;
  };
}

export const SCREENSHOT_PAGE_SIZE = 20;

export interface DashboardScreenshotPage {
  screenshots: DashboardScreenshot[];
  pagination: {
    page: number;
    page_size: number;
    total_count: number;
    total_pages: number;
  };
}

export interface DashboardNetworkObservation {
  interface_type: "wifi" | "wired" | "other" | "none";
  wifi_identity_available: boolean;
  ssid: string | null;
  bssid: string | null;
  local_ipv4: string | null;
  local_ipv6: string | null;
  observed_exit_ip: string | null;
  exit_ip_location: { country: string | null; region: string | null; city: string | null; accuracy: "ip_city" } | null;
  device_location: {
    latitude: number;
    longitude: number;
    horizontal_accuracy_meters: number;
    observed_at: string;
  } | null;
  matched_location: DashboardNetworkLocation | null;
}

export interface DashboardDevice {
  device_id: string;
  workspace_id: string;
  platform: "macos";
  paired_at: string;
  revoked: boolean;
  configuration_revision: number;
  status: {
    presence: "online" | "stale" | "offline" | "sleeping";
    agent_version: string;
    outbox_depth: number;
    local_media: {
      completed_file_count: number;
      completed_bytes: number;
      protected_file_count: number;
      protected_bytes: number;
    };
    network: DashboardNetworkObservation | null;
    network_history?: Array<DashboardNetworkObservation & { observed_at: string }>;
    collector_health: DashboardCollectorHealth[];
    observed_at: string;
  } | null;
  local_media_cleanup: {
    request_id: string;
    status: "queued" | "succeeded" | "failed";
    requested_at: string;
    completed_at: string | null;
    deleted_file_count: number | null;
    freed_bytes: number | null;
    error_code: string | null;
  } | null;
  collectors: CollectorConfig;
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

export interface DashboardNetworkLocation {
  location_id: string;
  name: string;
  match_ssid: string | null;
  match_bssid: string | null;
  country: string | null;
  region: string | null;
  city: string | null;
  created_at: string;
  updated_at: string;
}

export interface DashboardWorkspace {
  workspace_id: string;
  name: string;
}

export interface DashboardSystemMetric {
  event_id: string;
  occurred_at: string;
  metric_group: "cpu_memory" | "disk";
  payload: Record<string, unknown>;
}

export interface DashboardConversation {
  conversation_id: string;
  display_name: string;
  avatar_url: string | null;
  scope: "direct" | "group";
  member_count: number | null;
  message_count: number;
  last_message_at: string;
}

export interface DashboardConversationPage {
  conversations: DashboardConversation[];
  pagination: {
    page: number;
    page_size: number;
    total_count: number;
    total_pages: number;
  };
}

export type CommunicationSource = "communication.wechat" | "communication.messages";

export interface DashboardMessageAttachment {
  attachment_id: string;
  kind: "audio" | "image" | "video" | "file";
  sha256: string;
  size_bytes: number;
  mime_type: string;
  file_name?: string;
  object_id: string | null;
  object_state: "prepared" | "completed" | null;
}

export interface DashboardMessage {
  event_id: string;
  message_id: string;
  sender_id: string;
  sender_display_name: string;
  sender_avatar_url: string | null;
  occurred_at: string;
  direction: "incoming" | "outgoing";
  kind: "text" | "audio" | "image" | "video" | "file";
  text: string | null;
  attachments: DashboardMessageAttachment[];
}

export function mergeLatestCommunicationMessages(
  current: DashboardMessage[] | null,
  latestNewestFirst: DashboardMessage[],
): DashboardMessage[] {
  const byEventId = new Map((current ?? []).map((message) => [message.event_id, message]));
  for (const message of latestNewestFirst) byEventId.set(message.event_id, message);
  return [...byEventId.values()].sort((left, right) =>
    Date.parse(left.occurred_at) - Date.parse(right.occurred_at)
    || left.event_id.localeCompare(right.event_id)
  );
}

export interface CollectorConfigAudit {
  actor_user_id: string;
  configuration_revision: number;
  old_config: CollectorConfig;
  new_config: CollectorConfig;
  created_at: string;
}

export interface DashboardApiErrorBody {
  error?: { error_code?: string; message?: string };
}

export class DashboardApiError extends Error {
  readonly code: string;

  constructor(code: string, message: string) {
    super(message);
    this.name = "DashboardApiError";
    this.code = code;
  }
}

export type DashboardFetch = (input: RequestInfo | URL, init?: RequestInit) => Promise<Response>;

const defaultScreenCaptureConfig: CollectorConfig["screen.capture"] = {
  enabled: false,
  scheduled_enabled: true,
  interval_seconds: 300,
  activity_enabled: true,
  activity_min_interval_seconds: 30,
  excluded_bundle_ids: [],
};

const defaultMessagesConfig: CollectorConfig["communication.messages"] = {
  enabled: false,
  directions: ["incoming", "outgoing"],
  message_types: ["text"],
  conversation_scope: "all",
  initial_lookback_days: 7,
  sync_mode: "full",
  attachments_enabled: false,
  attachment_retention_days: 7,
};

const defaultPhotosConfig: CollectorConfig["photos.library"] = {
  enabled: false,
  media_types: ["image", "video"],
  include_originals: true,
  include_album_names: true,
  initial_lookback_days: 60,
  cloud_retention: "permanent",
};

export function normalizeDashboardDevice(device: DashboardDevice): DashboardDevice {
  const collectors = device.collectors as Partial<CollectorConfig>;
  return {
    ...device,
    status: device.status === null
      ? null
      : { ...device.status, collector_health: device.status.collector_health ?? [] },
    collectors: {
      ...device.collectors,
      "screen.capture": collectors["screen.capture"] ?? defaultScreenCaptureConfig,
      "communication.messages": collectors["communication.messages"] ?? defaultMessagesConfig,
      "photos.library": collectors["photos.library"] ?? defaultPhotosConfig,
    },
  };
}

export async function getDevice(
  fetcher: DashboardFetch,
  cloudApiOrigin: string,
  deviceId: string,
): Promise<DashboardDevice> {
  const device = await jsonRequest<DashboardDevice>(
    fetcher,
    apiUrl(cloudApiOrigin, `/v1/devices/${encodeURIComponent(deviceId)}`),
  );
  return normalizeDashboardDevice(device);
}

export async function getDevices(
  fetcher: DashboardFetch,
  cloudApiOrigin: string,
): Promise<Omit<DashboardDevice, "collectors">[]> {
  const result = await jsonRequest<{ devices: Omit<DashboardDevice, "collectors">[] }>(
    fetcher,
    apiUrl(cloudApiOrigin, "/v1/devices"),
  );
  return result.devices;
}

export function decodeDashboardRouteParam(value: string): string {
  try {
    return decodeURIComponent(value);
  } catch {
    return value;
  }
}

export function chatReadStorageKey(deviceId: string, conversationId: string): string {
  return `pca.chat-read:${deviceId}:${conversationId}`;
}

export function chatReadBaselineStorageKey(deviceId: string): string {
  return `pca.chat-read-baseline:${deviceId}`;
}

export function messagesReadStorageKey(deviceId: string, conversationId: string): string {
  return `pca.messages-read:${deviceId}:${conversationId}`;
}

export function messagesReadBaselineStorageKey(deviceId: string): string {
  return `pca.messages-read-baseline:${deviceId}`;
}

export function initializeChatReadAt(
  lastMessageAt: string,
  storedReadAt: string | null,
  baselineInitialized = false,
): string | null {
  if (storedReadAt !== null) return storedReadAt;
  return baselineInitialized ? null : lastMessageAt;
}

export function isConversationUnread(lastMessageAt: string, readAt: string | null): boolean {
  if (readAt === null) return true;
  const last = Date.parse(lastMessageAt);
  const read = Date.parse(readAt);
  return Number.isNaN(last) || Number.isNaN(read) || last > read;
}

export async function getWorkspaces(
  fetcher: DashboardFetch,
  cloudApiOrigin: string,
): Promise<DashboardWorkspace[]> {
  const result = await jsonRequest<{ workspaces: DashboardWorkspace[] }>(
    fetcher,
    apiUrl(cloudApiOrigin, "/v1/workspaces"),
  );
  return result.workspaces;
}

export async function getCollectorAudit(
  fetcher: DashboardFetch,
  cloudApiOrigin: string,
  deviceId: string,
): Promise<CollectorConfigAudit[]> {
  const result = await jsonRequest<{ audit: CollectorConfigAudit[] }>(
    fetcher,
    apiUrl(cloudApiOrigin, `/v1/devices/${encodeURIComponent(deviceId)}/collector-config/audit`),
  );
  return result.audit;
}

export async function getSystemMetrics(
  fetcher: DashboardFetch,
  cloudApiOrigin: string,
  deviceId: string,
): Promise<DashboardSystemMetric[]> {
  const result = await jsonRequest<{ metrics: DashboardSystemMetric[] }>(
    fetcher,
    apiUrl(cloudApiOrigin, `/v1/devices/${encodeURIComponent(deviceId)}/system-metrics`),
  );
  return result.metrics;
}

export async function getCommunicationConversations(
  fetcher: DashboardFetch,
  cloudApiOrigin: string,
  deviceId: string,
  source: CommunicationSource,
  limit = 100,
  page = 1,
): Promise<DashboardConversationPage> {
  return jsonRequest<DashboardConversationPage>(
    fetcher,
    apiUrl(
      cloudApiOrigin,
      `/v1/devices/${encodeURIComponent(deviceId)}/communication/conversations?source=${encodeURIComponent(source)}&limit=${limit}&page=${page}`,
    ),
  );
}

export async function getCommunicationMessages(
  fetcher: DashboardFetch,
  cloudApiOrigin: string,
  deviceId: string,
  conversationId: string,
  source: CommunicationSource,
  limit = 100,
  before?: Pick<DashboardMessage, "occurred_at" | "event_id">,
): Promise<DashboardMessage[]> {
  const query = new URLSearchParams({ source, limit: String(limit) });
  if (before !== undefined) {
    query.set("before", before.occurred_at);
    query.set("before_event_id", before.event_id);
  }
  const result = await jsonRequest<{ messages: DashboardMessage[] }>(
    fetcher,
    apiUrl(
      cloudApiOrigin,
      `/v1/devices/${encodeURIComponent(deviceId)}/communication/conversations/${encodeURIComponent(conversationId)}/messages?${query}`,
    ),
  );
  return result.messages;
}

export async function getCommunicationObjectReadUrl(
  fetcher: DashboardFetch,
  cloudApiOrigin: string,
  deviceId: string,
  objectId: string,
): Promise<string> {
  const result = await jsonRequest<{ url: string; expires_at: string }>(
    fetcher,
    apiUrl(
      cloudApiOrigin,
      `/v1/devices/${encodeURIComponent(deviceId)}/communication/objects/${encodeURIComponent(objectId)}/read`,
    ),
  );
  return result.url;
}

export async function updateCollectorConfig(
  fetcher: DashboardFetch,
  cloudApiOrigin: string,
  deviceId: string,
  config: CollectorConfig,
): Promise<number> {
  const result = await jsonRequest<{ configuration_revision: number }>(
    fetcher,
    apiUrl(cloudApiOrigin, `/v1/devices/${encodeURIComponent(deviceId)}/collector-config`),
    { method: "PUT", body: JSON.stringify(config) },
  );
  return result.configuration_revision;
}

export async function revokeDevice(
  fetcher: DashboardFetch,
  cloudApiOrigin: string,
  deviceId: string,
): Promise<void> {
  const response = await fetcher(apiUrl(cloudApiOrigin, `/v1/devices/${encodeURIComponent(deviceId)}/revoke`), {
    method: "POST",
    credentials: "include",
  });
  if (response.status !== 204) throw await apiError(response);
}

export async function purgeDevice(
  fetcher: DashboardFetch,
  cloudApiOrigin: string,
  deviceId: string,
): Promise<number> {
  const result = await jsonRequest<{ deleted: true; deleted_object_count: number }>(
    fetcher,
    apiUrl(cloudApiOrigin, `/v1/devices/${encodeURIComponent(deviceId)}/purge`),
    { method: "POST", body: JSON.stringify({ confirmation: deviceId }) },
  );
  return result.deleted_object_count;
}

export async function requestLocalMediaCleanup(
  fetcher: DashboardFetch,
  cloudApiOrigin: string,
  deviceId: string,
): Promise<void> {
  await jsonRequest(
    fetcher,
    apiUrl(
      cloudApiOrigin,
      `/v1/devices/${encodeURIComponent(deviceId)}/communication/local-media/cleanup`,
    ),
    { method: "POST" },
  );
}

export async function requestScreenshot(
  fetcher: DashboardFetch,
  cloudApiOrigin: string,
  deviceId: string,
): Promise<void> {
  await jsonRequest(
    fetcher,
    apiUrl(cloudApiOrigin, `/v1/devices/${encodeURIComponent(deviceId)}/screenshots`),
    { method: "POST" },
  );
}

export async function getScreenshots(
  fetcher: DashboardFetch,
  cloudApiOrigin: string,
  deviceId: string,
  limit = SCREENSHOT_PAGE_SIZE,
  page = 1,
): Promise<DashboardScreenshotPage> {
  const query = new URLSearchParams({ limit: String(limit), page: String(page) });
  return jsonRequest<DashboardScreenshotPage>(
    fetcher,
    apiUrl(cloudApiOrigin, `/v1/devices/${encodeURIComponent(deviceId)}/screenshots?${query}`),
  );
}

export async function getScreenshotReadUrl(
  fetcher: DashboardFetch,
  cloudApiOrigin: string,
  deviceId: string,
  screenshotId: string,
): Promise<string> {
  const result = await jsonRequest<{ url: string; expires_at: string }>(
    fetcher,
    apiUrl(
      cloudApiOrigin,
      `/v1/devices/${encodeURIComponent(deviceId)}/screenshots/${encodeURIComponent(screenshotId)}/read`,
    ),
  );
  return result.url;
}

export async function getPhotos(
  fetcher: DashboardFetch,
  cloudApiOrigin: string,
  deviceId: string,
  limit = PHOTO_PAGE_SIZE,
  page = 1,
): Promise<DashboardPhotoPage> {
  return jsonRequest<DashboardPhotoPage>(
    fetcher,
    apiUrl(cloudApiOrigin, `/v1/devices/${encodeURIComponent(deviceId)}/photos?limit=${limit}&page=${page}`),
  );
}

export async function getPhotoReadUrl(
  fetcher: DashboardFetch,
  cloudApiOrigin: string,
  deviceId: string,
  photoId: string,
): Promise<string> {
  const result = await jsonRequest<{ url: string; expires_at: string }>(
    fetcher,
    apiUrl(cloudApiOrigin, `/v1/devices/${encodeURIComponent(deviceId)}/photos/${encodeURIComponent(photoId)}/read`),
  );
  return result.url;
}

export async function getNetworkLocations(
  fetcher: DashboardFetch,
  cloudApiOrigin: string,
): Promise<DashboardNetworkLocation[]> {
  const result = await jsonRequest<{ locations: DashboardNetworkLocation[] }>(
    fetcher,
    apiUrl(cloudApiOrigin, "/v1/network-locations"),
  );
  return result.locations;
}

export async function createNetworkLocation(
  fetcher: DashboardFetch,
  cloudApiOrigin: string,
  input: {
    name: string;
    match_ssid: string | null;
    match_bssid: string | null;
    country: string | null;
    region: string | null;
    city: string | null;
  },
): Promise<void> {
  await jsonRequest(fetcher, apiUrl(cloudApiOrigin, "/v1/network-locations"), {
    method: "POST",
    body: JSON.stringify(input),
  });
}

export async function deleteNetworkLocation(
  fetcher: DashboardFetch,
  cloudApiOrigin: string,
  locationId: string,
): Promise<void> {
  const response = await fetcher(apiUrl(cloudApiOrigin, `/v1/network-locations/${encodeURIComponent(locationId)}`), {
    method: "DELETE",
    credentials: "include",
  });
  if (response.status !== 204) throw await apiError(response);
}

export function cloudApiOrigin(): string {
  if (process.env.NODE_ENV === "production") return "";
  return process.env.NEXT_PUBLIC_CLOUD_API_ORIGIN ?? "";
}

export function pairingAuthorizePath(sessionId: string): string {
  return `/v1/device-pairing/sessions/${encodeURIComponent(sessionId)}/authorize`;
}

function apiUrl(cloudApiOrigin: string, path: string): string {
  return cloudApiOrigin.length === 0 ? path : new URL(path, cloudApiOrigin).toString();
}

async function jsonRequest<T>(
  fetcher: DashboardFetch,
  input: string,
  init?: RequestInit,
): Promise<T> {
  const response = await fetcher(input, {
    ...init,
    headers: { "content-type": "application/json", ...init?.headers },
    credentials: "include",
  });
  if (!response.ok) throw await apiError(response);
  return (await response.json()) as T;
}

async function apiError(response: Response): Promise<DashboardApiError> {
  const body = (await response.json().catch(() => null)) as DashboardApiErrorBody | null;
  return new DashboardApiError(
    body?.error?.error_code ?? "REQUEST_FAILED",
    body?.error?.message ?? "The request failed.",
  );
}
