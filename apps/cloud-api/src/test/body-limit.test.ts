import assert from "node:assert/strict";
import test from "node:test";

import { MemoryControlRepository } from "@pca/db-cloud/src/repository.js";

import { createApp } from "../index.js";

test("rejects an oversized request before parsing or authentication", async () => {
  const api = createApp({ repository: new MemoryControlRepository() });
  const response = await api.request("/v1/device-pairing/sessions", {
    method: "POST",
    headers: {
      "content-length": String(100 * 1024 * 1024 + 1),
      "content-type": "application/json",
    },
    body: "{}",
  });

  assert.equal(response.status, 413);
  assert.deepEqual(await response.json(), {
    error: {
      error_code: "REQUEST_TOO_LARGE",
      message: "Request body exceeds the maximum allowed size.",
      retryable: false,
    },
  });
});
