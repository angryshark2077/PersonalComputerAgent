import assert from "node:assert/strict";
import test from "node:test";

import {
  decodeDashboardRouteParam,
  chatReadBaselineStorageKey,
  chatReadStorageKey,
  getCommunicationConversations,
  getCommunicationMessages,
  getCommunicationObjectReadUrl,
  initializeChatReadAt,
  isConversationUnread,
} from "../src/lib/api.ts";

test("decodes an encoded WeChat group identity exactly once", () => {
  assert.equal(decodeDashboardRouteParam("room%40chatroom"), "room@chatroom");
  assert.equal(decodeDashboardRouteParam("room@chatroom"), "room@chatroom");
});

test("chat unread state is isolated by device and conversation", () => {
  assert.equal(chatReadBaselineStorageKey("device-a"), "pca.chat-read-baseline:device-a");
  assert.equal(chatReadStorageKey("device-a", "room@chatroom"), "pca.chat-read:device-a:room@chatroom");
  assert.equal(initializeChatReadAt("2026-08-02T10:00:00Z", null), "2026-08-02T10:00:00Z");
  assert.equal(
    initializeChatReadAt("2026-08-02T10:00:00Z", "2026-08-02T09:00:00Z"),
    "2026-08-02T09:00:00Z",
  );
  assert.equal(initializeChatReadAt("2026-08-02T10:00:00Z", null, true), null);
  assert.equal(isConversationUnread("2026-08-02T10:00:00Z", null), true);
  assert.equal(isConversationUnread("2026-08-02T10:00:00Z", "2026-08-02T09:00:00Z"), true);
  assert.equal(isConversationUnread("2026-08-02T10:00:00Z", "2026-08-02T10:00:00Z"), false);
});

test("loads owner-scoped conversations for one device", async () => {
  let requested = "";
  const conversations = await getCommunicationConversations(async (input) => {
    requested = String(input);
    return new Response(JSON.stringify({ conversations: [] }), { status: 200 });
  }, "https://cloud.example", "device/id", 25);

  assert.deepEqual(conversations, []);
  assert.equal(
    requested,
    "https://cloud.example/v1/devices/device%2Fid/communication/conversations?limit=25",
  );
});

test("loads owner-scoped messages with encoded conversation identity", async () => {
  let requested = "";
  const messages = await getCommunicationMessages(async (input) => {
    requested = String(input);
    return new Response(JSON.stringify({ messages: [] }), { status: 200 });
  }, "https://cloud.example", "device", "room/name", 50);

  assert.deepEqual(messages, []);
  assert.equal(
    requested,
    "https://cloud.example/v1/devices/device/communication/conversations/room%2Fname/messages?limit=50",
  );
});

test("loads older messages with a stable timestamp and event cursor", async () => {
  let requested = "";
  await getCommunicationMessages(async (input) => {
    requested = String(input);
    return new Response(JSON.stringify({ messages: [] }), { status: 200 });
  }, "https://cloud.example", "device", "room@chatroom", 100, {
    occurred_at: "2026-08-02T10:00:00.000Z",
    event_id: "01986666-7666-8666-8666-666666666667",
  });

  assert.equal(
    requested,
    "https://cloud.example/v1/devices/device/communication/conversations/room%40chatroom/messages?limit=100&before=2026-08-02T10%3A00%3A00.000Z&before_event_id=01986666-7666-8666-8666-666666666667",
  );
});

test("loads a short private read URL for one completed media object", async () => {
  let requested = "";
  const url = await getCommunicationObjectReadUrl(async (input) => {
    requested = String(input);
    return new Response(JSON.stringify({
      url: "https://private.example/signed",
      expires_at: "2026-08-02T10:05:00.000Z",
    }), { status: 200 });
  }, "https://cloud.example", "device/id", "object/id");

  assert.equal(url, "https://private.example/signed");
  assert.equal(
    requested,
    "https://cloud.example/v1/devices/device%2Fid/communication/objects/object%2Fid/read",
  );
});
