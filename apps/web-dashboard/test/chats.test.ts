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
  mergeLatestCommunicationMessages,
  messagesReadBaselineStorageKey,
  messagesReadStorageKey,
  type DashboardMessage,
} from "../src/lib/api.ts";

function message(eventId: string, occurredAt: string): DashboardMessage {
  return {
    event_id: eventId,
    message_id: eventId,
    sender_id: "sender",
    sender_display_name: "Sender",
    sender_avatar_url: null,
    occurred_at: occurredAt,
    direction: "incoming",
    kind: "text",
    text: eventId,
    attachments: [],
  };
}

test("decodes an encoded WeChat group identity exactly once", () => {
  assert.equal(decodeDashboardRouteParam("room%40chatroom"), "room@chatroom");
  assert.equal(decodeDashboardRouteParam("room@chatroom"), "room@chatroom");
});

test("chat unread state is isolated by device and conversation", () => {
  assert.equal(chatReadBaselineStorageKey("device-a"), "pca.chat-read-baseline:device-a");
  assert.equal(chatReadStorageKey("device-a", "room@chatroom"), "pca.chat-read:device-a:room@chatroom");
  assert.equal(messagesReadBaselineStorageKey("device-a"), "pca.messages-read-baseline:device-a");
  assert.equal(messagesReadStorageKey("device-a", "room"), "pca.messages-read:device-a:room");
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
    return new Response(JSON.stringify({
      conversations: [],
      pagination: { page: 3, page_size: 25, total_count: 0, total_pages: 0 },
    }), { status: 200 });
  }, "https://cloud.example", "device/id", "communication.wechat", 25, 3);

  assert.deepEqual(conversations.conversations, []);
  assert.equal(
    requested,
    "https://cloud.example/v1/devices/device%2Fid/communication/conversations?source=communication.wechat&limit=25&page=3",
  );
});

test("loads owner-scoped messages with encoded conversation identity", async () => {
  let requested = "";
  const messages = await getCommunicationMessages(async (input) => {
    requested = String(input);
    return new Response(JSON.stringify({ messages: [] }), { status: 200 });
  }, "https://cloud.example", "device", "room/name", "communication.messages", 50);

  assert.deepEqual(messages, []);
  assert.equal(
    requested,
    "https://cloud.example/v1/devices/device/communication/conversations/room%2Fname/messages?source=communication.messages&limit=50",
  );
});

test("loads older messages with a stable timestamp and event cursor", async () => {
  let requested = "";
  await getCommunicationMessages(async (input) => {
    requested = String(input);
    return new Response(JSON.stringify({ messages: [] }), { status: 200 });
  }, "https://cloud.example", "device", "room@chatroom", "communication.wechat", 100, {
    occurred_at: "2026-08-02T10:00:00.000Z",
    event_id: "01986666-7666-8666-8666-666666666667",
  });

  assert.equal(
    requested,
    "https://cloud.example/v1/devices/device/communication/conversations/room%40chatroom/messages?source=communication.wechat&limit=100&before=2026-08-02T10%3A00%3A00.000Z&before_event_id=01986666-7666-8666-8666-666666666667",
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

test("merges polled chat messages without dropping history or duplicating events", () => {
  const older = message("event-a", "2026-08-04T14:50:00Z");
  const existing = message("event-b", "2026-08-04T14:51:00Z");
  const latest = message("event-c", "2026-08-04T14:52:00Z");

  assert.deepEqual(
    mergeLatestCommunicationMessages([older, existing], [latest, existing]),
    [older, existing, latest],
  );
});
