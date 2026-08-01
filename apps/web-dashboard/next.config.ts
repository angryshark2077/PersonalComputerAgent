import type { NextConfig } from "next";

export interface DashboardEnvironment {
  CLOUD_API_INTERNAL_ORIGIN?: string;
  NEXT_PUBLIC_CLOUD_API_ORIGIN?: string;
}

export function validateDashboardEnvironment(environment: DashboardEnvironment): void {
  if (environment.NEXT_PUBLIC_CLOUD_API_ORIGIN) {
    throw new Error("Dashboard Cloud API requests must use the same-origin proxy.");
  }
}

export function createNextConfig(
  internalOrigin: string | undefined,
  privateHosts: readonly string[] = [],
): NextConfig {
  if (internalOrigin === undefined || internalOrigin.length === 0) {
    return {};
  }

  const origin = validateInternalOrigin(internalOrigin, privateHosts);
  return {
    async rewrites() {
      return [
        { source: "/api/auth/:path*", destination: `${origin}/api/auth/:path*` },
        { source: "/v1/:path*", destination: `${origin}/v1/:path*` },
      ];
    },
  };
}

function validateInternalOrigin(internalOrigin: string, privateHosts: readonly string[]): string {
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

validateDashboardEnvironment(process.env);

export default createNextConfig(process.env.CLOUD_API_INTERNAL_ORIGIN);
