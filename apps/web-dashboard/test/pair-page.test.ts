import assert from "node:assert/strict";
import test from "node:test";

import { authorizePairing } from "../src/lib/api.ts";
import { parsePairingHandoff } from "../src/lib/pairing-state.ts";

const setupState = "A".repeat(43);

test("pairing handoff preserves the exact Setup session and state", () => {
  assert.deepEqual(
    parsePairingHandoff(new URLSearchParams({ session_id: "pairing-session", callback_state: setupState })),
    { sessionId: "pairing-session", callbackState: setupState },
  );
});

test("pairing handoff rejects a missing or malformed Setup state", () => {
  assert.equal(parsePairingHandoff(new URLSearchParams({ session_id: "pairing-session" })), null);
  assert.equal(
    parsePairingHandoff(new URLSearchParams({ session_id: "pairing-session", callback_state: "short" })),
    null,
  );
});

test("pairing authorization redirects only to the loopback callback returned by the API", async () => {
  let request: Request | undefined;
  const redirect = await authorizePairing(
    async (input, init) => {
      request = new Request(input, init);
      return new Response(null, {
        status: 302,
        headers: {
          location:
            `http://127.0.0.1:43123/pca/pair/callback?code=one-time-code&state=${setupState}`,
        },
      });
    },
    "https://cloud.example.test",
    "pairing-session",
    setupState,
  );

  assert.equal(request?.method, "POST");
  assert.equal(
    request?.url,
    "https://cloud.example.test/v1/device-pairing/sessions/pairing-session/authorize",
  );
  assert.equal(await request?.text(), JSON.stringify({ callback_state: setupState }));
  assert.equal(
    redirect,
    `http://127.0.0.1:43123/pca/pair/callback?code=one-time-code&state=${setupState}`,
  );
});

test("pairing authorization rejects a redirect outside the registered loopback callback", async () => {
  await assert.rejects(
    authorizePairing(
      async () =>
        new Response(null, {
          status: 302,
          headers: { location: "https://attacker.example.test/callback?code=one-time-code" },
        }),
      "https://cloud.example.test",
      "pairing-session",
      setupState,
    ),
    /invalid pairing callback/i,
  );
});

test("pairing authorization fails closed when Cloud returns another state", async () => {
  await assert.rejects(
    authorizePairing(
      async () => new Response(null, {
        status: 302,
        headers: { location: "http://127.0.0.1:43123/pca/pair/callback?code=one-time-code&state=wrong" },
      }),
      "https://cloud.example.test",
      "pairing-session",
      setupState,
    ),
    /state mismatch/i,
  );
});

test("pairing authorization supports the same-origin Cloud API deployment", async () => {
  let requestUrl: RequestInfo | URL | undefined;
  await authorizePairing(
    async (input) => {
      requestUrl = input;
      return new Response(null, {
        status: 302,
        headers: { location: `http://127.0.0.1:43123/pca/pair/callback?code=code&state=${setupState}` },
      });
    },
    "",
    "pairing-session",
    setupState,
  );

  assert.equal(requestUrl, "/v1/device-pairing/sessions/pairing-session/authorize");
});
