import assert from "node:assert/strict";
import test from "node:test";

import { getLifecycleEvents, getSystemMetrics } from "../src/lib/api.ts";
import { summarizeSystemMetrics } from "../src/lib/system-metrics.ts";

test("loads device system metrics from the owner-scoped endpoint", async () => {
  let requested = "";
  const metrics = await getSystemMetrics(async (input) => {
    requested = String(input);
    return new Response(JSON.stringify({ metrics: [] }), { status: 200 });
  }, "https://cloud.example", "01982222-7222-8222-8222-222222222222");

  assert.deepEqual(metrics, []);
  assert.equal(
    requested,
    "https://cloud.example/v1/devices/01982222-7222-8222-8222-222222222222/system-metrics",
  );
});

test("loads paginated lifecycle history from the device endpoint", async () => {
  let requested = "";
  const page = await getLifecycleEvents(async (input) => {
    requested = String(input);
    return new Response(JSON.stringify({
      events: [],
      pagination: { limit: 20, offset: 20, total_count: 37 },
    }), { status: 200 });
  }, "https://cloud.example", "01982222-7222-8222-8222-222222222222", 20, 20);

  assert.equal(page.pagination.total_count, 37);
  assert.equal(
    requested,
    "https://cloud.example/v1/devices/01982222-7222-8222-8222-222222222222/lifecycle-events?limit=20&offset=20",
  );
});

test("summarizes the latest CPU, memory, and disk metrics for the device page", () => {
  const summary = summarizeSystemMetrics([
    {
      event_id: "cpu",
      occurred_at: "2026-08-02T00:00:00Z",
      metric_group: "cpu_memory",
      payload: {
        metric_group: "cpu_memory",
        host: {
          cpu_usage_percent: 12.34,
          memory_total_bytes: 34_359_738_368,
          memory_used_bytes: 17_179_869_184,
        },
      },
    },
    {
      event_id: "disk",
      occurred_at: "2026-08-02T00:00:00Z",
      metric_group: "disk",
      payload: {
        metric_group: "disk",
        total_bytes: 128_849_018_880,
        available_bytes: 64_424_509_440,
        used_percent: 50,
      },
    },
  ]);

  assert.deepEqual(summary, {
    cpu: "12.3%",
    memory: "16.0 GiB / 32.0 GiB",
    disk: "60.0 GiB available of 120.0 GiB (50.0% used)",
  });
});
