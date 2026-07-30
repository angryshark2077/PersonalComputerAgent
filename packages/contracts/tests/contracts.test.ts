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
