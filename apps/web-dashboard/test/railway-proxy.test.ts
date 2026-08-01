import assert from "node:assert/strict";
import test from "node:test";

import { createNextConfig } from "../next.config.ts";
import {
  dashboardHealthResponse,
  dashboardReadiness,
  validateDashboardEnvironment,
} from "../src/lib/railway-environment.ts";

test("Railway proxy rewrites auth and control paths to the private API", async () => {
  const config = await createNextConfig("http://pca-cloud-api.railway.internal:8080");

  assert.deepEqual(await config.rewrites(), [
    {
      source: "/api/auth/:path*",
      destination: "http://pca-cloud-api.railway.internal:8080/api/auth/:path*",
    },
    {
      source: "/v1/:path*",
      destination: "http://pca-cloud-api.railway.internal:8080/v1/:path*",
    },
  ]);
});

test("Dashboard rejects a public browser Cloud API origin", () => {
  assert.throws(
    () => validateDashboardEnvironment({ NEXT_PUBLIC_CLOUD_API_ORIGIN: "https://api.invalid" }),
    /same-origin/,
  );
});

test("Dashboard runtime readiness fails closed without the private API origin", async () => {
  assert.deepEqual(dashboardReadiness({}), {
    ready: false,
    error: "CLOUD_API_INTERNAL_ORIGIN is required at runtime.",
  });
  assert.deepEqual(
    dashboardReadiness({
      CLOUD_API_INTERNAL_ORIGIN: "http://pca-cloud-api.railway.internal:8080",
    }),
    { ready: true },
  );
  const missing = dashboardHealthResponse({});
  assert.equal(missing.status, 503);
  assert.deepEqual(await missing.json(), { status: "not_ready" });
  const ready = dashboardHealthResponse({
    CLOUD_API_INTERNAL_ORIGIN: "http://pca-cloud-api.railway.internal:8080",
  });
  assert.equal(ready.status, 200);
  assert.deepEqual(await ready.json(), { status: "ok" });
});
