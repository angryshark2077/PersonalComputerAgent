import assert from "node:assert/strict";
import { mkdtemp, readFile, readdir, rm, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

import {
  createS1bAcceptanceCloud,
  type S1bExchangedDevice,
  type S1bPairingHandoff,
} from "../../apps/cloud-api/src/test/support/s1b-acceptance-cloud.ts";
import {
  authorizePairing,
  getCollectorAudit,
  getDevice,
  revokeDevice,
  updateCollectorConfig,
  type DashboardFetch,
} from "../../apps/web-dashboard/src/lib/api.ts";

const repositoryRoot = resolve(fileURLToPath(new URL("../..", import.meta.url)));
const agentHarness = process.env.PCA_S1B_ACCEPTANCE_AGENT;

if (agentHarness === undefined || agentHarness.length === 0) {
  throw new Error("PCA_S1B_ACCEPTANCE_AGENT is required");
}

async function main(): Promise<void> {
  const runtimeRoot = await mkdtemp(join(tmpdir(), "pca-s1b-acceptance-"));
  const cloud = await createS1bAcceptanceCloud();
  let pairingAgent: RunningPairingAgent | undefined;
  try {
    const dashboardFetch: DashboardFetch = (input, init) => fetch(input, init);
    await assertOldPairingInputRejected(runtimeRoot, cloud.origin);

    const pairedStatusPath = join(runtimeRoot, "paired-status.json");
    const agent = startPairingAgent(runtimeRoot, pairedStatusPath, cloud.origin);
    pairingAgent = agent;
    let handoff: S1bPairingHandoff;
    try {
      handoff = await pairingStartBeforeAgentExit(cloud.waitForPairingStart(), agent.result);
    } catch (error) {
      agent.stop();
      await agent.result;
      throw error;
    }
    const redirect = await authorizePairing(
      dashboardFetch,
      cloud.origin,
      handoff.sessionId,
      handoff.callbackState,
    );
    const callbackCode = await cloud.acceptCallback(redirect, handoff.callbackState);
    assert.notEqual(callbackCode, "accepted-callback-code");
    assert.ok(callbackCode.length >= 32, "Cloud must generate an opaque authorization code");
    agent.sendCallbackCode(callbackCode);
    const exchangedDevice = await exchangeBeforeAgentExit(cloud.waitForExchange(), agent.result);
    assert.deepEqual(cloud.inspect().pkce, {
      pairingStarts: 1,
      verifierDiffersFromCallbackState: true,
      challengeMatched: true,
    });
    const initialDevice = await getDevice(dashboardFetch, cloud.origin, exchangedDevice.deviceId);
    assert.equal(initialDevice.configuration_revision, 0);
    const revision = await updateCollectorConfig(
      dashboardFetch,
      cloud.origin,
      exchangedDevice.deviceId,
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
    const pairedProcess = await agent.result;
    assert.equal(pairedProcess.code, 0, "acceptance Agent pair-control phase must exit cleanly");
    const pairedStatusBytes = await readFile(pairedStatusPath);
    assert.deepEqual(JSON.parse(pairedStatusBytes.toString("utf8")), {
      phase: "pair-control",
      agent_status: "degraded",
      paired: true,
      applied_control_revision: 1,
      device_id: exchangedDevice.deviceId,
      workspace_id: exchangedDevice.workspaceId,
      paired_state_canary_checked: true,
    });

    const configuredDevice = await getDevice(dashboardFetch, cloud.origin, exchangedDevice.deviceId);
    assert.equal(configuredDevice.configuration_revision, 1);
    assert.equal(configuredDevice.collectors.network.enabled, true);
    const audit = await getCollectorAudit(dashboardFetch, cloud.origin, exchangedDevice.deviceId);
    assert.equal(audit.length, 1);
    assert.equal(audit[0]?.actor_user_id, cloud.owner.userId);
    assert.equal(audit[0]?.configuration_revision, 1);
    assert.equal(audit[0]?.new_config.network.enabled, true);
    assert.equal(audit[0]?.new_config["communication.wechat"].enabled, true);

    const beforeRevoke = cloud.inspect();
    assert.equal(beforeRevoke.exchangeCount, 1);
    assert.deepEqual(beforeRevoke.controlRequests, [
      { deviceId: exchangedDevice.deviceId, status: 200 },
    ]);

    const canaryResponse = await fetch(`${cloud.origin}/test/s1b/non-credential-canary`);
    assert.equal(canaryResponse.status, 200);
    await canaryResponse.json();

    await revokeDevice(dashboardFetch, cloud.origin, exchangedDevice.deviceId);
    const revokedStatusPath = join(runtimeRoot, "revoked-status.json");
    const revokedProcess = await runRevokeAgent(runtimeRoot, revokedStatusPath, cloud.origin);
    const revokedStatusBytes = await readFile(revokedStatusPath);
    assert.deepEqual(JSON.parse(revokedStatusBytes.toString("utf8")), {
      phase: "revoke",
      agent_status: "unpaired",
      paired: false,
      applied_control_revision: null,
      device_id: exchangedDevice.deviceId,
      workspace_id: exchangedDevice.workspaceId,
    });
    const revokedDevice = await getDevice(dashboardFetch, cloud.origin, exchangedDevice.deviceId);
    assert.equal(revokedDevice.revoked, true);

    const inspection = cloud.inspect();
    assert.equal(inspection.exchangeCount, 1);
    assert.deepEqual(inspection.controlRequests, [
      { deviceId: exchangedDevice.deviceId, status: 200 },
      { deviceId: exchangedDevice.deviceId, status: 401 },
    ]);
    assert.ok(inspection.sensitiveValues.length >= 5);
    const messageCanary = inspection.sensitiveValues.find((value) =>
      value.startsWith("accept-canary-")
    );
    assert.notEqual(messageCanary, undefined, "Cloud inspection must include the message canary");
    const canaryBodies = inspection.nonCredentialJson.filter((body) =>
      body.includes(Buffer.from(messageCanary as string))
    );
    assert.equal(canaryBodies.length, 1, "exactly one observed JSON body must carry the message canary");
    for (const [label, text] of [
      ["pair-control stdout", pairedProcess.stdout],
      ["pair-control stderr", pairedProcess.stderr],
      ["revoke stdout", revokedProcess.stdout],
      ["revoke stderr", revokedProcess.stderr],
    ] as const) {
      assertNoSensitiveBytes(label, Buffer.from(text), inspection.sensitiveValues);
    }
    assertNoSensitiveBytes("paired JSON status", pairedStatusBytes, inspection.sensitiveValues);
    assertNoSensitiveBytes("revoked JSON status", revokedStatusBytes, inspection.sensitiveValues);
    const nonBodySensitiveValues = inspection.sensitiveValues.filter((value) => value !== messageCanary);
    for (const [index, json] of inspection.nonCredentialJson.entries()) {
      assertNoSensitiveBytes(`Cloud/Dashboard JSON response ${index}`, json, nonBodySensitiveValues);
    }
    await assertSqliteHasNoSensitiveValues(runtimeRoot, inspection.sensitiveValues);
    await assert.rejects(stat(join(runtimeRoot, "test-keychain", "device-credential.json")));
    await assertFixtureSourcesHaveNoSensitiveValues(inspection.sensitiveValues);

    process.stdout.write("S1B shared Cloud process acceptance passed.\n");
  } finally {
    pairingAgent?.stop();
    await cloud.close();
    await rm(runtimeRoot, { recursive: true, force: true });
  }
}

async function assertOldPairingInputRejected(runtimeRoot: string, apiOrigin: string): Promise<void> {
  const sessionId = "old-node-owned-session";
  const callbackState = "old-node-owned-callback-state";
  const result = await runAgentProcess(
    "pair-control",
    join(runtimeRoot, "old-input-runtime"),
    join(runtimeRoot, "old-input-status.json"),
    `${JSON.stringify({
      api_origin: apiOrigin,
      session_id: sessionId,
      callback_state: callbackState,
    })}\n`,
  );
  assert.notEqual(result.code, 0, "the old Node-owned pairing input must be rejected");
  assertNoSensitiveBytes("rejected old-input stdout", Buffer.from(result.stdout), [sessionId, callbackState]);
  assertNoSensitiveBytes("rejected old-input stderr", Buffer.from(result.stderr), [sessionId, callbackState]);
}

interface RunningPairingAgent {
  result: Promise<ProcessResult>;
  sendCallbackCode(callbackCode: string): void;
  stop(): void;
}

function startPairingAgent(
  runtimeRoot: string,
  statusPath: string,
  apiOrigin: string,
): RunningPairingAgent {
  const child = spawn(agentHarness as string, [
    "--phase",
    "pair-control",
    "--runtime-root",
    runtimeRoot,
    "--status-file",
    statusPath,
  ], { stdio: ["pipe", "pipe", "pipe"] });
  const result = processResult(child);
  child.stdin.write(`${JSON.stringify({ api_origin: apiOrigin })}\n`);
  return {
    result,
    sendCallbackCode(callbackCode) {
      child.stdin.end(`${JSON.stringify({ callback_code: callbackCode })}\n`);
    },
    stop() {
      child.kill();
    },
  };
}

async function runRevokeAgent(
  runtimeRoot: string,
  statusPath: string,
  apiOrigin: string,
): Promise<ProcessResult> {
  const result = await runAgentProcess(
    "revoke",
    runtimeRoot,
    statusPath,
    `${JSON.stringify({ api_origin: apiOrigin })}\n`,
  );
  if (result.code !== 0) throw new Error(`acceptance Agent phase revoke exited ${result.code}`);
  return result;
}

async function pairingStartBeforeAgentExit(
  pairingStart: Promise<S1bPairingHandoff>,
  agent: Promise<ProcessResult>,
): Promise<S1bPairingHandoff> {
  let timeout: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      pairingStart,
      agent.then(() => {
        throw new Error("acceptance Agent exited before starting pairing");
      }),
      new Promise<never>((_, reject) => {
        timeout = setTimeout(
          () => reject(new Error("timed out waiting for Agent-owned pairing start")),
          2_000,
        );
      }),
    ]);
  } finally {
    if (timeout !== undefined) clearTimeout(timeout);
  }
}

