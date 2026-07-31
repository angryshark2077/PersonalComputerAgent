import { createHash, randomBytes, randomUUID } from "node:crypto";

import type { Context } from "hono";

import { validateContract } from "@pca/contracts/src/validate.js";
import type { DevicePairingExchange, DevicePairingStart } from "@pca/contracts/src/types.js";

export const pairingSessionLifetimeMs = 5 * 60 * 1000;
export const accessCredentialLifetimeMs = 60 * 60 * 1000;
export const refreshCredentialLifetimeMs = 30 * 24 * 60 * 60 * 1000;
export const pairingRateWindowMs = 60 * 1000;
export const pairingRateMaxPerIp = 10;
export const pairingRateMaxPerDeviceKey = 3;
const pairingRateMaxBuckets = 4096;

export function hashSecret(value: string): string {
  return createHash("sha256").update(value).digest("hex");
}

export function pkceChallenge(verifier: string): string {
  return createHash("sha256").update(verifier).digest("base64url");
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

interface RateBucket {
  startedAt: number;
  count: number;
}

export class PairingRateLimiter {
  readonly #ip = new Map<string, RateBucket>();
  readonly #deviceKey = new Map<string, RateBucket>();

  check(ip: string, deviceKeyHash: string, now = Date.now()): number | null {
    this.#prune(now);
    const ipRetry = this.#consume(this.#ip, ip, pairingRateMaxPerIp, now);
    const deviceRetry = this.#consume(
      this.#deviceKey,
      deviceKeyHash,
      pairingRateMaxPerDeviceKey,
      now,
    );
    if (ipRetry === null && deviceRetry === null) {
      return null;
    }
    return Math.max(ipRetry ?? 0, deviceRetry ?? 0);
  }

  #consume(
    buckets: Map<string, RateBucket>,
    key: string,
    maximum: number,
    now: number,
  ): number | null {
    const bucket = buckets.get(key);
    if (bucket === undefined || bucket.startedAt + pairingRateWindowMs <= now) {
      if (buckets.size >= pairingRateMaxBuckets) {
        buckets.delete(buckets.keys().next().value as string);
      }
      buckets.set(key, { startedAt: now, count: 1 });
      return null;
    }
    bucket.count += 1;
    return bucket.count > maximum ? Math.ceil((bucket.startedAt + pairingRateWindowMs - now) / 1000) : null;
  }

  #prune(now: number): void {
    for (const [key, bucket] of this.#ip) {
      if (bucket.startedAt + pairingRateWindowMs <= now) this.#ip.delete(key);
    }
    for (const [key, bucket] of this.#deviceKey) {
      if (bucket.startedAt + pairingRateWindowMs <= now) this.#deviceKey.delete(key);
    }
  }
}

export function errorResponse(
  context: Context,
  status: 400 | 401 | 403 | 409 | 410 | 429,
  errorCode: string,
  message: string,
): Response {
  return context.json(
    { error: { error_code: errorCode, message, retryable: false } },
    status,
  );
}
