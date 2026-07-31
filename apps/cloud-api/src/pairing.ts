import { createHash, randomBytes, randomUUID } from "node:crypto";

import type { Context } from "hono";

import { validateContract } from "@pca/contracts/src/validate.js";
import type { DevicePairingExchange, DevicePairingStart } from "@pca/contracts/src/types.js";

export const pairingSessionLifetimeMs = 5 * 60 * 1000;
export const accessCredentialLifetimeMs = 60 * 60 * 1000;
export const refreshCredentialLifetimeMs = 30 * 24 * 60 * 60 * 1000;

export function hashSecret(value: string): string {
  return createHash("sha256").update(value).digest("hex");
}

export function opaqueCredential(): string {
  return randomBytes(32).toString("base64url");
}

export function opaqueSessionId(): string {
  return randomUUID();
}

export function parsePairingStart(value: unknown): DevicePairingStart | null {
  return validateContract("device-pairing", value).valid ? (value as DevicePairingStart) : null;
}

export function parsePairingExchange(value: unknown): DevicePairingExchange | null {
  return validateContract("device-pairing", value).valid ? (value as DevicePairingExchange) : null;
}

export function errorResponse(
  context: Context,
  status: 400 | 401 | 403 | 409 | 410,
  errorCode: string,
  message: string,
): Response {
  return context.json(
    { error: { error_code: errorCode, message, retryable: false } },
    status,
  );
}
