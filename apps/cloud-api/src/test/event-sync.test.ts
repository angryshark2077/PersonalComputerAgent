import assert from "node:assert/strict";
import test from "node:test";

import { MemoryControlRepository } from "@pca/db-cloud/src/repository.js";

import { createApp, type OwnerPrincipal } from "../index.js";
import { pkceChallenge } from "../pairing.js";

const owner: OwnerPrincipal = {
  userId: "01983333-7333-8333-8333-333333333333",
  workspaceId: "01982222-7222-8222-8222-222222222222",
};

const otherOwner: OwnerPrincipal = {
  userId: "01983333-7333-8333-8333-333333333334",
  workspaceId: "01982222-7222-8222-8222-222222222223",
};

async function pairedApi() {
  const repository = new MemoryControlRepository([
    { workspaceId: owner.workspaceId, userId: owner.userId },
  ]);
  const api = createApp({ repository, ownerAuthenticator: async () => owner });
  return pairedApiWith(api);
}

async function pairedApiWith(api: ReturnType<typeof createApp>) {
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

function communicationText(deviceId: string) {
  return {
    event_id: "01986666-7666-8666-8666-666666666667",
    workspace_id: owner.workspaceId,
    device_id: deviceId,
    event_type: "communication.message_recorded",
    source: "communication.wechat",
    schema_version: 1,
    occurred_at: "2026-08-02T00:00:00Z",
    created_at: "2026-08-02T00:00:00Z",
    sensitivity: "high",
    payload: {
      message_id: "message-1",
      conversation_id: "conversation-1",
      source_key: "opaque-source-key-1",
      occurred_at: "2026-08-02T00:00:00Z",
      direction: "incoming",
      kind: "text",
      conversation: { scope: "direct" },
      text: "private body",
    },
    attachment_refs: [],
    idempotency_key: "opaque-source-key-1",
  };
}

function communicationImage(deviceId: string) {
  return {
    event_id: "01986666-7666-8666-8666-666666666668",
    workspace_id: owner.workspaceId,
    device_id: deviceId,
    event_type: "communication.message_recorded",
    source: "communication.wechat",
    schema_version: 1,
    occurred_at: "2026-08-02T00:01:00Z",
    created_at: "2026-08-02T00:01:00Z",
    sensitivity: "high",
    payload: {
      message_id: "message-2",
      conversation_id: "conversation-1",
      source_key: "opaque-source-key-2",
      occurred_at: "2026-08-02T00:01:00Z",
      direction: "outgoing",
      kind: "image",
      conversation: { scope: "group", member_count: 3 },
      attachments: [{
        attachment_id: "attachment-1",
        kind: "image",
        sha256: "a".repeat(64),
        size_bytes: 1024,
        mime_type: "image/jpeg",
      }],
    },
    attachment_refs: ["attachment-1"],
    idempotency_key: "opaque-source-key-2",
  };
}

test("paired device syncs a private communication event only through its dedicated endpoint", async () => {
  const { api, credentials } = await pairedApi();
  const request = {
    method: "POST",
    headers: {
      authorization: `Bearer ${credentials.device_access_token}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({
      batch_id: "01987777-7777-8777-8777-777777777779",
      device_id: credentials.device_id,
      protocol_version: 1,
      events: [communicationText(credentials.device_id)],
    }),
  };

  const first = await api.request("/v1/agent/sync/communication/events", request);
  assert.equal(first.status, 200);
  const body = await first.json() as {
    batch_id: string;
    accepted: string[];
    duplicates: string[];
    rejected: unknown[];
    server_time: string;
  };
  assert.equal(body.batch_id, "01987777-7777-8777-8777-777777777779");
  assert.deepEqual(body.accepted, ["01986666-7666-8666-8666-666666666667"]);
  assert.deepEqual(body.duplicates, []);
  assert.deepEqual(body.rejected, []);
  assert.notEqual(Number.isNaN(Date.parse(body.server_time)), true);

  const duplicate = await api.request("/v1/agent/sync/communication/events", request);
  const duplicateBody = await duplicate.json() as { accepted: string[]; duplicates: string[] };
  assert.equal(duplicate.status, 200, JSON.stringify(duplicateBody));
  assert.deepEqual(
    duplicateBody.duplicates,
    ["01986666-7666-8666-8666-666666666667"],
  );

  const wrongEndpoint = await api.request("/v1/agent/sync/events", request);
  assert.equal(wrongEndpoint.status, 400);
});

test("only the device owner can read projected communication conversations and messages", async () => {
  const repository = new MemoryControlRepository([
    { workspaceId: owner.workspaceId, userId: owner.userId },
    { workspaceId: otherOwner.workspaceId, userId: otherOwner.userId },
  ]);
  const api = createApp({ repository, ownerAuthenticator: async () => owner });
  const { credentials } = await pairedApiWith(api);
  const request = {
    method: "POST",
    headers: {
      authorization: `Bearer ${credentials.device_access_token}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({
      batch_id: "01987777-7777-8777-8777-777777777780",
      device_id: credentials.device_id,
      protocol_version: 1,
      events: [communicationText(credentials.device_id)],
    }),
  };
  assert.equal((await api.request("/v1/agent/sync/communication/events", request)).status, 200);

  const conversations = await api.request(`/v1/devices/${credentials.device_id}/communication/conversations`);
  assert.equal(conversations.status, 200);
  assert.deepEqual(await conversations.json(), {
    conversations: [{
      conversation_id: "conversation-1",
      scope: "direct",
      member_count: null,
      message_count: 1,
      last_message_at: "2026-08-02T00:00:00.000Z",
    }],
  });

  const messages = await api.request(
    `/v1/devices/${credentials.device_id}/communication/conversations/conversation-1/messages`,
  );
  assert.equal(messages.status, 200);
  assert.deepEqual(await messages.json(), {
    messages: [{
      event_id: "01986666-7666-8666-8666-666666666667",
      message_id: "message-1",
      occurred_at: "2026-08-02T00:00:00.000Z",
      direction: "incoming",
      kind: "text",
      text: "private body",
      attachments: [],
    }],
  });

  const otherApi = createApp({ repository, ownerAuthenticator: async () => otherOwner });
  assert.equal(
    (await otherApi.request(`/v1/devices/${credentials.device_id}/communication/conversations`)).status,
    403,
  );
});

test("communication attachment manifests are projected without exposing object access", async () => {
  const { api, credentials } = await pairedApi();
  const response = await api.request("/v1/agent/sync/communication/events", {
    method: "POST",
    headers: {
      authorization: `Bearer ${credentials.device_access_token}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({
      batch_id: "01987777-7777-8777-8777-777777777781",
      device_id: credentials.device_id,
      protocol_version: 1,
      events: [communicationImage(credentials.device_id)],
    }),
  });
  assert.equal(response.status, 200);

  const messages = await api.request(
    `/v1/devices/${credentials.device_id}/communication/conversations/conversation-1/messages`,
  );
  assert.deepEqual(await messages.json(), {
    messages: [{
      event_id: "01986666-7666-8666-8666-666666666668",
      message_id: "message-2",
      occurred_at: "2026-08-02T00:01:00.000Z",
      direction: "outgoing",
      kind: "image",
      text: null,
      attachments: [{
        attachment_id: "attachment-1",
        kind: "image",
        sha256: "a".repeat(64),
        size_bytes: 1024,
        mime_type: "image/jpeg",
      }],
    }],
  });
});

test("communication sync rejects an idempotency key that is not the opaque source key", async () => {
  const { api, credentials } = await pairedApi();
  const event = communicationText(credentials.device_id);
  event.idempotency_key = "different-key";
  const response = await api.request("/v1/agent/sync/communication/events", {
    method: "POST",
    headers: {
      authorization: `Bearer ${credentials.device_access_token}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({
      batch_id: "01987777-7777-8777-8777-777777777782",
      device_id: credentials.device_id,
      protocol_version: 1,
      events: [event],
    }),
  });
  assert.equal(response.status, 400);
});

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
