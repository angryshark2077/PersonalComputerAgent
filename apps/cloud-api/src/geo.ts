import { isIP } from "node:net";

import type { IpLocation } from "@pca/db-cloud/src/repository.js";

export interface GeoEnrichmentPort {
  locate(observedExitIp: string): Promise<IpLocation | null>;
}

interface CountryResponse {
  country?: unknown;
  subdivision?: unknown;
  city?: unknown;
}

export class CountryIsGeoEnricher implements GeoEnrichmentPort {
  readonly #cache = new Map<string, { expiresAt: number; value: IpLocation | null }>();

  constructor(
    private readonly request: typeof fetch = fetch,
    private readonly cacheMilliseconds = 60 * 60 * 1000,
    private readonly maxCacheEntries = 1_024,
    private readonly requestTimeoutMilliseconds = 3_000,
  ) {}

  async locate(observedExitIp: string): Promise<IpLocation | null> {
    if (isIP(observedExitIp) === 0) return null;
    const cached = this.#cache.get(observedExitIp);
    if (cached !== undefined && cached.expiresAt > Date.now()) return cached.value;
    if (cached !== undefined) this.#cache.delete(observedExitIp);
    const url = new URL(`https://api.country.is/${encodeURIComponent(observedExitIp)}`);
    url.searchParams.set("fields", "city,subdivision");
    const response = await this.request(url, {
      headers: { accept: "application/json" },
      signal: AbortSignal.timeout(this.requestTimeoutMilliseconds),
    });
    if (!response.ok) {
      if (response.status === 400 || response.status === 404) {
        this.#setCached(observedExitIp, null);
        return null;
      }
      throw new Error("geo provider unavailable");
    }
    const body = await response.json() as CountryResponse;
    const value: IpLocation = {
      country: textOrNull(body.country),
      region: textOrNull(body.subdivision),
      city: textOrNull(body.city),
      accuracy: "ip_city",
    };
    this.#setCached(observedExitIp, value);
    return value;
  }

  #setCached(observedExitIp: string, value: IpLocation | null): void {
    const now = Date.now();
    for (const [key, cached] of this.#cache) {
      if (cached.expiresAt <= now) this.#cache.delete(key);
    }
    this.#cache.delete(observedExitIp);
    while (this.#cache.size >= Math.max(1, this.maxCacheEntries)) {
      const oldest = this.#cache.keys().next().value as string | undefined;
      if (oldest === undefined) break;
      this.#cache.delete(oldest);
    }
    this.#cache.set(observedExitIp, { expiresAt: now + this.cacheMilliseconds, value });
  }
}

function textOrNull(value: unknown): string | null {
  return typeof value === "string" && value.length > 0 && value.length <= 100 ? value : null;
}
