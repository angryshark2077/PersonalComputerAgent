import assert from "node:assert/strict";
import { randomBytes, randomUUID } from "node:crypto";
import { mkdtemp, readFile, readdir, rm, stat } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

import { createApp } from "../../apps/cloud-api/src/index.ts";
import { pkceChallenge } from "../../apps/cloud-api/src/pairing.ts";
import {
  authorizePairing,
  getCollectorAudit,
  revokeDevice,
  updateCollectorConfig,
  type DashboardFetch,
} from "../../apps/web-dashboard/src/lib/api.ts";
import { MemoryControlRepository } from "../../packages/db-cloud/src/repository.ts";

const repositoryRoot = resolve(fileURLToPath(new URL("../..", import.meta.url)));
const agentHarness = process.env.PCA_S1B_ACCEPTANCE_AGENT;

if (agentHarness === undefined || agentHarness.length === 0) {
  throw new Error("PCA_S1B_ACCEPTANCE_AGENT is required");
}

const owner = {
  userId: "01981111-7111-8111-8111-111111111111",
  workspaceId: "01982222-7222-8222-8222-222222222222",
};

async function main(): Promise<void> {
  const runtimeRoot = await mkdtemp(join(tmpdir(), "pca-s1b-acceptance-"));
  try {
    const repository = new MemoryControlRepository([owner]);
    const api = createApp({
      repository,
      ownerAuthenticator: async () => owner,
      clientAddress: () => "203.0.113.10",
    });
    const dashboardFetch: DashboardFetch = (input, init) =>
      api.request(typeof input === "string" ? input : input.toString(), init);

    const callbackState = randomBytes(32).toString("base64url");
    const codeVerifier = randomBytes(32).toString("base64url");
    const callback = await listenForCallback(callbackState);
    const start = await api.request("/v1/device-pairing/sessions", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        device_public_key: randomBytes(32).toString("base64url"),
        code_challenge: pkceChallenge(codeVerifier),
        callback_uri: callback.uri,
        callback_state: callbackState,
      }),
    });
    assert.equal(start.status, 201);
    const { session_id: sessionId } = (await start.json()) as { session_id: string };
    const redirect = await authorizePairing(
      dashboardFetch,
      "",
      sessionId,
      callbackState,
    );
    const callbackResponse = await fetch(redirect);
    assert.equal(callbackResponse.status, 200);
    const authorizationCode = await callback.received;
    await callback.close();

    const exchange = await api.request("/v1/device-pairing/exchange", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        session_id: sessionId,
        authorization_code: authorizationCode,
        code_verifier: codeVerifier,
      }),
    });
    assert.equal(exchange.status, 200);
    const credentials = (await exchange.json()) as {
      workspace_id: string;
      device_id: string;
      device_access_token: string;
      refresh_token: string;
      access_expires_at: string;
      refresh_expires_at: string;
    };

    const revision = await updateCollectorConfig(
      dashboardFetch,
      "",
      credentials.device_id,
      {
        network: { enabled: true },
        "communication.wechat": {
          enabled: true,
          direction: "outgoing",
          message_type: "text",
          sync_mode: "full",
        },
      },
    );
    assert.equal(revision, 1);

    const control = await api.request("/v1/agent/control", {
      method: "POST",
      headers: {
        authorization: `Bearer ${credentials.device_access_token}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({
        heartbeat_id: randomUUID(),
        agent_version: "s1b-acceptance",
        presence: "online",
        outbox_depth: 0,
      }),
    });
    assert.equal(control.status, 200);
    const { snapshot } = (await control.json()) as {
      snapshot: { configuration_revision: number };
    };
    assert.equal(snapshot.configuration_revision, 1);

    const messageBodyCanary = `message-body-${randomBytes(24).toString("base64url")}`;
    const sensitiveValues = [
      credentials.device_access_token,
      credentials.refresh_token,
      messageBodyCanary,
    ];
    const pairedStatusPath = join(runtimeRoot, "paired-status.json");
    const pairedProcess = await runAgent("pair-control", runtimeRoot, pairedStatusPath, {
      ...credentials,
      access_expires_at_ms: Date.parse(credentials.access_expires_at),
      refresh_expires_at_ms: Date.parse(credentials.refresh_expires_at),
      callback_uri: callback.uri,
      configuration_revision: revision,
      message_body_canary: messageBodyCanary,
    });
    assertNoSensitiveText("pair-control stdout", pairedProcess.stdout, sensitiveValues);
    assertNoSensitiveText("pair-control stderr", pairedProcess.stderr, sensitiveValues);
    const pairedStatusBytes = await readFile(pairedStatusPath);
    assertNoSensitiveBytes("paired JSON status", pairedStatusBytes, sensitiveValues);
    assert.deepEqual(JSON.parse(pairedStatusBytes.toString("utf8")), {
      phase: "pair-control",
      agent_status: "degraded",
      paired: true,
      applied_control_revision: 1,
      device_id: credentials.device_id,
      workspace_id: credentials.workspace_id,
    });
    await assertDatabaseHasNoSensitiveValues(runtimeRoot, sensitiveValues);

    const audit = await getCollectorAudit(dashboardFetch, "", credentials.device_id);
    assert.equal(audit.length, 1);
    assert.equal(audit[0]?.actor_user_id, owner.userId);
    assert.equal(audit[0]?.configuration_revision, 1);
    assert.equal(audit[0]?.new_config.network.enabled, true);
    assert.equal(audit[0]?.new_config["communication.wechat"].enabled, true);

    await revokeDevice(dashboardFetch, "", credentials.device_id);
    const rejectedControl = await api.request("/v1/agent/control", {
      method: "POST",
      headers: {
        authorization: `Bearer ${credentials.device_access_token}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({
        heartbeat_id: randomUUID(),
        agent_version: "s1b-acceptance",
        presence: "online",
        outbox_depth: 0,
      }),
    });
    assert.equal(rejectedControl.status, 401);

    const revokedStatusPath = join(runtimeRoot, "revoked-status.json");
    const revokedProcess = await runAgent("revoke", runtimeRoot, revokedStatusPath, {
      device_id: credentials.device_id,
      workspace_id: credentials.workspace_id,
      configuration_revision: revision,
      message_body_canary: messageBodyCanary,
    });
    assertNoSensitiveText("revoke stdout", revokedProcess.stdout, sensitiveValues);
    assertNoSensitiveText("revoke stderr", revokedProcess.stderr, sensitiveValues);
    const revokedStatusBytes = await readFile(revokedStatusPath);
    assertNoSensitiveBytes("revoked JSON status", revokedStatusBytes, sensitiveValues);
    assert.deepEqual(JSON.parse(revokedStatusBytes.toString("utf8")), {
      phase: "revoke",
      agent_status: "unpaired",
      paired: false,
      applied_control_revision: null,
      device_id: credentials.device_id,
      workspace_id: credentials.workspace_id,
    });
    await assertDatabaseHasNoSensitiveValues(runtimeRoot, sensitiveValues);
    await assert.rejects(stat(join(runtimeRoot, "test-keychain", "device-credential.json")));
    await assertFixturesHaveNoSensitiveValues(sensitiveValues);

    process.stdout.write("S1B process acceptance passed.\n");
  } finally {
    await rm(runtimeRoot, { recursive: true, force: true });
  }
}

