# S2 System Collector Vertical Slice Design

**Date:** 2026-07-31

**Status:** Approved for implementation planning

**Scope:** The first S2 vertical slice: Collector Registry and durable state, host and Agent CPU/memory, PCA data-volume disk metrics, and transactional Event/Outbox persistence.

## 1. Objective

Add the smallest complete System Collector path to the installed Rust Agent:

```text
identity gate
  -> Collector Registry
  -> System sampling
  -> Agent Core Event factory
  -> DbActor transaction
  -> Event Store + Outbox + Collector state
```

The slice proves that a real Collector can run through the existing local runtime without introducing S1B pairing, Cloud Sync, Activity, Screenshot, Battery/Power, or Network work.

## 2. Approved Product Boundaries

- Production remains unpaired. The System Collector is registered but disabled and produces no System Event.
- Tests may inject a valid `workspace_id` and `device_id`. A valid injected identity is sufficient to enable the default-on System Collector with desired/applied revision `0/0`.
- The implementation uses a separate `crates/system-collector` Rust crate.
- `sysinfo` is pinned to `0.33.1`, with default features disabled and only the `system` and `disk` features enabled.
- Agent Core owns identity, Collector Registry, Event creation, EventSink, and DbActor access.
- The Collector never accesses SQLite or Cloud APIs and never owns workspace/device identity.
- CPU/memory and disk are separate samples because they run at different frequencies.
- Battery/Power and Network are deferred to the next System Collector slice.

## 3. Architecture and Ownership

### 3.1 `crates/system-collector`

The crate owns:

- the narrow `SystemMetricsSource` interface;
- the `sysinfo` production adapter;
- a deterministic Fake source for tests;
- CPU/memory and disk sampling;
- independent sampling timers;
- missed-tick behavior;
- retry and cancellation behavior.

The crate returns typed sample outcomes. It does not construct a complete `EventEnvelope`, own identity, access DbActor, or call Cloud APIs.

### 3.2 Agent Core

Agent Core owns:

- `Option<PairedIdentity>`;
- the `CollectorRegistry`;
- the canonical in-memory Collector runtime state;
- the asynchronous EventSink implementation;
- Event envelope creation;
- Collector state transitions;
- Outbox depth policy;
- startup, shutdown, and restart coordination.

The Registry is the sole in-memory fact source for Collector status. It tracks active degradation reasons so recovery from one reason cannot incorrectly move the Collector to `running` while another reason remains active.

### 3.3 Domain Contracts

`pca-domain` gains the minimum types required by the vertical slice:

- `CollectorDefinition`;
- durable Collector state DTOs;
- typed System sample DTOs;
- `EventCommit`, containing one through four Events and an optional resulting Collector state.

The existing synchronous EventSink placeholder becomes an object-safe asynchronous interface. Its completion means the DbActor transaction committed, not merely that an item entered an in-memory queue. The implementation uses a standard-library boxed future and does not add `async-trait`.

Collector commits always include the resulting Collector state. The optional state exists so the same EventSink remains usable by Provider and lifecycle producers that do not own Collector state.

This slice does not add dynamic Collector loading, a general plugin system, or speculative configuration abstractions.

### 3.4 DbActor

DbActor remains the only SQLite writer. It gains:

- Collector state loading;
- a state-only upsert for cases where no Event is required;
- active Outbox depth queries;
- an atomic Collector commit operation.

One Collector commit transaction writes:

1. one through four Events;
2. one stable Outbox row per Event;
3. the resulting `collector_states` row.

A status-change Event and its matching state row commit together. A metric Event and the updated `last_event_at_ms`/`last_health_at_ms` commit together.

## 4. Identity Gate and Startup

### 4.1 Unpaired production path

When no valid identity exists:

- Registry contains the `system` definition;
- durable status is `disabled`;
- revision is `0/0`;
- no sampling task starts;
- no System or Collector status Event is created;
- no dummy workspace/device identifier is invented for this Collector.

The disabled row preserves previously durable revision and timestamp fields if they exist.

### 4.2 Valid test identity path

When the process-test build injects valid UUIDs:

1. load the prior durable state;
2. preserve revisions and last-event/health timestamps;
3. recompute runtime status instead of restoring `running` or `degraded`;
4. transition from `disabled` to `initializing`;
5. immediately run one CPU/memory sample and one disk sample;
6. enter `running` only if both initial samples succeed and no backpressure condition is active;
7. otherwise enter `degraded`, `unsupported`, or `error` according to the failure class.

