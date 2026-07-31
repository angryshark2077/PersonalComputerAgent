import assert from "node:assert/strict";
import test from "node:test";

import { MemoryControlRepository } from "@pca/db-cloud/src/repository.js";

import { createApp, type OwnerPrincipal } from "../index.js";

const owner: OwnerPrincipal = {
  userId: "01983333-7333-8333-8333-333333333333",
  workspaceId: "01982222-7222-8222-8222-222222222222",
};

async function pairedApi() {
  const repository = new MemoryControlRepository([
    { workspaceId: owner.workspaceId, userId: owner.userId },
  ]);
  const api = createApp({ repository, ownerAuthenticator: async () => owner });
  const start = await api.request("/v1/device-pairing/sessions", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      device_public_key: "device-public-key-control",
      code_challenge: "verifier-control",
      callback_uri: "http://127.0.0.1:43123/pca/pair/callback",
    }),
  });
  const { session_id: sessionId } = (await start.json()) as { session_id: string };
  const authorized = await api.request(
    `/v1/device-pairing/sessions/${sessionId}/authorize`,
    { method: "POST" },
  );
  const code = new URL(authorized.headers.get("location") ?? "").searchParams.get("code");
  const exchange = await api.request("/v1/device-pairing/exchange", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      session_id: sessionId,
      authorization_code: code,
      code_verifier: "verifier-control",
    }),
  });
  const credentials = (await exchange.json()) as {
    device_id: string;
    device_access_token: string;
    refresh_token: string;
  };
  return { api, credentials, repository };
}

test("owner config is scoped, strict, and reaches device control", async () => {
  const { api, credentials } = await pairedApi();
  const config = await api.request(
    `/v1/devices/${credentials.device_id}/collector-config`,
    {
      method: "PUT",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        network: { enabled: true },
        "communication.wechat": {
          enabled: true,
          direction: "outgoing",
          message_type: "text",
          sync_mode: "full",
        },
      }),
    },
  );
  assert.equal(config.status, 200);
  assert.deepEqual(await config.json(), { configuration_revision: 1 });

  const control = await api.request("/v1/agent/control", {
    method: "POST",
    headers: {
      authorization: `Bearer ${credentials.device_access_token}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({
      heartbeat_id: "01985555-7555-8555-8555-555555555555",
      agent_version: "0.1.0",
      presence: "online",
      outbox_depth: 0,
    }),
  });
  assert.equal(control.status, 200);
  const result = (await control.json()) as {
    snapshot: { configuration_revision: number; collectors: { network: { enabled: boolean } } };
    server_time: string;
  };
  assert.equal(result.snapshot.configuration_revision, 1);
  assert.equal(result.snapshot.collectors.network.enabled, true);
  assert.notEqual(Number.isNaN(Date.parse(result.server_time)), true);

  const badScope = await api.request(
    `/v1/devices/${credentials.device_id}/collector-config`,
    {
      method: "PUT",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ network: { enabled: true }, extra: true }),
    },
  );
  assert.equal(badScope.status, 400);
});

test("refresh rotates credentials and a revoked device is rejected", async () => {
  const { api, credentials } = await pairedApi();
  const refresh = await api.request("/v1/devices/token/refresh", {
    method: "POST",
    headers: { authorization: `Bearer ${credentials.refresh_token}` },
  });
  assert.equal(refresh.status, 200);
  const rotated = (await refresh.json()) as {
    device_access_token: string;
    refresh_token: string;
  };
  const replay = await api.request("/v1/devices/token/refresh", {
    method: "POST",
    headers: { authorization: `Bearer ${credentials.refresh_token}` },
  });
  assert.equal(replay.status, 401);

  const revoked = await api.request(`/v1/devices/${credentials.device_id}/revoke`, {
    method: "POST",
  });
  assert.equal(revoked.status, 204);
  const control = await api.request("/v1/agent/control", {
    method: "POST",
    headers: {
      authorization: `Bearer ${rotated.device_access_token}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({
      heartbeat_id: "01985555-7555-8555-8555-555555555555",
      agent_version: "0.1.0",
      presence: "online",
      outbox_depth: 0,
    }),
  });
  assert.equal(control.status, 401);
});

test("owner endpoints cannot cross Workspace boundaries", async () => {
  const { api, credentials, repository } = await pairedApi();
  const otherWorkspace = createApp({
    repository,
    ownerAuthenticator: async () => ({
      userId: "01987777-7777-8777-8777-777777777777",
      workspaceId: "01989999-7999-8999-8999-999999999999",
    }),
  });
  const result = await otherWorkspace.request(
    `/v1/devices/${credentials.device_id}/collector-config`,
    {
      method: "PUT",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        network: { enabled: true },
        "communication.wechat": {
          enabled: false,
          direction: "outgoing",
          message_type: "text",
          sync_mode: "full",
        },
      }),
    },
  );
  assert.equal(result.status, 403);
  assert.equal((await result.json() as { error: { error_code: string } }).error.error_code, "WORKSPACE_FORBIDDEN");
  assert.notEqual(api, otherWorkspace);
});
