import assert from "node:assert/strict";
import test from "node:test";

import { createRailwayClientAddress } from "../index.js";
import { CountryIsGeoEnricher } from "../geo.js";

test("Railway client address trusts only its documented X-Real-IP in Railway", () => {
  const production = createRailwayClientAddress({ RAILWAY_ENVIRONMENT: "production" });
  assert.equal(production(new Request("https://api.example", {
    headers: { "x-real-ip": "203.0.113.7", "x-forwarded-for": "198.51.100.5" },
  })), "203.0.113.7");
  assert.equal(production(new Request("https://api.example", {
    headers: { "x-real-ip": "not-an-ip" },
  })), undefined);
  const local = createRailwayClientAddress({});
  assert.equal(local(new Request("https://api.example", {
    headers: { "x-real-ip": "203.0.113.7" },
  })), undefined);
});

test("country.is adapter keeps only city-level fields and caches by public IP", async () => {
  let calls = 0;
  const adapter = new CountryIsGeoEnricher(async (input) => {
    calls += 1;
    assert.match(String(input), /^https:\/\/api\.country\.is\/203\.0\.113\.7\?fields=city%2Csubdivision$/);
    return new Response(JSON.stringify({
      country: "SG",
      subdivision: "Singapore",
      city: "Singapore",
      location: { latitude: 1.3, longitude: 103.8 },
    }), { status: 200, headers: { "content-type": "application/json" } });
  });
  const expected = { country: "SG", region: "Singapore", city: "Singapore", accuracy: "ip_city" };
  assert.deepEqual(await adapter.locate("203.0.113.7"), expected);
  assert.deepEqual(await adapter.locate("203.0.113.7"), expected);
  assert.equal(calls, 1);
});

test("country.is adapter bounds request time and cache cardinality", async () => {
  let calls = 0;
  const adapter = new CountryIsGeoEnricher(async (_input, init) => {
    calls += 1;
    assert.ok(init?.signal instanceof AbortSignal);
    return new Response(JSON.stringify({ country: "SG" }), { status: 200 });
  }, 60_000, 2, 1_000);

  await adapter.locate("203.0.113.1");
  await adapter.locate("203.0.113.2");
  await adapter.locate("203.0.113.3");
  await adapter.locate("203.0.113.1");
  assert.equal(calls, 4, "oldest IP must be evicted once the cache reaches its bound");
});

test("country.is adapter aborts a stalled request", async () => {
  const adapter = new CountryIsGeoEnricher((_input, init) => new Promise((_resolve, reject) => {
    init?.signal?.addEventListener("abort", () => reject(init.signal?.reason), { once: true });
  }), 60_000, 2, 1);
  await assert.rejects(adapter.locate("203.0.113.7"), /timeout|aborted/i);
});
