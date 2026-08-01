import type { NextConfig } from "next";

import {
  requireBuildProxyOrigin,
  validateDashboardEnvironment,
  validateInternalOrigin,
} from "./src/lib/railway-environment.ts";

export function createNextConfig(
  internalOrigin: string,
): NextConfig {
  const origin = validateInternalOrigin(internalOrigin);
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

const buildProxyOrigin =
  process.env.NODE_ENV === "production" ? requireBuildProxyOrigin(process.env) : undefined;

export default buildProxyOrigin === undefined ? {} : createNextConfig(buildProxyOrigin);