The identity injection exists only behind the existing debug-only `process-test-hooks` boundary. Release builds cannot accept it.

## 5. Sampling Model

### 5.1 CPU and memory

- Run immediately after initialization.
- Then run every 30 seconds.
- Refresh CPU counters twice, respecting `sysinfo`'s minimum refresh interval for the initial warm-up.
- Record the actual elapsed sampling window.
- Normalize host and Agent CPU to the same `0-100` whole-machine scale.
- Collect host total/used memory and Agent resident memory.
- Do not collect PID, command line, executable path, environment, or other process metadata.

### 5.2 Disk

- Run immediately after initialization.
- Then run every 5 minutes.
- Resolve only the mounted volume containing the PCA data directory.
- Collect total and available bytes and derive used percentage.
- Do not emit mount path, volume name, filesystem name, or any other mounted volume.
- Do not recursively calculate PCA directory usage in this slice. Screenshot/Attachment work will add scoped directory usage when it has a producer.

### 5.3 Timer behavior

Both timers use Tokio `MissedTickBehavior::Skip`. Sleep, scheduler delay, backpressure, or retry recovery never creates catch-up samples or a burst of historical Events.

## 6. Event Contracts

All Event payloads use strict dedicated JSON Schemas and reject unknown fields.

### 6.1 CPU/memory metric

Envelope:

- `event_type`: `system.metric_sampled`
- `source`: `system`
- `schema_version`: `1`
- `sensitivity`: `normal`
- empty `attachment_refs`

Payload:

```json
{
  "metric_group": "cpu_memory",
  "sample_window_ms": 30000,
  "logical_cpu_count": 10,
  "host": {
    "cpu_usage_percent": 12.34,
    "memory_total_bytes": 34359738368,
    "memory_used_bytes": 17179869184
  },
  "agent": {
    "cpu_usage_percent": 0.42,
    "memory_resident_bytes": 73400320
  }
}
```

Constraints:

- percentages are finite and in `0-100`;
- byte values and `sample_window_ms` are non-negative;
- `logical_cpu_count` is positive;
- host used memory cannot exceed total memory.

### 6.2 Disk metric

Envelope fields match the CPU/memory Event.

Payload:

```json
{
  "metric_group": "disk",
  "scope": "pca_data_volume",
  "total_bytes": 994662584320,
  "available_bytes": 536870912000,
  "used_percent": 46.02,
  "low_space": false,
  "low_space_threshold_bytes": 2147483648,
  "warning_code": null
}
```

Constraints:

- `scope` is exactly `pca_data_volume`;
- available bytes cannot exceed total bytes;
- used percentage is finite and in `0-100`;
- `low_space` is true when available bytes are below 2 GiB;
- `warning_code` is `DISK_SPACE_LOW` exactly when `low_space` is true, otherwise null.

Low disk space does not make System sampling fail. The metric remains available so the later Screenshot/Attachment policy can consume it.

### 6.3 Collector status change

Envelope:

- `event_type`: `collector.status_changed`
- `source`: `collector.registry`
- `schema_version`: `1`
- `sensitivity`: `normal`

Payload fields:

- `collector_key`;
- `previous_status`;
- `status`;
- `desired_config_revision`;
- `applied_config_revision`;
- `reason`;
- nullable `error_code`.

The Registry emits this Event only when status actually changes. Retry attempts that remain in the same state do not create duplicate status Events.

Backpressure uses:

- status: `degraded`;
- reason: `outbox_backpressure`;
- error code: `COLLECTOR_DEGRADED`.

Retryable sampling failures use `COLLECTOR_DEGRADED`. Unsupported platform behavior uses `COLLECTOR_UNSUPPORTED`. Non-retryable initialization failures use `COLLECTOR_INIT_FAILED`.

### 6.4 System health change

Disk warning transitions use:

- `event_type`: `system.health_changed`;
- `source`: `system`;
- `schema_version`: `1`;
- `sensitivity`: `normal`.

Payload:

```json
{
  "condition": "disk_space_low",
  "active": true,
  "error_code": "DISK_SPACE_LOW",
  "available_bytes": 1073741824,
  "threshold_bytes": 2147483648
}
```

