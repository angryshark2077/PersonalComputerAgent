import assert from "node:assert/strict";
import test from "node:test";

import { MemoryControlRepository } from "@pca/db-cloud/src/repository.js";

import { createApp, createProductionApp, type OwnerPrincipal } from "../index.js";
import { pkceChallenge } from "../pairing.js";

const owner: OwnerPrincipal = {
  userId: "01983333-7333-8333-8333-333333333333",
  workspaceId: "01982222-7222-8222-8222-222222222222",
};
const start = {
  device_public_key: "device-public-key-a",
  code_challenge: pkceChallenge("verifier-a"),
  callback_uri: "http://127.0.0.1:43123/pca/pair/callback",
  callback_state: "1234567890123456789012345678901234567890123",
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
    {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ callback_state: start.callback_state }),
    },
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
        code_verifier: "wrong-verifier",
      }),
    });

  assert.equal((await exchange()).status, 400);

  const secondSession = await api.request("/v1/device-pairing/sessions", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ ...start, code_challenge: pkceChallenge("verifier-b") }),
  });
  const { session_id: secondSessionId } = (await secondSession.json()) as {
    session_id: string;
  };
  const secondAuthorized = await api.request(
    `/v1/device-pairing/sessions/${secondSessionId}/authorize`,
    {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ callback_state: start.callback_state }),
    },
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
    {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ callback_state: start.callback_state }),
    },
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
    {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ callback_state: start.callback_state }),
    },
  );
  const location = authorized.headers.get("location") ?? "";
  assert.equal(location.includes("access_token"), false);
  assert.equal(location.includes("refresh_token"), false);
});

test("callback state must match the hashed value supplied by Setup", async () => {
  const api = app();
  const response = await api.request("/v1/device-pairing/sessions", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(start),
  });
  const { session_id: sessionId } = (await response.json()) as { session_id: string };
  const mismatch = await api.request(`/v1/device-pairing/sessions/${sessionId}/authorize`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ callback_state: "x".repeat(43) }),
  });
  assert.equal(mismatch.status, 410);
  const authorized = await api.request(`/v1/device-pairing/sessions/${sessionId}/authorize`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ callback_state: start.callback_state }),
  });
  assert.equal(authorized.status, 302);
  assert.equal(
    new URL(authorized.headers.get("location") ?? "").searchParams.get("state"),
    start.callback_state,
  );
});

test("pairing start is rate limited to three device-key attempts per minute", async () => {
  const api = createApp({
    repository: new MemoryControlRepository([
      { workspaceId: owner.workspaceId, userId: owner.userId },
    ]),
    clientAddress: () => "203.0.113.10",
  });
  const request = () =>
    api.request("/v1/device-pairing/sessions", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(start),
    });
  assert.equal((await request()).status, 201);
  assert.equal((await request()).status, 201);
  assert.equal((await request()).status, 201);
  const limited = await request();
  assert.equal(limited.status, 429);
  assert.equal(limited.headers.get("retry-after"), "60");
});

test("pairing start is rate limited to ten source-IP attempts per minute", async () => {
  const api = createApp({
    repository: new MemoryControlRepository([
      { workspaceId: owner.workspaceId, userId: owner.userId },
    ]),
    clientAddress: () => "203.0.113.11",
  });
  for (let index = 0; index < 10; index += 1) {
    const response = await api.request("/v1/device-pairing/sessions", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ ...start, device_public_key: `device-${index}` }),
    });
    assert.equal(response.status, 201);
  }
  const limited = await api.request("/v1/device-pairing/sessions", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ ...start, device_public_key: "device-over-limit" }),
  });
  assert.equal(limited.status, 429);
  assert.equal(limited.headers.get("retry-after"), "60");
});

test("production composition fails closed when persistent configuration is absent", () => {
  assert.throws(() => createProductionApp({}), /missing required configuration: DATABASE_URL/);
});

test("production composition wires persistent PostgreSQL and Better Auth", () => {
  const api = createProductionApp({
    DATABASE_URL: "postgresql://localhost:1/pca",
    BETTER_AUTH_SECRET: "test-secret-that-is-long-enough-to-be-valid",
    BETTER_AUTH_URL: "http://localhost:3000",
  });
  assert.ok(api);
});
