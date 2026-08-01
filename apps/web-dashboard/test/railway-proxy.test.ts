import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import test from "node:test";

import { createNextConfig } from "../next.config.ts";
import {
  dashboardHealthResponse,
  requireBuildProxyOrigin,
  validateDashboardEnvironment,
} from "../src/lib/railway-environment.ts";

const validPrivateOrigin = "http://pca-cloud-api.railway.internal:8080";

test("Railway proxy rewrites auth and control paths to the private API", async () => {
  const config = await createNextConfig(validPrivateOrigin);

  assert.deepEqual(await config.rewrites(), [
    {
      source: "/api/auth/:path*",
      destination: "http://pca-cloud-api.railway.internal:8080/api/auth/:path*",
    },
    {
      source: "/v1/:path*",
      destination: "http://pca-cloud-api.railway.internal:8080/v1/:path*",
    },
  ]);
});

test("Dashboard rejects a public browser Cloud API origin", () => {
  assert.throws(
    () => validateDashboardEnvironment({ NEXT_PUBLIC_CLOUD_API_ORIGIN: "https://api.invalid" }),
    /same-origin/,
  );
});

test("production build rejects missing private API origin", () => {
  assert.throws(
    () => requireBuildProxyOrigin({ NODE_ENV: "production" }),
    /CLOUD_API_INTERNAL_ORIGIN/,
  );
});

test("production build rejects private API origins without an explicit valid port", () => {
  for (const origin of [
    "http://pca-cloud-api.railway.internal",
    "http://pca-cloud-api.railway.internal:0",
    "http://pca-cloud-api.railway.internal:65536",
    "https://pca-cloud-api.railway.internal:8080",
    "http://pca-cloud-api.railway.internal:8080/path",
    "http://user:pca-cloud-api.railway.internal:8080",
  ]) {
    assert.throws(
      () => requireBuildProxyOrigin({ CLOUD_API_INTERNAL_ORIGIN: origin }),
      /CLOUD_API_INTERNAL_ORIGIN/,
    );
  }
});

test("Dashboard health cannot become ready from a runtime-only origin", async () => {
  const build = createNextConfig(validPrivateOrigin);
  assert.equal(typeof build.rewrites, "function");

  const ready = dashboardHealthResponse();
  assert.equal(ready.status, 200);
  assert.deepEqual(await ready.json(), { status: "ok" });
});

test("production build requires the Dashboard private API origin", () => {
  const dashboardDirectory = new URL("..", import.meta.url).pathname;
  const baseEnvironment = { ...process.env, NODE_ENV: "production" };
  delete baseEnvironment.CLOUD_API_INTERNAL_ORIGIN;

  assert.throws(
    () => execFileSync("pnpm", ["build"], { cwd: dashboardDirectory, env: baseEnvironment, stdio: "pipe" }),
    /CLOUD_API_INTERNAL_ORIGIN/,
  );

  execFileSync("pnpm", ["build"], {
    cwd: dashboardDirectory,
    env: { ...baseEnvironment, CLOUD_API_INTERNAL_ORIGIN: validPrivateOrigin },
    stdio: "pipe",
  });
});

test("built Dashboard health stays ready without a runtime private API origin", async (t) => {
  const dashboardDirectory = new URL("..", import.meta.url).pathname;
  const buildEnvironment = {
    ...process.env,
    NODE_ENV: "production",
    CLOUD_API_INTERNAL_ORIGIN: validPrivateOrigin,
  };
  execFileSync("pnpm", ["build"], { cwd: dashboardDirectory, env: buildEnvironment, stdio: "pipe" });

  const runtimeEnvironment = { ...process.env, NODE_ENV: "production" };
  delete runtimeEnvironment.CLOUD_API_INTERNAL_ORIGIN;
  const server = spawn("pnpm", ["exec", "next", "start", "--hostname", "127.0.0.1", "--port", "31234"], {
    cwd: dashboardDirectory,
    env: runtimeEnvironment,
    stdio: ["ignore", "pipe", "pipe"],
  });
  t.after(() => server.kill("SIGTERM"));

  let output = "";
  const ready = new Promise<void>((resolve, reject) => {
    const onOutput = (chunk: Buffer) => {
      output += chunk.toString();
      if (output.includes("Ready")) {
        resolve();
      }
    };
    server.stdout.on("data", onOutput);
    server.stderr.on("data", onOutput);
    server.once("exit", (code) => reject(new Error(`next start exited ${code}: ${output}`)));
  });

  await ready;
  const response = await fetch("http://127.0.0.1:31234/healthz");
  assert.equal(response.status, 200);
  assert.deepEqual(await response.json(), { status: "ok" });
});