The first healthy disk sample creates no health-change Event. Crossing below the threshold creates one active Event; recovering creates one inactive Event. Periodic low-space samples do not repeat the warning Event. After process restart, the first low-space sample reasserts the active warning once because this slice does not add a separate durable condition-state store.

### 6.5 Event identity and time

- Agent Core creates a valid UUID Event ID.
- A retry of the same logical commit reuses the Event ID and Outbox ID.
- `occurred_at` is the completed sample time.
- `created_at` is the time Agent Core creates the envelope.
- Metric Events do not use attachments.

## 7. Durable Collector State

The new immutable S2 migration creates `collector_states` with:

- `collector_key` as the primary key;
- `status`;
- `version`;
- `desired_revision`;
- `applied_revision`;
- nullable `last_event_at_ms`;
- nullable `last_health_at_ms`;
- nullable `last_error_code`;
- `created_at_ms`;
- `updated_at_ms`.

Semantics:

- `last_event_at_ms` advances only when a `system` metric or health Event commits; a Registry status Event alone does not advance it.
- `last_health_at_ms` advances when a sampling/health evaluation is durably recorded, including a transition to a failure state.
- `last_error_code` clears after complete recovery.
- restart preserves revisions and timestamps but always recomputes live status.
- state-only persistence is allowed when no Event is required: the identity-less disabled path and a repeated failed health check that does not change status.
- a state-only write cannot advance `last_event_at_ms` or represent a successful sample.

The schema change includes a data-dictionary update, index review, empty and upgrade migrations, replay tests, forward-only recovery rules, and Cloud/Sync impact notes. No Cloud schema changes are required in this slice.

## 8. Backpressure

Active Outbox depth counts every row whose state is not `acked`.

The Registry applies hysteresis:

- depth `> 10,000`: suppress new `system.metric_sampled` Events and enter `degraded/outbox_backpressure`;
- depth `< 8,000`: resume normal sampling and remove the backpressure degradation reason;
- depth from `8,000` through `10,000`: retain the current in-process backpressure state.

After a process restart, the Registry recomputes from the high-water threshold. A depth at or below 10,000 may resume sampling because hysteresis is intended to prevent live threshold flapping, not persist a latch across restarts.

Suppressed samples are discarded. They are never queued in memory, reconstructed, or emitted later.

The one transition Event that reports entering backpressure is allowed to add one Outbox item. If persistence is unavailable, the Core retains the same pending Event identity and retries it rather than creating repeated transition Events.

## 9. Failure, Timeout, and Recovery

### 9.1 Retryable sampling failure

- The first failure immediately adds a sampling degradation reason.
- Retry delays are 30, 60, 120, 240, and 300 seconds.
- Further retries remain capped at 300 seconds.
- Any successful retry removes that sampling degradation reason.
- Status returns to `running` only when no degradation reason remains.

### 9.2 EventSink timeout

One EventSink commit has a 5-second caller deadline. Cancellation before DbActor begins causes the actor to skip the canceled request. If the timeout races with a transaction already in progress, retry uses the same Event and Outbox identifiers. SQLite uniqueness and idempotent state upsert prevent duplicates.

### 9.3 Non-retryable failures

- Unsupported host capability: `unsupported`.
- Invalid Collector definition/config or non-retryable initialization failure: `error`.
- DbActor unavailability stops new metric production and is retried through the same bounded failure path; the Core must not report a metric as persisted until EventSink confirms the commit.

No queue, retry loop, or in-memory sample backlog is unbounded.

## 10. Testing

### 10.1 Contract tests

Add strict schemas and valid/invalid fixtures for:

- both `system.metric_sampled` payload variants;
- `collector.status_changed`;
- `system.health_changed`.

Reject:

- unknown metric groups and unknown fields;
- invalid UUIDs;
- invalid status/error combinations;
- negative byte counts;
- non-finite or out-of-range percentages;
- inconsistent disk and memory sizes.

Rust DTOs and shared contract fixtures must validate each other.

### 10.2 System Collector unit tests

Use the Fake source and paused Tokio time to verify:

- immediate first CPU/memory and disk samples;
- 30-second and 5-minute schedules;
- skipped missed ticks and no catch-up burst;
- CPU normalization;
- PCA data-volume selection without emitted paths;
- low-space transition and recovery behavior;
- first-failure degradation;
- exact retry sequence and cap;
- recovery to the normal cadence;
- cancellation and stop behavior.

