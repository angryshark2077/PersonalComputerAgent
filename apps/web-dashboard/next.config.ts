import type { NextConfig } from "next";

import {
  validateDashboardEnvironment,
  validateInternalOrigin,
} from "./src/lib/railway-environment.ts";

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

validateDashboardEnvironment(process.env);

export default createNextConfig(process.env.CLOUD_API_INTERNAL_ORIGIN);
