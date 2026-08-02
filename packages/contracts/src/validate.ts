import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { Ajv2020, type ErrorObject, type ValidateFunction } from "ajv/dist/2020.js";

export type ContractSchemaName =
  | "bridge-envelope"
  | "collector-state"
  | "command-envelope"
  | "error-envelope"
  | "event-envelope"
  | "system-metric-sampled"
  | "collector-status-changed"
  | "system-health-changed"
  | "sync-batch-request"
  | "sync-batch-response"
  | "wechat-provider-state"
  | "device-pairing"
  | "agent-control-snapshot"
  | "dashboard-control"
  | "communication-conversation-observed"
  | "communication-message-sender-observed"
  | "communication-message-recorded"
  | "communication-object";

export interface ValidationResult {
  valid: boolean;
  errors: string[];
}

const contractRoot = dirname(dirname(fileURLToPath(import.meta.url)));
const schemaNames: ContractSchemaName[] = [
  "bridge-envelope",
  "collector-state",
  "command-envelope",
  "error-envelope",
  "event-envelope",
  "system-metric-sampled",
  "collector-status-changed",
  "system-health-changed",
  "sync-batch-request",
  "sync-batch-response",
  "wechat-provider-state",
  "device-pairing",
  "agent-control-snapshot",
  "dashboard-control",
  "communication-conversation-observed",
  "communication-message-sender-observed",
  "communication-message-recorded",
  "communication-object",
];

const ajv = new Ajv2020({
  allErrors: true,
  strict: true,
  formats: {
    uuid: /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i,
    "date-time": {
      type: "string",
      validate: (value: string) => !Number.isNaN(Date.parse(value)),
    },
  },
});

const schemas = new Map<ContractSchemaName, Record<string, unknown>>();
for (const name of schemaNames) {
  const schema = JSON.parse(
    readFileSync(join(contractRoot, `${name}.schema.json`), "utf8"),
  ) as Record<string, unknown>;
  schemas.set(name, schema);
  ajv.addSchema(schema);
}

const validators = new Map<ContractSchemaName, ValidateFunction>();
for (const name of schemaNames) {
  const schema = schemas.get(name);
  if (schema === undefined) {
    throw new Error(`schema was not registered: ${name}`);
  }
  validators.set(name, ajv.getSchema(String(schema.$id)) ?? ajv.compile(schema));
}

function formatError(error: ErrorObject): string {
  const location = error.instancePath || "/";
  return `${location} ${error.message ?? "is invalid"}`;
}

export function validateContract(
  schemaName: ContractSchemaName,
  value: unknown,
): ValidationResult {
  const validate = validators.get(schemaName);
  if (validate === undefined) {
    return { valid: false, errors: [`unknown contract schema: ${schemaName}`] };
  }

  if (!validate(value)) {
    return {
      valid: false,
      errors: (validate.errors ?? []).map(formatError),
    };
  }

  const relationshipErrors =
    schemaName === "system-metric-sampled"
      ? validateSystemMetricRelationships(value)
      : schemaName === "collector-status-changed"
        ? validateCollectorStatusRelationships(value)
        : [];

  return relationshipErrors.length === 0
    ? { valid: true, errors: [] }
    : { valid: false, errors: relationshipErrors };
}

function validateSystemMetricRelationships(value: unknown): string[] {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    return [];
  }

  const payload = value as Record<string, unknown>;
  if (payload.metric_group === "cpu_memory") {
    const host = payload.host as {
      memory_total_bytes: number;
      memory_used_bytes: number;
    };
    return host.memory_used_bytes > host.memory_total_bytes
      ? ["/host/memory_used_bytes must not exceed memory_total_bytes"]
      : [];
  }

  if (payload.metric_group !== "disk") {
    return [];
  }

  const totalBytes = payload.total_bytes as number;
  const availableBytes = payload.available_bytes as number;
  const thresholdBytes = payload.low_space_threshold_bytes as number;
  const lowSpace = payload.low_space as boolean;
  const warningCode = payload.warning_code as string | null;
  const usedPercent = payload.used_percent as number;
  const errors: string[] = [];

  if (availableBytes > totalBytes) {
    errors.push("/available_bytes must not exceed total_bytes");
  }
  if (lowSpace !== (availableBytes < thresholdBytes)) {
    errors.push("/low_space must match the available_bytes threshold");
  }
  if (warningCode !== (lowSpace ? "DISK_SPACE_LOW" : null)) {
    errors.push("/warning_code must match low_space");
  }
  if (Math.abs(usedPercent - ((totalBytes - availableBytes) / totalBytes) * 100) > 0.01) {
    errors.push("/used_percent must match total_bytes and available_bytes");
  }
  return errors;
}

function validateCollectorStatusRelationships(value: unknown): string[] {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    return [];
  }

  const payload = value as Record<string, unknown>;
  const expectedErrorCode: Record<string, string | null> = {
    disabled: null,
    permission_required: null,
    initializing: null,
    running: null,
    paused: null,
    degraded: "COLLECTOR_DEGRADED",
    unsupported: "COLLECTOR_UNSUPPORTED",
    error: "COLLECTOR_INIT_FAILED",
  };
  const expected = expectedErrorCode[payload.status as string];
  return expected === undefined || payload.error_code === expected
    ? []
    : ["/error_code must match status"];
}
