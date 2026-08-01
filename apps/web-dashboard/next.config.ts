import type { NextConfig } from "next";
import { PHASE_PRODUCTION_BUILD } from "next/constants.js";

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
    env: {
      DASHBOARD_BUILD_PROXY_ORIGIN: origin,
    },
    async rewrites() {
      return [
        { source: "/api/auth/:path*", destination: `${origin}/api/auth/:path*` },
        { source: "/v1/:path*", destination: `${origin}/v1/:path*` },
      ];
    },
  };
}

validateDashboardEnvironment(process.env);

export default function dashboardConfig(phase: string): NextConfig {
  if (phase !== PHASE_PRODUCTION_BUILD) {
    return {};
  }

  return createNextConfig(requireBuildProxyOrigin(process.env));
}
