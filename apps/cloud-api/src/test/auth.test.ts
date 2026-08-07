import assert from "node:assert/strict";
import test from "node:test";

import { ControlRepositoryError, type ControlRepository } from "@pca/db-cloud/src/repository.js";
import { Hono } from "hono";

import { requireDevice } from "../auth.js";

test("a credential associated with a missing device is unauthorized", async () => {
  const repository = {
    async authenticateDeviceAccess() {
      throw new ControlRepositoryError("DEVICE_NOT_FOUND");
    },
  } as unknown as ControlRepository;
  const app = new Hono();
  app.get("/", async (context) => {
    const device = await requireDevice(context, repository, "access");
    return device instanceof Response ? device : context.json(device);
  });

  const response = await app.request("/", {
    headers: { authorization: "Bearer valid-shaped-token" },
  });

  assert.equal(response.status, 401);
  assert.deepEqual(await response.json(), {
    error: {
      error_code: "CREDENTIAL_INVALID",
      message: "The device credential is invalid.",
      retryable: false,
    },
  });
});
