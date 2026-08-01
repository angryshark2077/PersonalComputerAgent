export interface DashboardEnvironment {
  CLOUD_API_INTERNAL_ORIGIN?: string | undefined;
  NEXT_PUBLIC_CLOUD_API_ORIGIN?: string | undefined;
}

export function validateDashboardEnvironment(environment: DashboardEnvironment): void {
  if (environment.NEXT_PUBLIC_CLOUD_API_ORIGIN) {
    throw new Error("Dashboard Cloud API requests must use the same-origin proxy.");
  }
}

export function validateInternalOrigin(
  internalOrigin: string,
): string {
  const privateOrigin = /^http:\/\/[a-z0-9-]+\.railway\.internal:([1-9]\d{0,4})$/i.exec(internalOrigin);
  const port = privateOrigin === null ? undefined : Number(privateOrigin[1]);
  if (port === undefined || port > 65535) {
    throw new Error("CLOUD_API_INTERNAL_ORIGIN must be an HTTP Railway private origin with an explicit valid port.");
  }

  let url: URL;
  try {
    url = new URL(internalOrigin);
  } catch {
    throw new Error("CLOUD_API_INTERNAL_ORIGIN must be an HTTP private origin.");
  }

  if (
    url.protocol !== "http:" ||
    url.hostname.length === 0 ||
    url.username.length > 0 ||
    url.password.length > 0 ||
    url.pathname !== "/" ||
    url.search.length > 0 ||
    url.hash.length > 0
  ) {
    throw new Error("CLOUD_API_INTERNAL_ORIGIN must be an HTTP Railway private origin with an explicit valid port.");
  }

  return url.origin;
}

export function requireBuildProxyOrigin(environment: NodeJS.ProcessEnv): string {
  if (!environment.CLOUD_API_INTERNAL_ORIGIN) {
    throw new Error("CLOUD_API_INTERNAL_ORIGIN is required for the production Dashboard build.");
  }

  return validateInternalOrigin(environment.CLOUD_API_INTERNAL_ORIGIN);
}

export const dashboardBuildProxyOrigin = process.env.DASHBOARD_BUILD_PROXY_ORIGIN;

export const dashboardBuildReadiness = { ready: dashboardBuildProxyOrigin !== undefined } as const;

export function dashboardHealthResponse(): Response {
  return Response.json({ status: "ok" });
}
