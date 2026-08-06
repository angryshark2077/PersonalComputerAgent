import assert from "node:assert/strict";
import test from "node:test";

import { safeLocalCallbackPath, signInWithEmail, signOut, signUpWithEmail } from "../src/lib/auth.ts";

test("sign in only returns to a local Dashboard path", () => {
  assert.equal(safeLocalCallbackPath("/pair?session_id=session&callback_state=state"), "/pair?session_id=session&callback_state=state");
  assert.equal(safeLocalCallbackPath("//attacker.example.test"), "/");
  assert.equal(safeLocalCallbackPath("/\\attacker.example.test"), "/");
});

test("sign in sends credentials only to the Better Auth email endpoint", async () => {
  let request: Request | undefined;
  await signInWithEmail(async (input, init) => {
    request = new Request(input, init);
    return new Response(JSON.stringify({ user: { id: "owner" } }), { status: 200 });
  }, "https://cloud.example.test", { email: "owner@example.test", password: "password" });

  assert.equal(request?.url, "https://cloud.example.test/api/auth/sign-in/email");
  assert.equal(request?.method, "POST");
  assert.equal(await request?.text(), JSON.stringify({ email: "owner@example.test", password: "password" }));
});

test("sign up creates a Better Auth email session without putting credentials in a URL", async () => {
  let request: Request | undefined;
  await signUpWithEmail(async (input, init) => {
    request = new Request(input, init);
    return new Response(JSON.stringify({ user: { id: "owner" } }), { status: 200 });
  }, "https://cloud.example.test", {
    name: "Owner",
    email: "owner@example.test",
    password: "password",
  });

  assert.equal(request?.url, "https://cloud.example.test/api/auth/sign-up/email");
  assert.equal(request?.method, "POST");
  assert.equal(
    await request?.text(),
    JSON.stringify({ name: "Owner", email: "owner@example.test", password: "password" }),
  );
});

test("sign out sends the JSON request required by the Better Auth sign-out endpoint", async () => {
  let request: Request | undefined;
  await signOut(async (input, init) => {
    request = new Request(input, init);
    return new Response(null, { status: 200 });
  }, "https://cloud.example.test");

  assert.equal(request?.url, "https://cloud.example.test/api/auth/sign-out");
  assert.equal(request?.method, "POST");
  assert.equal(request?.headers.get("content-type"), "application/json");
  assert.equal(await request?.text(), "{}");
  assert.equal(request?.headers.get("cookie"), null);
  assert.equal(request?.credentials, "include");
});
