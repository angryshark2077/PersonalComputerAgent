import assert from "node:assert/strict";
import test from "node:test";

import { MemoryControlRepository } from "@pca/db-cloud/src/repository.js";

import { createApp, type OwnerPrincipal } from "../index.js";
import { pkceChallenge } from "../pairing.js";

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
      device_public_key: "device-public-key-event-sync",
      code_challenge: pkceChallenge("verifier-event-sync"),
      callback_uri: "http://127.0.0.1:43123/pca/pair/callback",
      callback_state: "1234567890123456789012345678901234567890123",
    }),
  });
  const { session_id: sessionId } = (await start.json()) as { session_id: string };
  const authorized = await api.request(`/v1/device-pairing/sessions/${sessionId}/authorize`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ callback_state: "1234567890123456789012345678901234567890123" }),
  });
  const authorizationCode = new URL(authorized.headers.get("location") ?? "").searchParams.get("code");
  const exchange = await api.request("/v1/device-pairing/exchange", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      session_id: sessionId,
      authorization_code: authorizationCode,
      code_verifier: "verifier-event-sync",
    }),
  });
  const credentials = (await exchange.json()) as {
    device_id: string;
    device_access_token: string;
  };
  return { api, credentials };
}

function systemMetric(deviceId: string) {
  return {
    event_id: "01986666-7666-8666-8666-666666666666",
    workspace_id: owner.workspaceId,
    device_id: deviceId,
    event_type: "system.metric_sampled",
    source: "system",
    schema_version: 1,
    occurred_at: "2026-08-02T00:00:00Z",
    created_at: "2026-08-02T00:00:00Z",
    sensitivity: "normal",
    payload: {
      metric_group: "cpu_memory",
      sample_window_ms: 30_000,
      logical_cpu_count: 10,
      host: { cpu_usage_percent: 12.34, memory_total_bytes: 34_359_738_368, memory_used_bytes: 17_179_869_184 },
      agent: { cpu_usage_percent: 0.42, memory_resident_bytes: 73_400_320 },
    },
    attachment_refs: [],
    idempotency_key: "system:cpu-memory:01986666-7666-8666-8666-666666666666",
  };
}

test("paired device uploads one strict system metric idempotently and its owner can read it", async () => {
  const { api, credentials } = await pairedApi();
  const request = {
    method: "POST",
    headers: {
      authorization: `Bearer ${credentials.device_access_token}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({
      batch_id: "01987777-7777-8777-8777-777777777777",
      device_id: credentials.device_id,
      protocol_version: 1,
      events: [systemMetric(credentials.device_id)],
    }),
  };

  const first = await api.request("/v1/agent/sync/events", request);
  assert.equal(first.status, 200);
  const firstBody = await first.json() as {
    batch_id: string;
    accepted: string[];
    duplicates: string[];
    rejected: unknown[];
    server_time: string;
  };
  assert.equal(firstBody.batch_id, "01987777-7777-8777-8777-777777777777");
  assert.deepEqual(firstBody.accepted, ["01986666-7666-8666-8666-666666666666"]);
  assert.deepEqual(firstBody.duplicates, []);
  assert.deepEqual(firstBody.rejected, []);
  assert.notEqual(Number.isNaN(Date.parse(firstBody.server_time)), true);

  const duplicate = await api.request("/v1/agent/sync/events", request);
  assert.equal(duplicate.status, 200);
  assert.deepEqual((await duplicate.json() as { accepted: string[]; duplicates: string[] }).duplicates,
    ["01986666-7666-8666-8666-666666666666"]);

  const metrics = await api.request(`/v1/devices/${credentials.device_id}/system-metrics`);
  assert.equal(metrics.status, 200);
  assert.deepEqual(await metrics.json(), {
    metrics: [
      {
        event_id: "01986666-7666-8666-8666-666666666666",
        occurred_at: "2026-08-02T00:00:00.000Z",
        metric_group: "cpu_memory",
        payload: systemMetric(credentials.device_id).payload,
      },
    ],
  });
});

test("device event ingestion rejects an identity mismatch", async () => {
  const { api, credentials } = await pairedApi();
  const event = systemMetric(credentials.device_id);
  event.workspace_id = "01989999-7999-8999-8999-999999999999";

  const response = await api.request("/v1/agent/sync/events", {
    method: "POST",
    headers: {
      authorization: `Bearer ${credentials.device_access_token}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({
      batch_id: "01987777-7777-8777-8777-777777777778",
      device_id: credentials.device_id,
      protocol_version: 1,
      events: [event],
    }),
  });
  assert.equal(response.status, 403);
  assert.equal((await response.json() as { error: { error_code: string } }).error.error_code, "WORKSPACE_FORBIDDEN");
});
