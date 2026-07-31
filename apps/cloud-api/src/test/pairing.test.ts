import assert from "node:assert/strict";
import test from "node:test";

import { MemoryControlRepository } from "@pca/db-cloud/src/repository.js";

import { createApp, type OwnerPrincipal } from "../index.js";

const owner: OwnerPrincipal = {
  userId: "01983333-7333-8333-8333-333333333333",
  workspaceId: "01982222-7222-8222-8222-222222222222",
};
const start = {
  device_public_key: "device-public-key-a",
  code_challenge: "challenge-a",
  callback_uri: "http://127.0.0.1:43123/pca/pair/callback",
};

function app() {
  return createApp({
    repository: new MemoryControlRepository([
      { workspaceId: owner.workspaceId, userId: owner.userId },
    ]),
    ownerAuthenticator: async () => owner,
  });
}

test("a pairing code is single use and PKCE bound", async () => {
  const api = app();
  const sessionResponse = await api.request("/v1/device-pairing/sessions", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(start),
  });
  assert.equal(sessionResponse.status, 201);
  const { session_id: sessionId } = (await sessionResponse.json()) as {
    session_id: string;
  };

  const authorized = await api.request(
    `/v1/device-pairing/sessions/${sessionId}/authorize`,
    { method: "POST" },
  );
  assert.equal(authorized.status, 302);
  const location = authorized.headers.get("location");
  assert.notEqual(location, null);
  const callback = new URL(location ?? "");
  assert.deepEqual([...callback.searchParams.keys()].sort(), ["code", "state"]);
  const authorizationCode = callback.searchParams.get("code");
  assert.notEqual(authorizationCode, null);

  const exchange = () =>
    api.request("/v1/device-pairing/exchange", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        session_id: sessionId,
        authorization_code: authorizationCode,
        code_verifier: "verifier-a",
      }),
    });

  assert.equal((await exchange()).status, 400);

  const secondSession = await api.request("/v1/device-pairing/sessions", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ ...start, code_challenge: "verifier-b" }),
  });
  const { session_id: secondSessionId } = (await secondSession.json()) as {
    session_id: string;
  };
  const secondAuthorized = await api.request(
    `/v1/device-pairing/sessions/${secondSessionId}/authorize`,
    { method: "POST" },
  );
  const secondLocation = secondAuthorized.headers.get("location");
  const secondCode = new URL(secondLocation ?? "").searchParams.get("code");
  const successfulExchange = () =>
    api.request("/v1/device-pairing/exchange", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        session_id: secondSessionId,
        authorization_code: secondCode,
        code_verifier: "verifier-b",
      }),
    });
  assert.equal((await successfulExchange()).status, 200);
  assert.equal((await successfulExchange()).status, 409);
});

test("expired pairing sessions and unauthenticated authorization are rejected", async () => {
  const repository = new MemoryControlRepository([
    { workspaceId: owner.workspaceId, userId: owner.userId },
  ]);
  const noOwner = createApp({ repository });
  const session = await noOwner.request("/v1/device-pairing/sessions", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(start),
  });
  const { session_id: sessionId } = (await session.json()) as { session_id: string };
  const unauthorized = await noOwner.request(
    `/v1/device-pairing/sessions/${sessionId}/authorize`,
    { method: "POST" },
  );
  assert.equal(unauthorized.status, 401);
});

test("pairing responses never place credentials in a callback URL", async () => {
  const api = app();
  const response = await api.request("/v1/device-pairing/sessions", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(start),
  });
  const { session_id: sessionId } = (await response.json()) as { session_id: string };
  const authorized = await api.request(
    `/v1/device-pairing/sessions/${sessionId}/authorize`,
    { method: "POST" },
  );
  const location = authorized.headers.get("location") ?? "";
  assert.equal(location.includes("access_token"), false);
  assert.equal(location.includes("refresh_token"), false);
});
