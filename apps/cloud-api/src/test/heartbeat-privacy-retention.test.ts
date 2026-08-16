import assert from "node:assert/strict";
import test from "node:test";

import {
  heartbeatPrivacyRetentionMilliseconds,
  runHeartbeatPrivacyRetentionPass,
} from "../heartbeat-privacy-retention.js";

test("privacy retention drains bounded batches independently from heartbeats", async () => {
  const now = new Date("2026-08-17T12:00:00.000Z");
  const calls: Array<{ capturedBefore: Date; limit: number }> = [];
  const counts = [1000, 12];
  const redacted = await runHeartbeatPrivacyRetentionPass(
    {
      redactExpiredHeartbeatNetworkData: async (capturedBefore, limit) => {
        calls.push({ capturedBefore, limit });
        return counts.shift() ?? 0;
      },
    },
    now,
  );

  assert.equal(redacted, 1012);
  assert.equal(calls.length, 2);
  assert.equal(calls[0]?.limit, 1000);
  assert.equal(
    calls[0]?.capturedBefore.toISOString(),
    new Date(now.getTime() - heartbeatPrivacyRetentionMilliseconds).toISOString(),
  );
});
