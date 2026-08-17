import type { DashboardLifecycleEvent } from "./api";

export interface LifecycleTimelineItem {
  key: string;
  title: string;
  detail: string | null;
  occurredAt: string;
}

export function buildLifecycleTimeline(events: DashboardLifecycleEvent[]): LifecycleTimelineItem[] {
  const ordered = [...events].sort((left, right) => Date.parse(left.occurred_at) - Date.parse(right.occurred_at));
  const items: LifecycleTimelineItem[] = [];
  let pendingSleep: DashboardLifecycleEvent | null = null;
  let pendingStop: DashboardLifecycleEvent | null = null;
  let previousBootTime: string | null = null;

  for (const event of ordered) {
    if (event.event_type === "system.sleep") {
      // macOS dark wakes can emit a second sleep notification without an intervening
      // user-visible wake. Merge consecutive sleeps into one period that starts at the
      // first sleep and ends at the next real wake.
      if (pendingSleep === null) pendingSleep = event;
      continue;
    }
    if (event.event_type === "system.wake") {
      if (pendingSleep === null) {
        items.push(singleItem(event, "Woke from sleep", "No matching sleep event"));
      } else {
        items.push({
          key: `${pendingSleep.event_id}:${event.event_id}`,
          title: "Sleep",
          detail: `${formatTimeRange(pendingSleep.occurred_at, event.occurred_at)} · ${formatDuration(pendingSleep.occurred_at, event.occurred_at)}`,
          occurredAt: event.occurred_at,
        });
        pendingSleep = null;
      }
      continue;
    }
    if (event.event_type === "agent.stopped") {
      if (pendingStop !== null) items.push(singleItem(pendingStop, "Agent stopped"));
      pendingStop = event;
      continue;
    }
    if (event.event_type === "agent.started") {
      const bootChanged = previousBootTime !== null
        && event.boot_time !== null
        && previousBootTime !== event.boot_time;
      if (pendingStop !== null) {
        items.push({
          key: `${pendingStop.event_id}:${event.event_id}`,
          title: bootChanged ? "Mac restarted" : "Agent restarted",
          detail: `${formatTimeRange(pendingStop.occurred_at, event.occurred_at)} · ${formatDuration(pendingStop.occurred_at, event.occurred_at)}`,
          occurredAt: event.occurred_at,
        });
        pendingStop = null;
      } else {
        items.push(singleItem(
          event,
          bootChanged ? "Mac started after shutdown or restart" : "Agent started",
          event.boot_time === null ? null : `macOS booted ${formatDateTime(event.boot_time)}`,
        ));
      }
      if (event.boot_time !== null) previousBootTime = event.boot_time;
      continue;
    }
    items.push(singleItem(event, "Agent recovered after an unexpected exit"));
  }

  if (pendingSleep !== null) items.push(singleItem(pendingSleep, "Entered sleep", "No matching wake event yet"));
  if (pendingStop !== null) items.push(singleItem(pendingStop, "Agent stopped", "No matching start event yet"));
  return items.sort((left, right) => Date.parse(right.occurredAt) - Date.parse(left.occurredAt));
}

function singleItem(event: DashboardLifecycleEvent, title: string, detail: string | null = null): LifecycleTimelineItem {
  return { key: event.event_id, title, detail, occurredAt: event.occurred_at };
}

function formatTimeRange(start: string, end: string): string {
  return `${formatDateTime(start)}–${formatDateTime(end)}`;
}

function formatDateTime(value: string): string {
  return new Date(value).toLocaleString();
}

function formatDuration(start: string, end: string): string {
  const seconds = Math.max(0, Math.round((Date.parse(end) - Date.parse(start)) / 1000));
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  if (hours > 0) return `${hours}h ${minutes}m`;
  if (minutes > 0) return `${minutes}m`;
  return `${seconds}s`;
}
