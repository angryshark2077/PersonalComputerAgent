import assert from "node:assert/strict";
import test from "node:test";

import { getCommunicationConversations, getCommunicationMessages } from "../src/lib/api.ts";

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
