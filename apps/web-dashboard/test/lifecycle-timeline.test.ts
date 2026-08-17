import assert from "node:assert/strict";
import test from "node:test";

import { buildLifecycleTimeline } from "../src/lib/lifecycle-timeline.ts";

test("pairs sleep and wake events without displaying dark wakes", () => {
  const timeline = buildLifecycleTimeline([
    event("wake", "system.wake", "2026-08-17T10:53:25Z"),
    event("sleep", "system.sleep", "2026-08-16T21:10:25Z"),
  ]);
  assert.equal(timeline.length, 1);
  assert.equal(timeline[0]?.title, "Sleep");
  assert.match(timeline[0]?.detail ?? "", /13h 43m/);
});

test("merges consecutive sleep events into one period ending at the next wake", () => {
  const timeline = buildLifecycleTimeline([
    event("wake", "system.wake", "2026-08-17T10:53:25Z"),
    event("sleep-2", "system.sleep", "2026-08-16T21:10:25Z"),
    event("sleep-1", "system.sleep", "2026-08-16T11:07:52Z"),
  ]);
  assert.equal(timeline.length, 1);
  assert.equal(timeline[0]?.title, "Sleep");
  // 23h45m proves the period starts at the FIRST sleep (21:10:25Z start would be 13h43m).
  assert.match(timeline[0]?.detail ?? "", /23h 45m/);
});

test("keeps the first sleep visible when no matching wake has arrived", () => {
  const timeline = buildLifecycleTimeline([
    event("sleep-2", "system.sleep", "2026-08-17T21:10:25Z"),
    event("sleep-1", "system.sleep", "2026-08-17T11:07:52Z"),
  ]);
  assert.equal(timeline.length, 1);
  assert.equal(timeline[0]?.title, "Entered sleep");
  assert.equal(timeline[0]?.detail, "No matching wake event yet");
  assert.equal(timeline[0]?.occurredAt, "2026-08-17T11:07:52Z");
});

test("distinguishes an Agent restart from a macOS reboot when boot time is available", () => {
  const timeline = buildLifecycleTimeline([
    event("start-2", "agent.started", "2026-08-17T05:07:48Z", "2026-08-16T23:00:00Z"),
    event("stop", "agent.stopped", "2026-08-17T05:07:45Z"),
    event("start-1", "agent.started", "2026-08-17T04:00:00Z", "2026-08-16T23:00:00Z"),
  ]);
  assert.equal(timeline[0]?.title, "Agent restarted");

  const reboot = buildLifecycleTimeline([
    event("start-2", "agent.started", "2026-08-17T05:07:48Z", "2026-08-17T05:00:00Z"),
    event("stop", "agent.stopped", "2026-08-17T04:59:45Z"),
    event("start-1", "agent.started", "2026-08-16T04:00:00Z", "2026-08-16T03:55:00Z"),
  ]);
  assert.equal(reboot[0]?.title, "Mac restarted");
});

function event(
  eventId: string,
  eventType: "agent.started" | "agent.stopped" | "agent.crash_recovered" | "system.sleep" | "system.wake",
  occurredAt: string,
  bootTime: string | null = null,
) {
  return { event_id: eventId, event_type: eventType, occurred_at: occurredAt, boot_time: bootTime };
}
