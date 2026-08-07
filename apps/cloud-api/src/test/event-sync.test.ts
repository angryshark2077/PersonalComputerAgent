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
  return { ...await pairedApiWith(api), repository };
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

function agentStarted(deviceId: string) {
  return {
    event_id: "01986666-7666-8666-8666-666666666669",
    workspace_id: owner.workspaceId,
    device_id: deviceId,
    event_type: "agent.started",
    source: "runtime.lifecycle",
    schema_version: 1,
    occurred_at: "2026-08-02T00:00:00Z",
    created_at: "2026-08-02T00:00:00Z",
    sensitivity: "normal",
    payload: {},
    attachment_refs: [],
    idempotency_key: "lifecycle:01986666-7666-8666-8666-666666666669",
  };
}

function networkOnline(deviceId: string) {
  return {
    ...agentStarted(deviceId),
    event_id: "01986666-7666-8666-8666-66666666666a",
    event_type: "network.online",
    idempotency_key: "lifecycle:01986666-7666-8666-8666-66666666666a",
  };
}

function networkChanged(deviceId: string) {
  return {
    ...agentStarted(deviceId),
    event_id: "01986666-7666-8666-8666-66666666666b",
    event_type: "network.changed",
    idempotency_key: "lifecycle:01986666-7666-8666-8666-66666666666b",
  };
}

