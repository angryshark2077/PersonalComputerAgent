import assert from "node:assert/strict";
import test from "node:test";

import { createNextConfig, validateDashboardEnvironment } from "../next.config.ts";

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
