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
