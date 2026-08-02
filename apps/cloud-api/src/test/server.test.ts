import assert from "node:assert/strict";
import test from "node:test";

import { createProductionApp } from "../index.js";
import { parseListenPort } from "../server.js";

function validProductionEnvironment() {
  return {
    DATABASE_URL: "postgresql://pca-test@127.0.0.1:5432/pca_test",
    BETTER_AUTH_SECRET: "test-secret-that-is-long-enough-to-be-valid",
    BETTER_AUTH_URL: "http://localhost:3000",
    R2_ENDPOINT: "https://example.r2.cloudflarestorage.com",
    R2_ACCESS_KEY_ID: "test-access-key",
    R2_SECRET_ACCESS_KEY: "test-secret-key",
    R2_BUCKET: "private-media",
    R2_BUCKET_PUBLIC: "false",
  };
}

test("healthz is public and contains no deployment configuration", async () => {
  const app = createProductionApp(validProductionEnvironment());
  const response = await app.request("http://pca.invalid/healthz");
  assert.equal(response.status, 200);
  assert.deepEqual(await response.json(), { status: "ok" });
});

test("server requires a numeric Railway port", () => {
  assert.throws(() => parseListenPort("invalid"), /PORT/);
});
