import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import { validateContract } from "../src/validate.js";

const here = dirname(fileURLToPath(import.meta.url));
const fixture = (name: string): unknown =>
  JSON.parse(readFileSync(join(here, "../fixtures", name), "utf8"));

test("valid Bridge request satisfies the canonical schema", () => {
  assert.deepEqual(
    validateContract("bridge-envelope", fixture("bridge-request.valid.json")),
    { valid: true, errors: [] },
  );
});

test("Bridge request without request_id is rejected", () => {
  const result = validateContract(
    "bridge-envelope",
    fixture("bridge-request.missing-request-id.json"),
  );
  assert.equal(result.valid, false);
  assert.match(result.errors.join("\n"), /request_id/);
});

test("Bridge payload is an object rather than encoded bytes", () => {
  const value = fixture("bridge-request.valid.json") as { payload: unknown };
  assert.equal(Array.isArray(value.payload), false);
  assert.equal(typeof value.payload, "object");
});

test("valid Event fixture satisfies the canonical schema", () => {
  assert.deepEqual(validateContract("event-envelope", fixture("event.valid.json")), {
    valid: true,
    errors: [],
  });
});

test("System metric payloads are strict discriminated unions", () => {
  for (const name of [
    "system-metric.cpu-memory.valid.json",
    "system-metric.disk.valid.json",
  ]) {
    assert.deepEqual(
      validateContract("system-metric-sampled", fixture(name)),
      { valid: true, errors: [] },
    );
  }

  assert.equal(
    validateContract(
      "system-metric-sampled",
      fixture("system-metric.invalid-percent.json"),
    ).valid,
    false,
  );
  assert.equal(
    validateContract(
      "system-metric-sampled",
      fixture("system-metric.invalid-unknown-field.json"),
    ).valid,
    false,
  );
});

test("System metric payloads reject inconsistent relationships", () => {
  const cpu = fixture("system-metric.cpu-memory.valid.json") as {
    host: { memory_total_bytes: number; memory_used_bytes: number };
  };
  cpu.host.memory_used_bytes = cpu.host.memory_total_bytes + 1;
  assert.equal(validateContract("system-metric-sampled", cpu).valid, false);

  const diskWithExtraSpace = fixture("system-metric.disk.valid.json") as {
    total_bytes: number;
    available_bytes: number;
  };
  diskWithExtraSpace.available_bytes = diskWithExtraSpace.total_bytes + 1;
  assert.equal(
    validateContract("system-metric-sampled", diskWithExtraSpace).valid,
    false,
  );

  const diskWithMismatchedLowSpace = fixture("system-metric.disk.valid.json") as {
    low_space: boolean;
  };
  diskWithMismatchedLowSpace.low_space = true;
  assert.equal(
    validateContract("system-metric-sampled", diskWithMismatchedLowSpace).valid,
    false,
  );

  const diskWithMismatchedWarning = fixture("system-metric.disk.valid.json") as {
    warning_code: string | null;
  };
  diskWithMismatchedWarning.warning_code = "DISK_SPACE_LOW";
  assert.equal(
    validateContract("system-metric-sampled", diskWithMismatchedWarning).valid,
    false,
  );

  const diskWithMismatchedPercent = fixture("system-metric.disk.valid.json") as {
    used_percent: number;
  };
  diskWithMismatchedPercent.used_percent = 50.02;
  assert.equal(
    validateContract("system-metric-sampled", diskWithMismatchedPercent).valid,
    false,
  );
});

test("Collector status and System health payloads validate", () => {
  assert.equal(
    validateContract(
      "collector-status-changed",
      fixture("collector-status.valid.json"),
    ).valid,
    true,
  );
  assert.equal(
    validateContract("system-health-changed", fixture("system-health.active.json"))
      .valid,
    true,
  );
});

test("Collector status requires the status-specific error code", () => {
  const degraded = fixture("collector-status.valid.json") as {
    status: string;
    error_code: string | null;
    reason: string;
  };
  degraded.status = "degraded";
  degraded.reason = "sampling_failed";
  degraded.error_code = null;
  assert.equal(validateContract("collector-status-changed", degraded).valid, false);
});

test("registry contains every Appendix C enum group without duplicates", () => {
  const registry = JSON.parse(
    readFileSync(join(here, "../registry.json"), "utf8"),
  ) as { enums: Record<string, string[]> };

  assert.equal(Object.keys(registry.enums).length, 16);
  for (const [name, values] of Object.entries(registry.enums)) {
    assert.equal(new Set(values).size, values.length, `${name} contains duplicates`);
  }
});

test("registry contains the complete Appendix D error-code baseline", () => {
  const registry = JSON.parse(
    readFileSync(join(here, "../registry.json"), "utf8"),
  ) as { error_codes: string[] };

  assert.equal(registry.error_codes.length, 57);
  assert.equal(new Set(registry.error_codes).size, 57);
  assert.ok(registry.error_codes.includes("AUTH_REQUIRED"));
  assert.ok(registry.error_codes.includes("DISK_SPACE_LOW"));
});

test("local runtime status uses the canonical health fields", () => {
  const value = fixture("runtime-status.local-healthy.json") as Record<string, unknown>;
  assert.equal(value.agent_status, "unpaired");
  assert.equal(value.bridge_status, "ready");
  assert.equal(value.local_healthy, true);
  assert.equal(typeof value.heartbeat_at, "string");
});

test("handshake fixtures never carry the shared secret", () => {
  for (const name of [
    "bridge-handshake.challenge.json",
    "bridge-handshake.response.json",
  ]) {
    const value = JSON.stringify(fixture(name));
    assert.equal(value.includes("shared_secret"), false);
  }
});
