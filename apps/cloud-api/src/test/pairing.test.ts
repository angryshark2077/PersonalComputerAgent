import assert from "node:assert/strict";
import { createHmac } from "node:crypto";
import test from "node:test";

import { memoryAdapter } from "better-auth/adapters/memory";
import { betterAuth } from "better-auth/minimal";

import { MemoryControlRepository } from "@pca/db-cloud/src/repository.js";

import {
  createApp,
  createProductionApp,
  createOwnerWorkspaceBootstrapHooks,
  createHashedSessionHooks,
  createTrustedProxyClientAddress,
  type OwnerPrincipal,
} from "../index.js";
import { createBetterAuthOwnerAuthenticator } from "../auth.js";
import { hashSecret, pkceChallenge } from "../pairing.js";

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

test("production source IP accepts only an explicitly configured signed proxy header", () => {
  const secret = "trusted-proxy-test-secret";
  const ip = "203.0.113.42";
  const signature = createHmac("sha256", secret).update(ip).digest("base64url");
  const address = createTrustedProxyClientAddress({ TRUSTED_PROXY_CLIENT_IP_HMAC_SECRET: secret });
  assert.equal(
    address(
      new Request("http://localhost", {
        headers: { "x-pca-client-ip": ip, "x-pca-client-ip-signature": signature },
      }),
    ),
    ip,
  );
  assert.equal(
    address(new Request("http://localhost", { headers: { "x-forwarded-for": ip } })),
    undefined,
  );
});

test("production session hook persists only a hash of the generated Better Auth token", async () => {
  const token = "session-token-that-must-never-reach-postgresql";
  const result = await createHashedSessionHooks().session.create.before({ token });
  assert.deepEqual(result, { data: { token: hashSecret(token) } });
  assert.notEqual(result.data.token, token);
});

test("hashed Better Auth sessions still resolve through a signed cookie", async () => {
  const database = { user: [], session: [], account: [] };
  const auth = betterAuth({
    database: memoryAdapter(database),
    secret: "test-secret-that-is-long-enough-to-be-valid",
    baseURL: "http://localhost:3000",
    emailAndPassword: { enabled: true },
    session: { fields: { token: "sessionTokenHash" } },
    databaseHooks: createHashedSessionHooks(),
  });
  const signup = await auth.handler(
    new Request("http://localhost:3000/api/auth/sign-up/email", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ name: "Session Test", email: "session@example.invalid", password: "password123" }),
    }),
  );
  assert.equal(signup.status, 200);
  const sessionToken = ((await signup.json()) as { token: string }).token;
  const persisted = database.session[0] as Record<string, unknown> | undefined;
  assert.equal(persisted?.sessionTokenHash, sessionToken);
  assert.equal("token" in (persisted ?? {}), false);
  const cookie = signup.headers.get("set-cookie")?.split(";", 1)[0];
  if (cookie === undefined) assert.fail("sign-up did not set a session cookie");
  const session = await auth.api.getSession({ headers: new Headers({ cookie }) });
  assert.equal(session?.user.email, "session@example.invalid");
});

test("production signup bootstrap creates one Owner workspace before pairing authorization", async () => {
  const userId = "01983333-7333-8333-8333-333333333334";
  const repository = new MemoryControlRepository();
  const hooks = createOwnerWorkspaceBootstrapHooks(repository);
  await hooks.user.create.after({ id: userId });
  await hooks.user.create.after({ id: userId });
  const [workspace] = await repository.listOwnerWorkspaces(userId);
  assert.notEqual(workspace, undefined);
  assert.equal((await repository.listOwnerWorkspaces(userId)).length, 1);

  const api = createApp({
    repository,
    ownerAuthenticator: async () => ({ userId, workspaceId: workspace?.workspaceId ?? "" }),
  });
  const pairing = await api.request("/v1/device-pairing/sessions", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      ...start,
      device_public_key: "fresh-signup-device-key",
      code_challenge: pkceChallenge("fresh-signup-verifier"),
    }),
  });
  const { session_id: sessionId } = (await pairing.json()) as { session_id: string };
  const authorized = await api.request(`/v1/device-pairing/sessions/${sessionId}/authorize`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ callback_state: start.callback_state }),
  });
  assert.equal(authorized.status, 302);
});

test("an authenticated Owner request repairs a failed post-create workspace bootstrap", async () => {
  const userId = "01983333-7333-8333-8333-333333333335";
  const repository = new MemoryControlRepository();
  const failedHook = createOwnerWorkspaceBootstrapHooks({
    bootstrapOwnerWorkspace: async () => {
      throw new Error("simulated post-create failure");
    },
  });
  await assert.rejects(() => failedHook.user.create.after({ id: userId }));
  assert.equal(await repository.resolveOwnerWorkspace(userId), null);

  const ownerAuthenticator = createBetterAuthOwnerAuthenticator(
    { api: { getSession: async () => ({ user: { id: userId } }) } },
    repository,
  );
  const api = createApp({ repository, ownerAuthenticator });
  const pairing = await api.request("/v1/device-pairing/sessions", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      ...start,
      device_public_key: "recovered-signup-device-key",
      code_challenge: pkceChallenge("recovered-signup-verifier"),
    }),
  });
  const { session_id: sessionId } = (await pairing.json()) as { session_id: string };
  const recovered = await api.request(`/v1/device-pairing/sessions/${sessionId}/authorize`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ callback_state: start.callback_state }),
  });
  assert.equal(recovered.status, 302);
  assert.equal((await repository.listOwnerWorkspaces(userId)).length, 1);
});