async function exchangeBeforeAgentExit(
  exchange: Promise<S1bExchangedDevice>,
  agent: Promise<ProcessResult>,
): Promise<S1bExchangedDevice> {
  return Promise.race([
    exchange,
    agent.then(() => {
      throw new Error("acceptance Agent exited before exchanging credentials");
    }),
  ]);
}

interface ProcessResult {
  code: number;
  stdout: string;
  stderr: string;
}

async function runAgentProcess(
  phase: "pair-control" | "revoke",
  runtimeRoot: string,
  statusPath: string,
  input: string,
): Promise<ProcessResult> {
  const child = spawn(agentHarness as string, [
    "--phase",
    phase,
    "--runtime-root",
    runtimeRoot,
    "--status-file",
    statusPath,
  ], { stdio: ["pipe", "pipe", "pipe"] });
  const result = processResult(child);
  child.stdin.end(input);
  return result;
}

function processResult(child: ReturnType<typeof spawn>): Promise<ProcessResult> {
  return new Promise((resolveProcess, rejectProcess) => {
    let stdout = "";
    let stderr = "";
    child.stdout.setEncoding("utf8").on("data", (chunk) => { stdout += chunk; });
    child.stderr.setEncoding("utf8").on("data", (chunk) => { stderr += chunk; });
    child.once("error", rejectProcess);
    child.once("close", (code) => {
      resolveProcess({ code: code ?? -1, stdout, stderr });
    });
  });
}

