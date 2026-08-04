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
  ) {}

  async locate(observedExitIp: string): Promise<IpLocation | null> {
    if (isIP(observedExitIp) === 0) return null;
    const cached = this.#cache.get(observedExitIp);
    if (cached !== undefined && cached.expiresAt > Date.now()) return cached.value;
    const url = new URL(`https://api.country.is/${encodeURIComponent(observedExitIp)}`);
    url.searchParams.set("fields", "city,subdivision");
    const response = await this.request(url, { headers: { accept: "application/json" } });
    if (!response.ok) {
      if (response.status === 400 || response.status === 404) {
        this.#cache.set(observedExitIp, { expiresAt: Date.now() + this.cacheMilliseconds, value: null });
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
    this.#cache.set(observedExitIp, { expiresAt: Date.now() + this.cacheMilliseconds, value });
    return value;
  }
}

function textOrNull(value: unknown): string | null {
  return typeof value === "string" && value.length > 0 && value.length <= 100 ? value : null;
}