function photoAsset(deviceId: string) {
  return {
    event_id: "01986666-7666-8666-8666-666666666691",
    workspace_id: owner.workspaceId,
    device_id: deviceId,
    event_type: "photos.asset_recorded",
    source: "photos.library",
    schema_version: 1,
    occurred_at: "2026-08-05T12:00:00Z",
    created_at: "2026-08-05T12:00:00Z",
    sensitivity: "high",
    payload: {
      asset_id: "photo-local-1",
      captured_at: "2026-08-05T12:00:00Z",
      media_type: "image",
      original_filename: "IMG_0001.HEIC",
      mime_type: "image/heic",
      pixel_width: 4032,
      pixel_height: 3024,
      duration_seconds: 0,
      album_names: ["Recent"],
    },
    attachment_refs: [],
    idempotency_key: "photos:asset:photo-local-1",
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

function appleMessagesText(deviceId: string) {
  const event = communicationText(deviceId);
  return {
    ...event,
    event_id: "01986666-7666-8666-8666-666666666674",
    source: "communication.messages",
    occurred_at: "2026-08-02T00:02:00Z",
    created_at: "2026-08-02T00:02:00Z",
    payload: {
      ...event.payload,
      message_id: "apple-message-1",
      conversation_id: "apple-conversation-1",
      sender_id: "+15550000000",
      sender_display_name: "Apple Contact",
      source_key: "apple-source-key-1",
      occurred_at: "2026-08-02T00:02:00Z",
      text: "apple private body",
    },
    idempotency_key: "apple-source-key-1",
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

  async listObjects() {
    return [];
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
  const lateThumbnail = communicationImage(credentials.device_id);
  lateThumbnail.event_id = "01986666-7666-8666-8666-666666666673";
  lateThumbnail.payload.source_key = "opaque-source-key-2:thumbnail-retry";
  lateThumbnail.idempotency_key = "opaque-source-key-2:thumbnail-retry";
  for (const event of [
    communicationImage(credentials.device_id),
    communicationFullImage(credentials.device_id),
    lateThumbnail,
  ]) {
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
    pagination: { page: 1, page_size: 50, total_count: 1, total_pages: 1 },
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

test("owner pages through every projected communication conversation", async () => {
  const { api, credentials } = await pairedApi();
  const events = Array.from({ length: 101 }, (_, index) => {
    const event = communicationText(credentials.device_id);
    const occurredAt = new Date(Date.UTC(2026, 7, 2, 0, index)).toISOString();
    const conversationId = `conversation-${String(index).padStart(3, "0")}`;
    return {
      ...event,
      event_id: `01986666-7666-8666-8666-${String(666_666_666_700 + index)}`,
      occurred_at: occurredAt,
      created_at: occurredAt,
      payload: {
        ...event.payload,
        message_id: `message-${index}`,
        conversation_id: conversationId,
        source_key: `source-key-${index}`,
        occurred_at: occurredAt,
      },
      idempotency_key: `source-key-${index}`,
    };
  });
  const sync = await api.request("/v1/agent/sync/communication/events", {
    method: "POST",
    headers: {
      authorization: `Bearer ${credentials.device_access_token}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({
      batch_id: "01987777-7777-8777-8777-777777777781",
      device_id: credentials.device_id,
      protocol_version: 1,
      events,
    }),
  });
  assert.equal(sync.status, 200);

  const first = await api.request(
    `/v1/devices/${credentials.device_id}/communication/conversations?limit=100&page=1`,
  );
  const second = await api.request(
    `/v1/devices/${credentials.device_id}/communication/conversations?limit=100&page=2`,
  );
  const firstBody = await first.json() as {
    conversations: Array<{ conversation_id: string }>;
    pagination: { page: number; page_size: number; total_count: number; total_pages: number };
  };
  const secondBody = await second.json() as {
    conversations: Array<{ conversation_id: string }>;
    pagination: { page: number; page_size: number; total_count: number; total_pages: number };
  };
  assert.equal(first.status, 200);
  assert.equal(second.status, 200);
  assert.deepEqual(firstBody.pagination, { page: 1, page_size: 100, total_count: 101, total_pages: 2 });
  assert.deepEqual(secondBody.pagination, { page: 2, page_size: 100, total_count: 101, total_pages: 2 });
  assert.equal(firstBody.conversations.length, 100);
  assert.deepEqual(secondBody.conversations.map((conversation) => conversation.conversation_id), ["conversation-000"]);
});

test("WeChat and Apple Messages are isolated by communication source", async () => {
  const { api, credentials } = await pairedApi();
  const sync = await api.request("/v1/agent/sync/communication/events", {
    method: "POST",
    headers: {
      authorization: `Bearer ${credentials.device_access_token}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({
      batch_id: "01987777-7777-8777-8777-777777777794",
      device_id: credentials.device_id,
      protocol_version: 1,
      events: [communicationText(credentials.device_id), appleMessagesText(credentials.device_id)],
    }),
  });
  assert.equal(sync.status, 200);

  const wechatConversations = await api.request(
    `/v1/devices/${credentials.device_id}/communication/conversations`,
  );
  const wechatBody = await wechatConversations.json() as {
    conversations: Array<{ conversation_id: string }>;
  };
  assert.deepEqual(wechatBody.conversations.map((item) => item.conversation_id), ["conversation-1"]);

  const messagesConversations = await api.request(
    `/v1/devices/${credentials.device_id}/communication/conversations?source=communication.messages`,
  );
  const messagesBody = await messagesConversations.json() as {
    conversations: Array<{ conversation_id: string }>;
  };
  assert.deepEqual(messagesBody.conversations.map((item) => item.conversation_id), ["apple-conversation-1"]);

  const wrongSource = await api.request(
    `/v1/devices/${credentials.device_id}/communication/conversations/apple-conversation-1/messages`,
  );
  assert.deepEqual(await wrongSource.json(), { messages: [] });

  const appleMessages = await api.request(
    `/v1/devices/${credentials.device_id}/communication/conversations/apple-conversation-1/messages?source=communication.messages`,
  );
  const appleMessagesBody = await appleMessages.json() as {
    messages: Array<{ message_id: string; text: string }>;
  };
  assert.deepEqual(appleMessagesBody.messages.map((item) => [item.message_id, item.text]), [
    ["apple-message-1", "apple private body"],
  ]);

  assert.equal((await api.request(
    `/v1/devices/${credentials.device_id}/communication/conversations?source=unknown`,
  )).status, 400);
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

test("a missing communication attachment has a dedicated prepare error", async () => {
  const store = new FakeR2ObjectStore();
  const { api, credentials } = await pairedApiWith(createApp({
    repository: new MemoryControlRepository([
      { workspaceId: owner.workspaceId, userId: owner.userId },
    ]),
    ownerAuthenticator: async () => owner,
    objectStore: store,
  }));
  const sync = await api.request("/v1/agent/sync/communication/events", {
    method: "POST",
    headers: {
      authorization: `Bearer ${credentials.device_access_token}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({
      batch_id: "01987777-7777-8777-8777-777777777784",
      device_id: credentials.device_id,
      protocol_version: 1,
      events: [communicationImage(credentials.device_id)],
    }),
  });
  assert.equal(sync.status, 200);

  const prepare = await api.request("/v1/agent/communication/objects/prepare", {
    method: "POST",
    headers: {
      authorization: `Bearer ${credentials.device_access_token}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({
      event_id: "01986666-7666-8666-8666-666666666668",
      attachment_id: "attachment-that-does-not-exist",
    }),
  });

  assert.equal(prepare.status, 404);
  assert.deepEqual(await prepare.json(), {
    error: {
      error_code: "COMMUNICATION_ATTACHMENT_NOT_FOUND",
      message: "The communication attachment no longer exists.",
      retryable: false,
    },
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

  const equalFidelityRetry = communicationImage(credentials.device_id);
  equalFidelityRetry.event_id = "01986666-7666-8666-8666-66666666667a";
  equalFidelityRetry.payload.source_key = "opaque-source-key-2:retry";
  equalFidelityRetry.idempotency_key = "opaque-source-key-2:retry";
  assert.equal((await api.request("/v1/agent/sync/communication/events", {
    method: "POST",
    headers: { authorization: `Bearer ${credentials.device_access_token}`, "content-type": "application/json" },
    body: JSON.stringify({
      batch_id: "01987777-7777-8777-8777-77777777779a",
      device_id: credentials.device_id,
      protocol_version: 1,
      events: [equalFidelityRetry],
    }),
  })).status, 200);
  const afterRetry = await (await api.request(
    `/v1/devices/${credentials.device_id}/communication/conversations/conversation-1/messages`,
  )).json() as { messages: Array<{ event_id: string; attachments: Array<{ object_id: string | null }> }> };
  assert.equal(afterRetry.messages[0]?.event_id, "01986666-7666-8666-8666-666666666668");
  assert.equal(afterRetry.messages[0]?.attachments[0]?.object_id, preparedBody.object_id);

  const otherApi = createApp({ repository, ownerAuthenticator: async () => otherOwner, objectStore: store });
  assert.equal(
    (await otherApi.request(
      `/v1/devices/${credentials.device_id}/communication/objects/${preparedBody.object_id}/read`,
    )).status,
    403,
  );
});

test("Owner manual screenshot request reaches the device and only a completed private JPEG is readable", async () => {
  const store = new FakeR2ObjectStore();
  const repository = new MemoryControlRepository([
    { workspaceId: owner.workspaceId, userId: owner.userId },
  ]);
  const api = createApp({ repository, ownerAuthenticator: async () => owner, objectStore: store });
  const { credentials } = await pairedApiWith(api);

  const queued = await api.request(`/v1/devices/${credentials.device_id}/screenshots`, { method: "POST" });
  assert.equal(queued.status, 202);
  const queuedBody = await queued.json() as { request: { request_id: string; status: string } };
  assert.equal(queuedBody.request.status, "queued");

  const control = await api.request("/v1/agent/control", {
    method: "POST",
    headers: {
      authorization: `Bearer ${credentials.device_access_token}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({
      heartbeat_id: "01985555-7555-8555-8555-555555555558",
      agent_version: "0.1.0",
      presence: "online",
      outbox_depth: 0,
      local_media: {
        completed_file_count: 0,
        completed_bytes: 0,
        protected_file_count: 0,
        protected_bytes: 0,
      },
      cleanup_result: null,
      network: null,
    }),
  });
  const controlBody = await control.json() as { snapshot: { screenshot_request: { request_id: string } } };
  assert.equal(controlBody.snapshot.screenshot_request.request_id, queuedBody.request.request_id);

  const screenshotId = "01986666-7666-8666-8666-666666666690";
  const prepared = await api.request("/v1/agent/screenshots/prepare", {
    method: "POST",
    headers: {
      authorization: `Bearer ${credentials.device_access_token}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({
      screenshot_id: screenshotId,
      request_id: queuedBody.request.request_id,
      trigger: "manual",
      captured_at: "2026-08-05T12:00:00.000Z",
      app_bundle_id: "com.google.Chrome",
      pixel_width: 1728,
      pixel_height: 1117,
      sha256: "c".repeat(64),
      size_bytes: 4096,
      mime_type: "image/jpeg",
    }),
  });
  assert.equal(prepared.status, 200);
  assert.equal((await prepared.json() as { state: string }).state, "prepared");
  assert.equal((await api.request(`/v1/devices/${credentials.device_id}/screenshots`)).status, 200);
  assert.deepEqual(await (await api.request(`/v1/devices/${credentials.device_id}/screenshots`)).json(), {
    screenshots: [],
    pagination: { page: 1, page_size: 50, total_count: 0, total_pages: 0 },
  });

  store.uploaded = true;
  assert.equal((await api.request("/v1/agent/screenshots/complete", {
    method: "POST",
    headers: {
      authorization: `Bearer ${credentials.device_access_token}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({ screenshot_id: screenshotId }),
  })).status, 200);

  const list = await api.request(`/v1/devices/${credentials.device_id}/screenshots`);
  const listBody = await list.json() as {
    screenshots: Array<{ screenshot_id: string; trigger: string }>;
    pagination: { page: number; total_count: number; total_pages: number };
  };
  assert.deepEqual(listBody.screenshots.map((item) => [item.screenshot_id, item.trigger]), [[screenshotId, "manual"]]);
  assert.deepEqual(listBody.pagination, { page: 1, page_size: 50, total_count: 1, total_pages: 1 });
  assert.deepEqual(
    await (await api.request(`/v1/devices/${credentials.device_id}/screenshots/${screenshotId}/read`)).json(),
    { url: "https://private-media.example/read", expires_at: "2026-08-02T00:06:00.000Z" },
  );
});

test("Owner paginates every completed screenshot by page number", async () => {
  const { api, credentials, repository } = await pairedApi();
  const createCompleted = async (screenshotId: string, capturedAt: string) => {
    await repository.prepareScreenshot(owner.workspaceId, credentials.device_id, {
      screenshotId,
      objectKey: `screenshots/${screenshotId}`,
      requestId: null,
      trigger: "activity",
      capturedAt: new Date(capturedAt),
      appBundleId: null,
      pixelWidth: 1728,
      pixelHeight: 1117,
      expectedSha256: "d".repeat(64),
      expectedSizeBytes: 4096,
      expectedMimeType: "image/jpeg",
      now: new Date(capturedAt),
    });
    await repository.completeScreenshot(owner.workspaceId, credentials.device_id, screenshotId, new Date(capturedAt));
  };

  await createCompleted("01986666-7666-8666-8666-666666666691", "2026-08-05T12:01:00.000Z");
  await createCompleted("01986666-7666-8666-8666-666666666692", "2026-08-05T12:02:00.000Z");
  await createCompleted("01986666-7666-8666-8666-666666666693", "2026-08-05T12:03:00.000Z");

  const first = await api.request(`/v1/devices/${credentials.device_id}/screenshots?limit=2&page=1`);
  assert.equal(first.status, 200);
  const firstBody = await first.json() as {
    screenshots: Array<{ screenshot_id: string }>;
    pagination: { page: number; page_size: number; total_count: number; total_pages: number };
  };
  assert.deepEqual(firstBody.screenshots.map((item) => item.screenshot_id), [
    "01986666-7666-8666-8666-666666666693",
    "01986666-7666-8666-8666-666666666692",
  ]);
  assert.deepEqual(firstBody.pagination, { page: 1, page_size: 2, total_count: 3, total_pages: 2 });

  const second = await api.request(`/v1/devices/${credentials.device_id}/screenshots?limit=2&page=2`);
  const secondBody = await second.json() as {
    screenshots: Array<{ screenshot_id: string }>;
    pagination: { page: number; page_size: number; total_count: number; total_pages: number };
  };
  assert.deepEqual(secondBody.screenshots.map((item) => item.screenshot_id), [
    "01986666-7666-8666-8666-666666666691",
  ]);
  assert.deepEqual(secondBody.pagination, { page: 2, page_size: 2, total_count: 3, total_pages: 2 });
});

test("Photo originals remain private, become readable only after completion, and have no expiry", async () => {
  const store = new FakeR2ObjectStore();
  const repository = new MemoryControlRepository([
    { workspaceId: owner.workspaceId, userId: owner.userId },
  ]);
  const api = createApp({ repository, ownerAuthenticator: async () => owner, objectStore: store });
  const { credentials } = await pairedApiWith(api);
  assert.equal((await api.request("/v1/agent/sync/events", {
    method: "POST",
    headers: { authorization: `Bearer ${credentials.device_access_token}`, "content-type": "application/json" },
    body: JSON.stringify({
      batch_id: "01987777-7777-8777-8777-777777777792",
      device_id: credentials.device_id,
      protocol_version: 1,
      events: [photoAsset(credentials.device_id)],
    }),
  })).status, 200);

  const photoId = "01986666-7666-8666-8666-666666666692";
  const prepared = await api.request("/v1/agent/photos/prepare", {
    method: "POST",
    headers: { authorization: `Bearer ${credentials.device_access_token}`, "content-type": "application/json" },
    body: JSON.stringify({
      photo_id: photoId,
      event_id: "01986666-7666-8666-8666-666666666691",
      asset_id: "photo-local-1",
      captured_at: "2026-08-05T12:00:00Z",
      media_type: "image",
      original_filename: "IMG_0001.HEIC",
      mime_type: "image/heic",
      pixel_width: 4032,
      pixel_height: 3024,
      duration_seconds: 0,
      album_names: ["Recent"],
      sha256: "d".repeat(64),
      size_bytes: 8192,
    }),
  });
  assert.equal(prepared.status, 200);
  assert.equal((await prepared.json() as { state: string }).state, "prepared");
  assert.deepEqual(await (await api.request(`/v1/devices/${credentials.device_id}/photos`)).json(), { photos: [] });

  store.uploaded = true;
  assert.equal((await api.request("/v1/agent/photos/complete", {
    method: "POST",
    headers: { authorization: `Bearer ${credentials.device_access_token}`, "content-type": "application/json" },
    body: JSON.stringify({ photo_id: photoId }),
  })).status, 200);
  const listed = await (await api.request(`/v1/devices/${credentials.device_id}/photos`)).json() as { photos: Array<{ photo_id: string; original_filename: string }> };
  assert.deepEqual(listed.photos.map((photo) => [photo.photo_id, photo.original_filename]), [[photoId, "IMG_0001.HEIC"]]);
  assert.deepEqual(
    await (await api.request(`/v1/devices/${credentials.device_id}/photos/${photoId}/read`)).json(),
    { url: "https://private-media.example/read", expires_at: "2026-08-02T00:06:00.000Z" },
  );
});

test("paired device uploads strict agent and network lifecycle events", async () => {
  const { api, credentials } = await pairedApi();
  const response = await api.request("/v1/agent/sync/events", {
    method: "POST",
    headers: {
      authorization: `Bearer ${credentials.device_access_token}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({
      batch_id: "01987777-7777-8777-8777-777777777776",
      device_id: credentials.device_id,
      protocol_version: 1,
      events: [
        agentStarted(credentials.device_id),
        networkOnline(credentials.device_id),
        networkChanged(credentials.device_id),
      ],
    }),
  });
  assert.equal(response.status, 200);
  assert.deepEqual((await response.json() as { accepted: string[] }).accepted, [
    "01986666-7666-8666-8666-666666666669",
    "01986666-7666-8666-8666-66666666666a",
    "01986666-7666-8666-8666-66666666666b",
  ]);

  const broadened = agentStarted(credentials.device_id);
  broadened.payload = { reason: "must remain local" };
  const rejected = await api.request("/v1/agent/sync/events", {
    method: "POST",
    headers: {
      authorization: `Bearer ${credentials.device_access_token}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({
      batch_id: "01987777-7777-8777-8777-777777777775",
      device_id: credentials.device_id,
      protocol_version: 1,
      events: [broadened],
    }),
  });
  assert.equal(rejected.status, 400);
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
