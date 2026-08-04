export interface CollectorConfig {
  network: { enabled: boolean };
  "communication.wechat": {
    enabled: boolean;
    directions: ["incoming", "outgoing"];
    message_types: ["text", "audio", "image", "video"] | ["text", "audio", "image", "video", "file"];
    conversation_scope: "direct_and_group_at_most_fifteen_members";
    max_group_members: 15;
    sync_mode: "full";
    retention_days: 180;
  };
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
    network: {
      interface_type: "wifi" | "wired" | "other" | "none";
      wifi_identity_available: boolean;
      ssid: string | null;
      bssid: string | null;
      local_ipv4: string | null;
      local_ipv6: string | null;
      public_ip: string | null;
      ip_location: { country: string | null; region: string | null; city: string | null; accuracy: "ip_city" } | null;
      matched_location: DashboardNetworkLocation | null;
    } | null;
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

export async function getDevice(
  fetcher: DashboardFetch,
  cloudApiOrigin: string,
  deviceId: string,
): Promise<DashboardDevice> {
  return jsonRequest(fetcher, apiUrl(cloudApiOrigin, `/v1/devices/${encodeURIComponent(deviceId)}`));
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
  limit = 100,
): Promise<DashboardConversation[]> {
  const result = await jsonRequest<{ conversations: DashboardConversation[] }>(
    fetcher,
    apiUrl(
      cloudApiOrigin,
      `/v1/devices/${encodeURIComponent(deviceId)}/communication/conversations?limit=${limit}`,
    ),
  );
  return result.conversations;
}

export async function getCommunicationMessages(
  fetcher: DashboardFetch,
  cloudApiOrigin: string,
  deviceId: string,
  conversationId: string,
  limit = 100,
  before?: Pick<DashboardMessage, "occurred_at" | "event_id">,
): Promise<DashboardMessage[]> {
  const query = new URLSearchParams({ limit: String(limit) });
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
