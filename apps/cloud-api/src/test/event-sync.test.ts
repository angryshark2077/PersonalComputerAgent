import assert from "node:assert/strict";
import test from "node:test";

import { MemoryControlRepository } from "@pca/db-cloud/src/repository.js";

import { createApp, type OwnerPrincipal } from "../index.js";
import { pkceChallenge } from "../pairing.js";
import type { R2ObjectDescriptor, R2ObjectHead, R2ObjectStore } from "../r2.js";

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
      sender_id: "wxid_sender",
      sender_display_name: "Sender One",
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

function communicationSender(deviceId: string) {
  return {
    event_id: "01986666-7666-8666-8666-666666666671",
    workspace_id: owner.workspaceId,
    device_id: deviceId,
    event_type: "communication.message_sender_observed",
    source: "communication.wechat",
    schema_version: 1,
    occurred_at: "2026-08-02T00:00:00Z",
    created_at: "2026-08-02T00:00:00Z",
    sensitivity: "high",
    payload: {
      message_id: "message-1",
      source_key: "opaque-source-key-1",
      sender_id: "wxid_sender",
      sender_display_name: "Group Alias",
      avatar_url: "https://avatar.example/member.jpg",
      observed_at: "2026-08-02T00:00:00Z",
    },
    attachment_refs: [],
    idempotency_key: "message-sender-observed-1",
  };
}

