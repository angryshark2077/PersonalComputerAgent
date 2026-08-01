import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { createServer } from "node:net";
import test from "node:test";

import { betterAuth } from "better-auth/minimal";
import { eq } from "drizzle-orm";
import { drizzle } from "drizzle-orm/node-postgres";
import { Pool } from "pg";

import {
  authAccounts,
  authSessions,
  authUsers,
  cloudSchema,
} from "@pca/db-cloud/src/schema.js";

import { createHashedSessionAdapter } from "../index.js";
import { runCloudMigrations } from "../migrate.js";
import { hashSecret } from "../pairing.js";

const postgresUser = "pca_session_test";
const testDirectory = dirname(fileURLToPath(import.meta.url));

test("PostgreSQL stores only hashes while raw signed cookies still resolve and revoke sessions", async () => {
  const postgres = await startTemporaryPostgres();
  const pool = new Pool({ connectionString: postgres.connectionString });
  const database = drizzle(pool, { schema: cloudSchema });
  const auth = betterAuth({
    database: createHashedSessionAdapter(database),
    secret: "test-secret-that-is-long-enough-to-be-valid",
    baseURL: "http://localhost:3000",
    emailAndPassword: { enabled: true },
    advanced: { database: { generateId: () => randomUUID() } },
    user: { fields: { image: "imageUrl" } },
    session: { fields: { token: "sessionTokenHash" } },
    account: { fields: { password: "passwordHash" } },
  });

  try {
    const first = await signUp(auth, "first-session@example.invalid");
    const [stored] = await database
      .select({ token: authSessions.sessionTokenHash })
      .from(authSessions)
      .where(eq(authSessions.sessionTokenHash, hashSecret(first.token)));
    assert.equal(stored?.token, hashSecret(first.token));
    assert.notEqual(stored?.token, first.token);

    const current = await auth.api.getSession({ headers: new Headers({ cookie: first.cookie }) });
    assert.equal(current?.user.email, "first-session@example.invalid");

    const logout = await auth.handler(
      new Request("http://localhost:3000/api/auth/sign-out", {
        method: "POST",
        headers: { cookie: first.cookie, origin: "http://localhost:3000" },
      }),
    );
    assert.equal(logout.status, 200);
    const [revoked] = await database
      .select({ token: authSessions.sessionTokenHash })
      .from(authSessions)
      .where(eq(authSessions.sessionTokenHash, hashSecret(first.token)));
    assert.equal(revoked, undefined);

    const loggedIn = await signIn(auth, "first-session@example.invalid");
    const [storedLogin] = await database
      .select({ token: authSessions.sessionTokenHash })
      .from(authSessions)
      .where(eq(authSessions.sessionTokenHash, hashSecret(loggedIn.token)));
    assert.equal(storedLogin?.token, hashSecret(loggedIn.token));
    assert.notEqual(storedLogin?.token, loggedIn.token);
    const currentLogin = await auth.api.getSession({
      headers: new Headers({ cookie: loggedIn.cookie }),
    });
    assert.equal(currentLogin?.user.email, "first-session@example.invalid");
    const loggedOut = await auth.handler(
      new Request("http://localhost:3000/api/auth/sign-out", {
        method: "POST",
        headers: { cookie: loggedIn.cookie, origin: "http://localhost:3000" },
      }),
    );
    assert.equal(loggedOut.status, 200);
    const [revokedLogin] = await database
      .select({ token: authSessions.sessionTokenHash })
      .from(authSessions)
      .where(eq(authSessions.sessionTokenHash, hashSecret(loggedIn.token)));
    assert.equal(revokedLogin, undefined);

    const expired = await signUp(auth, "expired-session@example.invalid");
    await database
      .update(authSessions)
      .set({ expiresAt: new Date(Date.now() - 1_000) })
      .where(eq(authSessions.sessionTokenHash, hashSecret(expired.token)));
    const expiredSession = await auth.api.getSession({
      headers: new Headers({ cookie: expired.cookie }),
    });
    assert.equal(expiredSession, null);
    const [expiredRow] = await database
      .select({ token: authSessions.sessionTokenHash })
      .from(authSessions)
      .where(eq(authSessions.sessionTokenHash, hashSecret(expired.token)));
    assert.equal(expiredRow, undefined);
  } finally {
    await pool.end();
    await postgres.stop();
  }
});

async function signUp(auth: { handler(request: Request): Promise<Response> }, email: string) {
  const response = await auth.handler(
    new Request("http://localhost:3000/api/auth/sign-up/email", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ name: "Session Test", email, password: "password123" }),
    }),
  );
  assert.equal(response.status, 200);
  const { token } = (await response.json()) as { token: string };
  const cookie = response.headers.get("set-cookie")?.split(";", 1)[0];
  assert.notEqual(cookie, undefined, "sign-up did not set a session cookie");
  return { token, cookie: cookie ?? "" };
}

async function signIn(auth: { handler(request: Request): Promise<Response> }, email: string) {
  const response = await auth.handler(
    new Request("http://localhost:3000/api/auth/sign-in/email", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ email, password: "password123" }),
    }),
  );
  assert.equal(response.status, 200);
  const { token } = (await response.json()) as { token: string };
  const cookie = response.headers.get("set-cookie")?.split(";", 1)[0];
  assert.notEqual(cookie, undefined, "sign-in did not set a session cookie");
  return { token, cookie: cookie ?? "" };
}

async function startTemporaryPostgres() {
  const root = await mkdtemp(join(tmpdir(), "pca-session-test-"));
  const dataDirectory = join(root, "data");
  const port = await availablePort();
  let started = false;
  const run = (binary: string, arguments_: string[]) =>
    execFileSync(binary, arguments_, { encoding: "utf8", stdio: "pipe" });

  try {
    run("initdb", [
      "-D",
      dataDirectory,
      "-A",
      "trust",
      "-U",
      postgresUser,
      "--encoding=UTF8",
      "--no-locale",
      "--no-sync",
    ]);
    run("pg_ctl", [
      "-D",
      dataDirectory,
      "-o",
      `-F -h 127.0.0.1 -p ${port}`,
      "-l",
      join(root, "postgres.log"),
      "-w",
      "start",
    ]);
    started = true;
    const connectionString = `postgresql://${postgresUser}@127.0.0.1:${port}/postgres`;
    await runCloudMigrations(connectionString, resolve(testDirectory, "../../../../packages/db-cloud/migrations"));
    return {
      connectionString,
      stop: async () => {
        if (started) run("pg_ctl", ["-D", dataDirectory, "-m", "fast", "-w", "stop"]);
        await rm(root, { recursive: true, force: true });
      },
    };
  } catch (error) {
    if (started) run("pg_ctl", ["-D", dataDirectory, "-m", "fast", "-w", "stop"]);
    await rm(root, { recursive: true, force: true });
    throw error;
  }
}

async function availablePort(): Promise<number> {
  return new Promise((resolvePort, reject) => {
    const server = createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      if (typeof address === "object" && address !== null) {
        server.close((error) => (error === undefined ? resolvePort(address.port) : reject(error)));
      } else {
        server.close();
        reject(new Error("unable to allocate a temporary PostgreSQL port"));
      }
    });
  });
}