function assertNoSensitiveBytes(
  label: string,
  value: Buffer,
  sensitiveValues: readonly string[],
): void {
  for (const sensitive of sensitiveValues) {
    assert.equal(value.includes(Buffer.from(sensitive)), false, `${label} contained sensitive material`);
  }
}

async function assertSqliteHasNoSensitiveValues(
  runtimeRoot: string,
  sensitiveValues: readonly string[],
): Promise<void> {
  for (const path of await filesRecursively(runtimeRoot)) {
    const name = path.slice(path.lastIndexOf("/") + 1);
    if (name !== "agent.sqlite" && name !== "agent.sqlite-wal" && name !== "agent.sqlite-shm") {
      continue;
    }
    assertNoSensitiveBytes(`SQLite artifact ${name}`, await readFile(path), sensitiveValues);
  }
}

async function assertFixtureSourcesHaveNoSensitiveValues(
  sensitiveValues: readonly string[],
): Promise<void> {
  const roots = [
    join(repositoryRoot, "scripts", "tests"),
    join(repositoryRoot, "agent", "core", "tests", "support"),
    join(repositoryRoot, "apps", "cloud-api", "src", "test", "support"),
  ];
  const paths = new Set<string>();
  for (const root of roots) {
    for (const path of await filesRecursively(root)) paths.add(path);
  }
  for (const path of await fixtureFiles(repositoryRoot)) paths.add(path);
  for (const path of paths) {
    assertNoSensitiveBytes(`fixture source ${path}`, await readFile(path), sensitiveValues);
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

async function filesRecursively(directory: string): Promise<string[]> {
  const paths: string[] = [];
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) paths.push(...await filesRecursively(path));
    else paths.push(path);
  }
  return paths;
}

main().catch((error: unknown) => {
  const message = error instanceof Error ? error.message : "unknown acceptance failure";
  process.stderr.write(`S1B process acceptance failed: ${message}\n`);
  process.exitCode = 1;
});