function communicationConversation(deviceId: string) {
  return {
    event_id: "01986666-7666-8666-8666-666666666670",
    workspace_id: owner.workspaceId,
    device_id: deviceId,
    event_type: "communication.conversation_observed",
    source: "communication.wechat",
    schema_version: 1,
    occurred_at: "2026-08-02T00:00:00Z",
    created_at: "2026-08-02T00:00:00Z",
    sensitivity: "high",
    payload: {
      conversation_id: "conversation-1",
      display_name: "Ding Maiya",
      avatar_url: "https://avatar.example/conversation.jpg",
      observed_at: "2026-08-02T00:00:00Z",
      conversation: { scope: "direct" },
    },
    attachment_refs: [],
    idempotency_key: "conversation-observed-1",
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
      sender_id: "wxid_self",
      sender_display_name: "You",
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

function communicationFullImage(deviceId: string) {
  const event = communicationImage(deviceId);
  event.event_id = "01986666-7666-8666-8666-666666666672";
  event.created_at = "2026-08-02T00:02:00Z";
  event.payload.source_key = "opaque-source-key-2:full";
  event.payload.attachments = [{
    attachment_id: "attachment-1:full",
    kind: "image",
    sha256: "b".repeat(64),
    size_bytes: 4096,
    mime_type: "image/jpeg",
  }];
  event.attachment_refs = ["attachment-1:full"];
  event.idempotency_key = "opaque-source-key-2:full";
  return event;
}

class FakeR2ObjectStore implements R2ObjectStore {
  latest: R2ObjectDescriptor | null = null;
  uploaded = false;
  headOverride: R2ObjectHead | undefined;

  async signUpload(descriptor: R2ObjectDescriptor) {
    this.latest = descriptor;
    return {
      url: "https://private-media.example/upload",
      headers: { "content-type": descriptor.expectedMimeType },
    };
  }

  async headObject() {
    if (this.headOverride !== undefined) return this.headOverride;
    if (!this.uploaded || this.latest === null) return null;
    return {
      sizeBytes: this.latest.expectedSizeBytes,
      mimeType: this.latest.expectedMimeType,
      sha256: this.latest.expectedSha256,
    };
  }

  async deleteObject() {
    this.uploaded = false;
    this.headOverride = undefined;
  }

  async signRead() {
    return { url: "https://private-media.example/read", expiresAt: new Date("2026-08-02T00:06:00Z") };
  }
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
      events: [
        communicationConversation(credentials.device_id),
        communicationText(credentials.device_id),
        communicationSender(credentials.device_id),
      ],
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
  assert.deepEqual(body.accepted, [
    "01986666-7666-8666-8666-666666666670",
    "01986666-7666-8666-8666-666666666667",
    "01986666-7666-8666-8666-666666666671",
  ]);
  assert.deepEqual(body.duplicates, []);
  assert.deepEqual(body.rejected, []);
  assert.notEqual(Number.isNaN(Date.parse(body.server_time)), true);

  const duplicate = await api.request("/v1/agent/sync/communication/events", request);
  const duplicateBody = await duplicate.json() as { accepted: string[]; duplicates: string[] };
  assert.equal(duplicate.status, 200, JSON.stringify(duplicateBody));
  assert.deepEqual(
    duplicateBody.duplicates,
    [
      "01986666-7666-8666-8666-666666666670",
      "01986666-7666-8666-8666-666666666667",
      "01986666-7666-8666-8666-666666666671",
    ],
  );

  const wrongEndpoint = await api.request("/v1/agent/sync/events", request);
  assert.equal(wrongEndpoint.status, 400);
});

test("a later media projection replaces the earlier projection for the same message", async () => {
  const { api, credentials } = await pairedApi();
  for (const event of [communicationImage(credentials.device_id), communicationFullImage(credentials.device_id)]) {
    const response = await api.request("/v1/agent/sync/communication/events", {
      method: "POST",
      headers: {
        authorization: `Bearer ${credentials.device_access_token}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({
        batch_id: crypto.randomUUID(),
        device_id: credentials.device_id,
        protocol_version: 1,
        events: [event],
      }),
    });
    assert.equal(response.status, 200);
  }

  const response = await api.request(
    `/v1/devices/${credentials.device_id}/communication/conversations/conversation-1/messages`,
  );
  assert.equal(response.status, 200);
  const body = await response.json() as { messages: Array<{
    event_id: string;
    message_id: string;
    attachments: Array<{ attachment_id: string; size_bytes: number }>;
  }> };
  assert.equal(body.messages.length, 1);
  assert.equal(body.messages[0]?.event_id, "01986666-7666-8666-8666-666666666672");
  assert.equal(body.messages[0]?.message_id, "message-2");
  assert.deepEqual(body.messages[0]?.attachments, [{
    attachment_id: "attachment-1:full",
    kind: "image",
    sha256: "b".repeat(64),
    size_bytes: 4096,
    mime_type: "image/jpeg",
    object_id: null,
    object_state: null,
  }]);
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
      events: [
        communicationConversation(credentials.device_id),
        communicationText(credentials.device_id),
        communicationSender(credentials.device_id),
      ],
    }),
  };
  assert.equal((await api.request("/v1/agent/sync/communication/events", request)).status, 200);

  const conversations = await api.request(`/v1/devices/${credentials.device_id}/communication/conversations`);
  assert.equal(conversations.status, 200);
  assert.deepEqual(await conversations.json(), {
    conversations: [{
      conversation_id: "conversation-1",
      display_name: "Ding Maiya",
      avatar_url: "https://avatar.example/conversation.jpg",
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
      sender_id: "wxid_sender",
      sender_display_name: "Group Alias",
      sender_avatar_url: "https://avatar.example/member.jpg",
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

test("owner pages backward through communication messages without duplicates", async () => {
  const { api, credentials } = await pairedApi();
  const newer = communicationText(credentials.device_id);
  const older = communicationText(credentials.device_id);
  older.event_id = "01986666-7666-8666-8666-666666666665";
  older.occurred_at = "2026-08-01T23:59:00Z";
  older.created_at = "2026-08-01T23:59:00Z";
  older.payload = {
    ...older.payload,
    message_id: "message-older",
    source_key: "opaque-source-key-older",
    occurred_at: "2026-08-01T23:59:00Z",
    text: "older body",
  };
  older.idempotency_key = "opaque-source-key-older";
  const sync = await api.request("/v1/agent/sync/communication/events", {
    method: "POST",
    headers: {
      authorization: `Bearer ${credentials.device_access_token}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({
      batch_id: "01987777-7777-8777-8777-777777777790",
      device_id: credentials.device_id,
      protocol_version: 1,
      events: [communicationConversation(credentials.device_id), older, newer],
    }),
  });
  assert.equal(sync.status, 200);

  const first = await api.request(
    `/v1/devices/${credentials.device_id}/communication/conversations/conversation-1/messages?limit=1`,
  );
  const firstBody = await first.json() as { messages: Array<{ event_id: string; occurred_at: string }> };
  assert.deepEqual(firstBody.messages.map((message) => message.event_id), [newer.event_id]);

  const cursor = new URLSearchParams({
    limit: "1",
    before: firstBody.messages[0]!.occurred_at,
    before_event_id: firstBody.messages[0]!.event_id,
  });
  const second = await api.request(
    `/v1/devices/${credentials.device_id}/communication/conversations/conversation-1/messages?${cursor}`,
  );
  const secondBody = await second.json() as { messages: Array<{ event_id: string }> };
  assert.deepEqual(secondBody.messages.map((message) => message.event_id), [older.event_id]);

  assert.equal((await api.request(
    `/v1/devices/${credentials.device_id}/communication/conversations/conversation-1/messages?before=${encodeURIComponent(newer.occurred_at)}`,
  )).status, 400);
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
      sender_id: "wxid_self",
      sender_display_name: "You",
      sender_avatar_url: null,
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
        object_id: null,
        object_state: null,
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

test("only a completed attachment receives a short Owner read URL", async () => {
  const store = new FakeR2ObjectStore();
  const repository = new MemoryControlRepository([
    { workspaceId: owner.workspaceId, userId: owner.userId },
    { workspaceId: otherOwner.workspaceId, userId: otherOwner.userId },
  ]);
  const api = createApp({ repository, ownerAuthenticator: async () => owner, objectStore: store });
  const { credentials } = await pairedApiWith(api);
  assert.equal((await api.request("/v1/agent/sync/communication/events", {
    method: "POST",
    headers: { authorization: `Bearer ${credentials.device_access_token}`, "content-type": "application/json" },
    body: JSON.stringify({
      batch_id: "01987777-7777-8777-8777-777777777783",
      device_id: credentials.device_id,
      protocol_version: 1,
      events: [communicationImage(credentials.device_id)],
    }),
  })).status, 200);

  const prepared = await api.request("/v1/agent/communication/objects/prepare", {
    method: "POST",
    headers: { authorization: `Bearer ${credentials.device_access_token}`, "content-type": "application/json" },
    body: JSON.stringify({
      event_id: "01986666-7666-8666-8666-666666666668",
      attachment_id: "attachment-1",
    }),
  });
  const preparedBody = await prepared.json() as { object_id: string; state: string; upload: { url: string } };
  assert.equal(prepared.status, 200);
  assert.equal(preparedBody.state, "prepared");
  assert.equal(preparedBody.upload.url, "https://private-media.example/upload");
  assert.equal(
    (await api.request(`/v1/devices/${credentials.device_id}/communication/objects/${preparedBody.object_id}/read`)).status,
    404,
  );

  store.uploaded = true;
  store.headOverride = {
    sizeBytes: 1024,
    mimeType: "image/jpeg",
    sha256: "b".repeat(64),
  };
  assert.equal((await api.request("/v1/agent/communication/objects/complete", {
    method: "POST",
    headers: { authorization: `Bearer ${credentials.device_access_token}`, "content-type": "application/json" },
    body: JSON.stringify({ object_id: preparedBody.object_id }),
  })).status, 409);
  assert.equal(store.uploaded, false);
  store.uploaded = true;
  store.headOverride = undefined;
  const completed = await api.request("/v1/agent/communication/objects/complete", {
    method: "POST",
    headers: { authorization: `Bearer ${credentials.device_access_token}`, "content-type": "application/json" },
    body: JSON.stringify({ object_id: preparedBody.object_id }),
  });
  assert.deepEqual(await completed.json(), { object_id: preparedBody.object_id, state: "completed" });
  assert.deepEqual(
    await (await api.request(
      `/v1/devices/${credentials.device_id}/communication/conversations/conversation-1/messages`,
    )).json(),
    {
      messages: [{
        event_id: "01986666-7666-8666-8666-666666666668",
        message_id: "message-2",
        sender_id: "wxid_self",
        sender_display_name: "You",
        sender_avatar_url: null,
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
          object_id: preparedBody.object_id,
          object_state: "completed",
        }],
      }],
    },
  );
  assert.deepEqual(
    await (await api.request(
      `/v1/devices/${credentials.device_id}/communication/objects/${preparedBody.object_id}/read`,
    )).json(),
    { url: "https://private-media.example/read", expires_at: "2026-08-02T00:06:00.000Z" },
  );

  const otherApi = createApp({ repository, ownerAuthenticator: async () => otherOwner, objectStore: store });
  assert.equal(
    (await otherApi.request(
      `/v1/devices/${credentials.device_id}/communication/objects/${preparedBody.object_id}/read`,
    )).status,
    403,
  );
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
