import { dashboardBuildReadiness, dashboardHealthResponse } from "../../lib/railway-environment";

export const dynamic = "force-dynamic";

export function GET(): Response {
  return dashboardBuildReadiness.ready
    ? dashboardHealthResponse()
    : Response.json({ status: "not_ready" }, { status: 503 });
}
