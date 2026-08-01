import { serve } from "@hono/node-server";

import { createProductionApp } from "./index.js";

export function parseListenPort(value: string | undefined): number {
  const port = Number(value);
  if (!Number.isInteger(port) || port < 1 || port > 65_535) throw new Error("invalid PORT");
  return port;
}

export function startProductionServer(environment = process.env) {
  const app = createProductionApp(environment);
  return serve({
    fetch: app.fetch,
    hostname: "0.0.0.0",
    port: parseListenPort(environment.PORT),
  });
}

if (process.env.NODE_TEST_CONTEXT === undefined) startProductionServer();
