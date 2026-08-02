export interface CollectorConfig {
  network: { enabled: boolean };
  "communication.wechat": {
    enabled: boolean;
    directions: ["incoming", "outgoing"];
    message_types: ["text", "audio", "image", "video"];
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
    observed_at: string;
  } | null;
  collectors: CollectorConfig;
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
  kind: "audio" | "image" | "video";
  sha256: string;
  size_bytes: number;
  mime_type: string;
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
  kind: "text" | "audio" | "image" | "video";
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

export function initializeChatReadAt(lastMessageAt: string, storedReadAt: string | null): string {
  return storedReadAt ?? lastMessageAt;
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