async function listenForCallback(expectedState: string): Promise<{
  uri: string;
  received: Promise<string>;
  close: () => Promise<void>;
}> {
  let resolveCode: (value: string) => void;
  let rejectCode: (error: Error) => void;
  const received = new Promise<string>((resolve, reject) => {
    resolveCode = resolve;
    rejectCode = reject;
  });
  const server = createServer((request, response) => {
    try {
      const url = new URL(request.url ?? "", "http://127.0.0.1");
      const code = url.searchParams.getAll("code");
      const state = url.searchParams.getAll("state");
      assert.equal(request.method, "GET");
      assert.equal(url.pathname, "/pca/pair/callback");
      assert.deepEqual(state, [expectedState]);
      assert.equal(code.length, 1);
      assert.notEqual(code[0], "");
      response.writeHead(200, { "content-type": "text/plain" });
      response.end("Pairing callback accepted.");
      resolveCode(code[0] as string);
    } catch (error) {
      response.writeHead(400);
      response.end();
      rejectCode(error instanceof Error ? error : new Error("invalid callback"));
    }
  });
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => resolve());
  });
  const address = server.address();
  assert.notEqual(address, null);
  assert.equal(typeof address, "object");
  return {
    uri: `http://127.0.0.1:${(address as { port: number }).port}/pca/pair/callback`,
    received,
    close: () => new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve())),
  };
}

async function runAgent(
  phase: "pair-control" | "revoke",
  runtimeRoot: string,
  statusPath: string,
  payload: object,
): Promise<{ stdout: string; stderr: string }> {
  return new Promise((resolveProcess, rejectProcess) => {
    const child = spawn(agentHarness as string, [
      "--phase",
      phase,
      "--runtime-root",
      runtimeRoot,
      "--status-file",
      statusPath,
    ], { stdio: ["pipe", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    child.stdout.setEncoding("utf8").on("data", (chunk) => { stdout += chunk; });
    child.stderr.setEncoding("utf8").on("data", (chunk) => { stderr += chunk; });
    child.once("error", rejectProcess);
    child.once("close", (code) => {
      if (code === 0) resolveProcess({ stdout, stderr });
      else rejectProcess(new Error(`acceptance Agent phase ${phase} exited ${code}`));
    });
    child.stdin.end(JSON.stringify(payload));
  });
}

function assertNoSensitiveText(label: string, value: string, sensitiveValues: readonly string[]): void {
  assertNoSensitiveBytes(label, Buffer.from(value), sensitiveValues);
}

function assertNoSensitiveBytes(label: string, value: Buffer, sensitiveValues: readonly string[]): void {
  for (const sensitive of sensitiveValues) {
    assert.equal(value.includes(Buffer.from(sensitive)), false, `${label} contained sensitive material`);
  }
}

async function assertDatabaseHasNoSensitiveValues(
  runtimeRoot: string,
  sensitiveValues: readonly string[],
): Promise<void> {
  for (const name of await readdir(runtimeRoot)) {
    if (!name.includes("sqlite")) continue;
    assertNoSensitiveBytes(
      `SQLite artifact ${name}`,
      await readFile(join(runtimeRoot, name)),
      sensitiveValues,
    );
  }
}

async function assertFixturesHaveNoSensitiveValues(sensitiveValues: readonly string[]): Promise<void> {
  for (const path of await fixtureFiles(repositoryRoot)) {
    assertNoSensitiveBytes(`fixture ${path}`, await readFile(path), sensitiveValues);
  }
}

async function fixtureFiles(directory: string): Promise<string[]> {
  const paths: string[] = [];
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    if ([".git", ".worktrees", "node_modules", "target", ".next", ".build"].includes(entry.name)) {
      continue;
    }
    const path = join(directory, entry.name);
    if (entry.isDirectory()) {
      paths.push(...await fixtureFiles(path));
    } else if (path.split("/").some((segment) => segment.toLowerCase().includes("fixture"))) {
      paths.push(path);
    }
  }
  return paths;
}

main().catch((error: unknown) => {
  const message = error instanceof Error ? error.message : "unknown acceptance failure";
  process.stderr.write(`S1B process acceptance failed: ${message}\n`);
  process.exitCode = 1;
});
