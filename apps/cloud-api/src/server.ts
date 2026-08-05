import { serve } from "@hono/node-server";

import { createProductionRuntime } from "./index.js";
import { startCommunicationObjectRecovery } from "./communication-object-recovery.js";
import { startScreenshotRetentionWorker } from "./screenshot-retention.js";

export function parseListenPort(value: string | undefined): number {
  const port = Number(value);
  if (!Number.isInteger(port) || port < 1 || port > 65_535) throw new Error("invalid PORT");
  return port;
}

export function startProductionServer(environment = process.env) {
  const runtime = createProductionRuntime(environment);
  const server = serve({
    fetch: runtime.app.fetch,
    hostname: "0.0.0.0",
    port: parseListenPort(environment.PORT),
  });
  const retention = runtime.objectStore === undefined
    ? null
    : startScreenshotRetentionWorker(runtime.repository, runtime.objectStore);
  if (runtime.objectStore !== undefined) {
    startCommunicationObjectRecovery(runtime.repository, runtime.objectStore);
  }
  server.once("close", () => retention?.stop());
  return server;
}

if (process.env.NODE_TEST_CONTEXT === undefined) startProductionServer();
