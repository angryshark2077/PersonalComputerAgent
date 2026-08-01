import assert from "node:assert/strict";
import test from "node:test";

import { pairingAuthorizePath } from "../src/lib/api.ts";
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

test("pairing uses the Dashboard same-origin authorization path", () => {
  assert.equal(
    pairingAuthorizePath("pairing-session"),
    "/v1/device-pairing/sessions/pairing-session/authorize",
  );
});