### 10.3 Real `sysinfo` smoke test

On macOS, verify:

- the current Agent/test process can be found;
- CPU values are finite and within `0-100`;
- memory/disk sizes have valid relationships;
- the temporary PCA data directory maps to a volume;
- no unstable metric is asserted to equal a specific value.

### 10.4 Registry/Core tests

Verify:

- unpaired means disabled with no System Event or Outbox;
- valid injected UUIDs enable the default System Collector at revision `0/0`;
- startup passes through `initializing`;
- initial sample results determine `running` or `degraded`;
- restart preserves revision/timestamps but does not inherit live status;
- repeated identical state does not duplicate status Events;
- simultaneous degradation reasons do not recover prematurely;
- backpressure boundaries at 10,000, 10,001, 8,000, and 7,999;
- backpressure and timer recovery do not produce catch-up metrics.

### 10.5 DbActor and migration tests

Verify:

- empty migration;
- upgrade from the immutable S1A `0001` schema;
- replay without schema change;
- checksum mismatch and future-version rejection;
- Event, Outbox, and CollectorState all commit or all roll back;
- process kill inside the transaction yields either all three durable effects or none;
- a timed-out commit retried with the same IDs remains unique;
- active Outbox depth excludes only `acked`.

### 10.6 Two-hour virtual offline test

With no ACKs and no backpressure:

- CPU/memory produces 241 samples: one immediate plus 240 periodic;
- disk produces 25 samples: one immediate plus 24 periodic;
- every Event has exactly one Outbox row;
- all Event IDs are unique;
- no sample is lost, duplicated, or fabricated as catch-up.

Status and health-transition Events are asserted separately and are not included in the metric counts.

### 10.7 Process and regression tests

- Run real agentd against a temporary runtime root and SQLite database with debug-only injected UUIDs.
- Verify real System Event, Outbox, and Collector state rows.
- Verify the production unpaired path remains disabled.
- Confirm release builds do not contain the identity-injection entrypoint.
- Re-run existing S1A install, restart, transaction, packaging, and security-boundary tests.
- Under active System collection, verify average Agent CPU below 1% and memory below 120 MB.

## 11. Dependency Decision

Use:

```toml
sysinfo = {
  version = "=0.33.1",
  default-features = false,
  features = ["system", "disk"]
}
```

Do not enable `component`, `network`, `user`, `multithread`, or `serde`. `sysinfo 0.33.1` declares Rust 1.74 as its minimum, so it is compatible with the workspace's fixed Rust 1.82 toolchain. Its license is MIT.

## 12. Documentation and Expected Change Surface

The expected implementation change surface is limited to:

- root Cargo workspace metadata and `Cargo.lock`;
- `crates/domain`;
- new `crates/system-collector`;
- `crates/db-local` and a new immutable migration;
- `crates/provider-contracts` for the asynchronous EventSink signature;
- `agent/core`;
- root contracts and their package mirror;
- contract, unit, integration, process, and packaging tests;
- the local data dictionary, architecture/performance documentation, and S2 task notes.

Every changed line must support this vertical slice. The S2 task remains open after this work because Activity, Screenshot, permissions, attachments, and the remainder of the S2 exit gate are not complete.

## 13. Explicit Exclusions

- Battery/Power and Network;
- Activity and Screenshot;
- Attachment staging or upload;
- S1B pairing and production credential identity loading;
- Cloud Sync, ACK, retry transport, or Outbox cleanup;
- Cloud projections and Dashboard work;
- Cloud desired-config retrieval;
- user manual pause/resume;
- generic plugin or dynamic Collector loading;
- `.env`, secret, account, install-path, or S1A release-channel changes.

## 14. Acceptance Criteria

The slice is complete only when:

1. a production unpaired Agent keeps System disabled across restart;
2. debug-only valid identity injection starts System with revision `0/0`;
3. host and Agent CPU/memory Events and PCA data-volume disk Events persist through the standard Event/Outbox path;
4. Collector state, Events, and Outbox rows meet the approved transaction boundary;
5. two virtual offline hours lose, duplicate, and backfill zero metric samples;
6. backpressure, retry, timeout, restart, and migration boundaries have deterministic tests;
7. active System collection remains within the existing CPU and memory budgets;
8. S1A installation, restart, transaction durability, packaging, and security boundaries do not regress;
9. no deferred S2 capability is presented as complete.
