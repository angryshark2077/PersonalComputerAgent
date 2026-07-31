import assert from "node:assert/strict";
import test from "node:test";

import { authorizePairing } from "../src/lib/api.ts";
import { createCallbackState } from "../src/lib/pairing-state.ts";

test("pairing callback state meets the Cloud API entropy and character requirements", () => {
  const state = createCallbackState();

  assert.match(state, /^[A-Za-z0-9_-]{43,}$/);
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
            "http://127.0.0.1:43123/pca/pair/callback?code=one-time-code&state=callback-state",
        },
      });
    },
    "https://cloud.example.test",
    "pairing-session",
    "callback-state",
  );

  assert.equal(request?.method, "POST");
  assert.equal(
    request?.url,
    "https://cloud.example.test/v1/device-pairing/sessions/pairing-session/authorize",
  );
  assert.equal(await request?.text(), JSON.stringify({ callback_state: "callback-state" }));
  assert.equal(
    redirect,
    "http://127.0.0.1:43123/pca/pair/callback?code=one-time-code&state=callback-state",
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
      "callback-state",
    ),
    /invalid pairing callback/i,
  );
});

test("pairing authorization supports the same-origin Cloud API deployment", async () => {
  let requestUrl: RequestInfo | URL | undefined;
  await authorizePairing(
    async (input) => {
      requestUrl = input;
      return new Response(null, {
        status: 302,
        headers: { location: "http://127.0.0.1:43123/pca/pair/callback?code=code&state=state" },
      });
    },
    "",
    "pairing-session",
    "callback-state",
  );

  assert.equal(requestUrl, "/v1/device-pairing/sessions/pairing-session/authorize");
});
