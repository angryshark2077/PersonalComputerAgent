import assert from "node:assert/strict";
import test from "node:test";

import { collectorHealthPresentation } from "../src/lib/collector-health.ts";
import type { DashboardCollectorHealth } from "../src/lib/api.ts";

function health(overrides: Partial<DashboardCollectorHealth> = {}): DashboardCollectorHealth {
  return {
    collector_key: "communication.wechat",
    collector_version: "0.1.133",
    status: "running",
    desired_config_revision: 5,
    applied_config_revision: 5,
    last_event_at: "2026-08-11T18:11:53.000Z",
    last_health_at: "2026-08-14T00:20:00.000Z",
    error_code: null,
    reported_at: "2026-08-14T00:30:00.000Z",
    agent_version: "0.1.133",
    ...overrides,
  };
}

test("collector health explains a concrete failure code", () => {
  const result = collectorHealthPresentation(health({ status: "degraded", error_code: "WECHAT_KEY_REJECTED" }), Date.parse("2026-08-14T00:40:00Z"));
  assert.equal(result.alert, true);
  assert.match(result.reason ?? "", /stored WeChat key was rejected/);
  assert.match(result.reason ?? "", /WECHAT_KEY_REJECTED/);
});

test("collector health explains a missing enabled Network observation", () => {
  const result = collectorHealthPresentation(health({
    collector_key: "network",
    status: "degraded",
    error_code: "NETWORK_OBSERVATION_UNAVAILABLE",
  }), Date.parse("2026-08-14T00:40:00Z"));
  assert.equal(result.alert, true);
  assert.match(result.reason ?? "", /no current network observation/);
});

test("collector health marks a missed 30-minute report as overdue", () => {
  const result = collectorHealthPresentation(health(), Date.parse("2026-08-14T02:00:01Z"));
  assert.equal(result.alert, true);
  assert.equal(result.label, "Health report overdue");
});
