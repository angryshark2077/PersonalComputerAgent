export interface DashboardEnvironment {
  CLOUD_API_INTERNAL_ORIGIN?: string | undefined;
  NEXT_PUBLIC_CLOUD_API_ORIGIN?: string | undefined;
}

export type DashboardReadiness =
  | { ready: true }
  | { ready: false; error: string };

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

export function dashboardReadiness(environment: DashboardEnvironment): DashboardReadiness {
  try {
    validateDashboardEnvironment(environment);
    if (!environment.CLOUD_API_INTERNAL_ORIGIN) {
      return { ready: false, error: "CLOUD_API_INTERNAL_ORIGIN is required at runtime." };
    }
    validateInternalOrigin(environment.CLOUD_API_INTERNAL_ORIGIN);
    return { ready: true };
  } catch (error) {
    return {
      ready: false,
      error: error instanceof Error ? error.message : "Dashboard configuration is invalid.",
    };
  }
}

export function dashboardHealthResponse(environment: DashboardEnvironment): Response {
  const readiness = dashboardReadiness(environment);
  return readiness.ready
    ? Response.json({ status: "ok" })
    : Response.json({ status: "not_ready" }, { status: 503 });
}
