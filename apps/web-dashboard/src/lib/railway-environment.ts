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
  privateHosts: readonly string[] = [],
): string {
  let url: URL;
  try {
    url = new URL(internalOrigin);
  } catch {
    throw new Error("CLOUD_API_INTERNAL_ORIGIN must be an HTTP private origin.");
  }

  if (
    url.protocol !== "http:" ||
    url.hostname.length === 0 ||
    (!url.hostname.endsWith(".railway.internal") && !privateHosts.includes(url.hostname)) ||
    url.pathname !== "/" ||
    url.search.length > 0 ||
    url.hash.length > 0
  ) {
    throw new Error("CLOUD_API_INTERNAL_ORIGIN must be an HTTP private origin without a path, query, or fragment.");
  }

  return url.origin;
}

export function requireBuildProxyOrigin(environment: NodeJS.ProcessEnv): string {
  if (!environment.CLOUD_API_INTERNAL_ORIGIN) {
    throw new Error("CLOUD_API_INTERNAL_ORIGIN is required for the production Dashboard build.");
  }

  return validateInternalOrigin(environment.CLOUD_API_INTERNAL_ORIGIN);
}

export const dashboardBuildProxyOrigin =
  process.env.NODE_ENV === "production" ? requireBuildProxyOrigin(process.env) : undefined;

export const dashboardBuildReadiness = { ready: true } as const;

export function dashboardHealthResponse(): Response {
  return Response.json({ status: "ok" });
}
