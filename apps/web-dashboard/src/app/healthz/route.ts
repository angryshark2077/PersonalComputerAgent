import { dashboardHealthResponse } from "../../lib/railway-environment";

export const dynamic = "force-dynamic";

export function GET(): Response {
  return dashboardHealthResponse({
    CLOUD_API_INTERNAL_ORIGIN: process.env.CLOUD_API_INTERNAL_ORIGIN,
    NEXT_PUBLIC_CLOUD_API_ORIGIN: process.env.NEXT_PUBLIC_CLOUD_API_ORIGIN,
  });
}
