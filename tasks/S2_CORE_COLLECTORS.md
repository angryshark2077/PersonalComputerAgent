# S2 · Core Collectors

## Must read

Spec §3, §4, §13, §14, §21, §25.

## Objective

Implement Activity, System and Screenshot vertical slices through Event Store and Outbox.

## Deliverables

- Collector Registry and desired/applied config revision.
- permission/capability mapping.
- Activity Bridge source and Rust sessionizer.
- System health collector.
- Screenshot scheduling and ScreenCaptureKit Bridge.
- privacy exclusion rules.
- pause/resume.
- backpressure.
- attachment local staging.
- Collector health and error events.

## Invariants

- Swift produces platform observations; Rust creates domain events/sessions.
- Collector never calls cloud.
- Permission revoke stops collection within 5 seconds.
- Pause does not fabricate catch-up events.
- Screenshot respects excluded apps/displays and retention configuration.
- Disk <2GB disables screenshots/attachments first.

## Tests

- app switch stable/debounce
- idle/lock/sleep session cutoff
- multi-display capture group
- permission denied/revoked
- privacy pause
- 2h offline outbox durability
- crash between capture and attachment enqueue
- backpressure thresholds

## Exit gate

- Activity/System/Screenshot events persist and queue.
- 2h offline no loss/no duplicate.
- permission revoke stops within 5 seconds.
- idle CPU and memory remain inside budget.

## 2026-07-31 · System vertical slice complete

Completed only the first System slice: Collector Registry and durable state, host and
Agent CPU/memory, PCA data-volume disk metrics, atomic Event/Outbox/CollectorState
commit, deterministic scheduling/retry, and exact 10,000/8,000 backpressure hysteresis.

The overall S2 objective and exit gate remain open. Battery/Power and Network are
deferred to the next System increment. Activity, Screenshot, permission revocation,
privacy exclusions beyond this System payload, Attachment staging, and full S2
acceptance are not marked complete.
