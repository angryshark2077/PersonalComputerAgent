export interface CollectorConfig {
  network: { enabled: boolean };
  "communication.wechat": {
    enabled: boolean;
    direction: "outgoing";
    message_type: "text";
    sync_mode: "full";
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

export async function authorizePairing(
  fetcher: DashboardFetch,
  cloudApiOrigin: string,
  sessionId: string,
  callbackState: string,
): Promise<string> {
  const response = await fetcher(pairingAuthorizeUrl(cloudApiOrigin, sessionId), {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ callback_state: callbackState }),
    redirect: "manual",
    credentials: "include",
  });
  if (response.status !== 302) {
    throw await apiError(response);
  }
  const redirect = response.headers.get("location");
  if (redirect === null || !isPairingCallback(redirect)) {
    throw new DashboardApiError("PAIRING_CALLBACK_INVALID", "Invalid pairing callback.");
  }
  if (new URL(redirect).searchParams.get("state") !== callbackState) {
    throw new DashboardApiError("PAIRING_CALLBACK_INVALID", "Pairing callback state mismatch.");
  }
  return redirect;
}

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

function pairingAuthorizeUrl(cloudApiOrigin: string, sessionId: string): string {
  return apiUrl(cloudApiOrigin, `/v1/device-pairing/sessions/${encodeURIComponent(sessionId)}/authorize`);
}

function apiUrl(cloudApiOrigin: string, path: string): string {
  return cloudApiOrigin.length === 0 ? path : new URL(path, cloudApiOrigin).toString();
}

function isPairingCallback(value: string): boolean {
  try {
    const callback = new URL(value);
    const parameters = [...callback.searchParams.keys()].sort();
    return (
      callback.protocol === "http:" &&
      callback.hostname === "127.0.0.1" &&
      callback.port.length > 0 &&
      callback.pathname === "/pca/pair/callback" &&
      parameters.length === 2 &&
      parameters[0] === "code" &&
      parameters[1] === "state" &&
      callback.searchParams.get("code") !== null &&
      callback.searchParams.get("state") !== null
    );
  } catch {
    return false;
  }
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
