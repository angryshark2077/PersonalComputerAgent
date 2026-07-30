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
  | "sync-batch-request"
  | "sync-batch-response"
  | "wechat-provider-state";

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
  "sync-batch-request",
  "sync-batch-response",
  "wechat-provider-state",
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

  if (validate(value)) {
    return { valid: true, errors: [] };
  }

  return {
    valid: false,
    errors: (validate.errors ?? []).map(formatError),
  };
}
