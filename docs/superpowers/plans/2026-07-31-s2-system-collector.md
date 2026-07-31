# S2 System Collector Vertical Slice Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add the first real System Collector path so a debug-only valid identity can persist host and Agent CPU/memory plus PCA data-volume disk metrics through Collector Registry, Event Store, Outbox, and durable Collector state, while the production unpaired Agent remains disabled.

**Architecture:** Agent Core owns identity, Registry, Event creation, the asynchronous EventSink, and DbActor. The independent `pca-system-collector` crate owns a bounded blocking sampler actor plus async schedules and returns typed observations; DbActor atomically writes one through four Events, their Outbox rows, and the resulting Collector state.

**Tech Stack:** Rust 1.82, Tokio, Serde/serde_json, rusqlite/SQLite WAL, UUID, `sysinfo = 0.33.1`, JSON Schema Draft 2020-12, TypeScript/Ajv, Python and shell verification gates.

## Global Constraints

- Production has no valid paired identity in this slice; `system` must remain durable `disabled` and emit no System Event or Outbox row.
- Debug-only tests may inject a non-nil valid UUID `workspace_id` and `device_id`; that identity enables System at desired/applied revision `0/0`.
- Pin `sysinfo` exactly to `=0.33.1` with `default-features = false` and features `["system", "disk"]`.
- Do not enable `component`, `network`, `user`, `multithread`, or `serde`.
- Rust minimum stays exactly `1.82`; do not add `async-trait`, unsafe code, unbounded channels, unbounded retries, or unbounded in-memory sample queues.
- CPU/memory runs immediately and every 30 seconds; disk runs immediately and every 5 minutes.
- Both host and Agent CPU are finite whole-machine percentages in `0-100`.
- Only the volume containing PCA `Data/` is sampled; never emit mount path, volume name, filesystem name, PID, command line, executable path, or environment.
- EventSink completion means the SQLite transaction committed; its caller deadline is 5 seconds and retries reuse the same Event IDs.
- One Collector commit contains one through four Events, one Outbox row per Event, and an optional resulting Collector state.
- Outbox depth counts every non-`acked` row. Suppress metrics above 10,000, resume below 8,000, retain the current live latch from 8,000 through 10,000, and recompute from the high threshold after restart.
- Sampling retry delays are exactly 30, 60, 120, 240, and then 300 seconds capped.
- Missed ticks are skipped. Do not cache, reconstruct, or emit catch-up samples.
- Keep Battery/Power, Network, Activity, Screenshot, Attachment, S1B pairing, Cloud Sync/ACK, Cloud projections, Dashboard, desired-config pull, and user pause/resume out of scope.
- Do not change `.env`, secrets, account settings, installation paths, S1A release channels, or immutable migrations `0000` and `0001`.
- The full S2 task remains open after this slice.

---

## File Structure

Create focused files rather than extending `agent/core/src/app.rs` with Collector internals:

```text
crates/system-collector/
├── Cargo.toml                 # Exact dependency/features and crate lint policy
├── src/lib.rs                 # Public re-exports only
├── src/source.rs              # SystemMetricsSource and sysinfo adapter
├── src/sampler_actor.rs       # Bounded blocking owner thread
├── src/runtime.rs             # Async timers, suppression, retry, cancellation
├── tests/runtime.rs           # Paused-time deterministic schedule tests
└── tests/sysinfo_smoke.rs     # Real macOS range/privacy smoke test

agent/core/src/
├── collector_registry.rs      # Pure System status/degradation state machine
├── event_factory.rs           # Strict EventEnvelope construction
├── event_sink.rs              # Five-second DbActor EventSink adapter
├── system_runtime.rs          # Registry + collector + persistence orchestration
└── app.rs                     # Startup/cleanup wiring only
```

Keep the existing `crates/domain/src/lib.rs`, `crates/db-local/src/actor.rs`, and `crates/db-local/src/repository.rs` layouts because those crates currently use single focused modules and this slice does not justify unrelated restructuring.

---

### Task 1: Add Strict System and Collector Event Payload Contracts

**Files:**
- Create: `contracts/system-metric-sampled.schema.json`
- Create: `contracts/collector-status-changed.schema.json`
- Create: `contracts/system-health-changed.schema.json`
- Create: `packages/contracts/system-metric-sampled.schema.json`
- Create: `packages/contracts/collector-status-changed.schema.json`
- Create: `packages/contracts/system-health-changed.schema.json`
- Create: `packages/contracts/fixtures/system-metric.cpu-memory.valid.json`
- Create: `packages/contracts/fixtures/system-metric.disk.valid.json`
- Create: `packages/contracts/fixtures/system-metric.invalid-percent.json`
- Create: `packages/contracts/fixtures/system-metric.invalid-unknown-field.json`
- Create: `packages/contracts/fixtures/collector-status.valid.json`
- Create: `packages/contracts/fixtures/system-health.active.json`
- Modify: `packages/contracts/src/validate.ts:7-42`
- Modify: `packages/contracts/src/types.ts:65-86`
- Modify: `packages/contracts/tests/contracts.test.ts:32-82`
- Modify: `contracts/README.md`

**Interfaces:**
- Consumes: existing `validateContract(schemaName, value)` and canonical collector/error enums.
- Produces: `SystemMetricPayload`, `CpuMemoryMetricPayload`, `DiskMetricPayload`, `CollectorStatusChangedPayload`, `SystemHealthChangedPayload`, and three registered schema names used by Rust fixtures and Agent Core Event tests.

- [ ] **Step 1: Add failing contract tests and fixtures**

Add tests that name the exact new contracts before registering them:

```ts
test("System metric payloads are strict discriminated unions", () => {
  for (const name of [
    "system-metric.cpu-memory.valid.json",
    "system-metric.disk.valid.json",
  ]) {
    assert.deepEqual(
      validateContract("system-metric-sampled", fixture(name)),
      { valid: true, errors: [] },
    );
  }
  assert.equal(
    validateContract(
      "system-metric-sampled",
      fixture("system-metric.invalid-percent.json"),
    ).valid,
    false,
  );
  assert.equal(
    validateContract(
      "system-metric-sampled",
      fixture("system-metric.invalid-unknown-field.json"),
    ).valid,
    false,
  );
});

test("Collector status and System health payloads validate", () => {
  assert.equal(
    validateContract("collector-status-changed", fixture("collector-status.valid.json"))
      .valid,
    true,
  );
  assert.equal(
    validateContract("system-health-changed", fixture("system-health.active.json"))
      .valid,
    true,
  );
});
```

The valid CPU fixture must use `metric_group: "cpu_memory"`, finite percentages in `0-100`, positive logical CPU count, and `memory_used_bytes <= memory_total_bytes`. The disk fixture must use `scope: "pca_data_volume"` and consistent `low_space`, threshold, and nullable warning code fields.

- [ ] **Step 2: Run the contract tests and verify they fail**

Run:

```bash
pnpm --filter @pca/contracts test
```

Expected: TypeScript compilation fails because the three schema names are not members of `ContractSchemaName`.

- [ ] **Step 3: Add the three schemas and TypeScript DTOs**

Implement `system-metric-sampled.schema.json` with `oneOf` branches whose `metric_group` values are constants. Both branches use `additionalProperties: false`. Use JSON Schema conditionals so disk consistency is enforceable:

```json
{
  "allOf": [
    {
      "if": { "properties": { "low_space": { "const": true } } },
      "then": { "properties": { "warning_code": { "const": "DISK_SPACE_LOW" } } },
      "else": { "properties": { "warning_code": { "type": "null" } } }
    }
  ]
}
```

Add exact TypeScript shapes:

```ts
export interface CpuMemoryMetricPayload {
  metric_group: "cpu_memory";
  sample_window_ms: number;
  logical_cpu_count: number;
  host: {
    cpu_usage_percent: number;
    memory_total_bytes: number;
    memory_used_bytes: number;
  };
  agent: {
    cpu_usage_percent: number;
    memory_resident_bytes: number;
  };
}

export interface DiskMetricPayload {
  metric_group: "disk";
  scope: "pca_data_volume";
  total_bytes: number;
  available_bytes: number;
  used_percent: number;
  low_space: boolean;
  low_space_threshold_bytes: 2147483648;
  warning_code: "DISK_SPACE_LOW" | null;
}

export type SystemMetricPayload =
  | CpuMemoryMetricPayload
  | DiskMetricPayload;

export type CollectorStatus =
  | "disabled"
  | "permission_required"
  | "initializing"
  | "running"
  | "paused"
  | "degraded"
  | "unsupported"
  | "error";

export interface CollectorStatusChangedPayload {
  collector_key: string;
  previous_status: CollectorStatus;
  status: CollectorStatus;
  desired_config_revision: number;
  applied_config_revision: number;
  reason: string;
  error_code: string | null;
}

export interface SystemHealthChangedPayload {
  condition: "disk_space_low";
  active: boolean;
  error_code: "DISK_SPACE_LOW";
  available_bytes: number;
  threshold_bytes: 2147483648;
}
```

Mirror every schema byte-for-byte under both `contracts/` roots and register all names in `validate.ts`. JSON Schema enforces shape, enums, nullability, and individual numeric ranges. Add `validateSystemMetricRelationships` in `validate.ts` after Ajv succeeds to reject:

- host used memory above total memory;
- disk available bytes above total bytes;
- `low_space` that disagrees with `available_bytes < low_space_threshold_bytes`;
- `warning_code` that disagrees with `low_space`;
- `used_percent` that differs from the derived percentage by more than 0.01.

Exercise the relationship validator by cloning the valid fixtures in tests and mutating one related field at a time.

Restrict Collector status reasons to:

```text
identity_available
initial_samples_succeeded
sampling_failed
sampling_recovered
outbox_backpressure
outbox_recovered
persistence_failed
persistence_recovered
collector_unsupported
collector_error
```

Add `validateCollectorStatusRelationships` so `degraded`, `unsupported`, and `error` require `COLLECTOR_DEGRADED`, `COLLECTOR_UNSUPPORTED`, and `COLLECTOR_INIT_FAILED` respectively, while `disabled`, `permission_required`, `initializing`, `running`, and `paused` require a null error code.

- [ ] **Step 4: Run contract, type, and mirror checks**

Run:

```bash
pnpm --filter @pca/contracts test
pnpm --filter @pca/contracts typecheck
cmp contracts/system-metric-sampled.schema.json packages/contracts/system-metric-sampled.schema.json
cmp contracts/collector-status-changed.schema.json packages/contracts/collector-status-changed.schema.json
cmp contracts/system-health-changed.schema.json packages/contracts/system-health-changed.schema.json
python3 scripts/verify_contracts.py
```

Expected: all commands exit 0 and `verify_contracts.py` reports 12 schemas.

- [ ] **Step 5: Commit the contracts**

```bash
git add contracts packages/contracts
git commit -m "feat: define S2 system event contracts"
```

---

### Task 2: Upgrade the Rust Domain Contract and EventSink

**Files:**
- Modify: `crates/domain/src/lib.rs:21-195`
- Create: `crates/domain/tests/system_contracts.rs`
- Verify compilation: `crates/provider-contracts/src/lib.rs`

**Interfaces:**
- Consumes: canonical Event envelope, Collector status enum, and payload field names from Task 1.
- Produces:

```rust
pub const MAX_EVENTS_PER_COMMIT: usize = 4;
pub struct CollectorDefinition {
    pub key: &'static str,
    pub version: &'static str,
    pub supported_event_types: &'static [&'static str],
}
pub struct CollectorState {
    pub collector_key: String,
    pub collector_version: String,
    pub status: CollectorStatus,
    pub desired_config_revision: u64,
    pub applied_config_revision: u64,
    pub last_event_at_ms: Option<i64>,
    pub last_health_at_ms: Option<i64>,
    pub last_error_code: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}
pub struct HostCpuMemory {
    pub cpu_usage_percent: f64,
    pub memory_total_bytes: u64,
    pub memory_used_bytes: u64,
}
pub struct AgentCpuMemory {
    pub cpu_usage_percent: f64,
    pub memory_resident_bytes: u64,
}
pub struct CpuMemorySample {
    pub sample_window_ms: u64,
    pub logical_cpu_count: u32,
    pub host: HostCpuMemory,
    pub agent: AgentCpuMemory,
}
pub enum DiskScope { PcaDataVolume }
pub struct DiskSample {
    pub scope: DiskScope,
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub used_percent: f64,
    pub low_space: bool,
    pub low_space_threshold_bytes: u64,
    pub warning_code: Option<String>,
}
#[serde(tag = "metric_group", rename_all = "snake_case")]
pub enum SystemMetricSample { CpuMemory(CpuMemorySample), Disk(DiskSample) }
#[derive(Clone)]
pub struct EventCommit {
    events: Vec<EventEnvelope>,
    collector_state: Option<CollectorState>,
}
impl EventCommit {
    pub fn events(&self) -> &[EventEnvelope];
    pub fn collector_state(&self) -> Option<&CollectorState>;
}
pub trait EventSink {
    fn commit<'a>(&'a self, commit: EventCommit) -> EventSinkFuture<'a>;
}
```

- [ ] **Step 1: Write failing Rust contract tests**

Create tests that prove the commit bound and serialization names:

```rust
#[test]
fn event_commit_requires_one_through_four_events() {
    assert!(EventCommit::try_new(Vec::new(), None).is_err());
    assert!(EventCommit::try_new(vec![event("1")], None).is_ok());
    assert!(EventCommit::try_new(
        (0..=MAX_EVENTS_PER_COMMIT).map(|index| event(&index.to_string())).collect(),
        None,
    )
    .is_err());
}

#[test]
fn collector_state_uses_canonical_snake_case_status() {
    let json = serde_json::to_value(state(CollectorStatus::Degraded))
        .expect("serialize collector state");
    assert_eq!(json["status"], "degraded");
    assert_eq!(json["desired_config_revision"], 0);
    assert_eq!(json["applied_config_revision"], 0);
}
```

Also deserialize the valid Task 1 payload fixtures into `SystemMetricSample` and reject the invalid percentage fixture through checked constructors.

- [ ] **Step 2: Run the new domain test and verify it fails**

Run:

```bash
cargo test -p pca-domain --test system_contracts
```

Expected: compile failure because `EventCommit`, `CollectorState`, and System sample DTOs do not exist.

- [ ] **Step 3: Add minimal domain types and checked constructors**

Add:

```rust
pub type EventSinkFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), DomainError>> + Send + 'a>>;

pub trait EventSink: Send + Sync {
    fn commit<'a>(&'a self, commit: EventCommit) -> EventSinkFuture<'a>;
}

impl EventCommit {
    pub fn try_new(
        events: Vec<EventEnvelope>,
        collector_state: Option<CollectorState>,
    ) -> Result<Self, DomainError> {
        if !(1..=MAX_EVENTS_PER_COMMIT).contains(&events.len()) {
            return Err(DomainError::new(
                "COLLECTOR_DEGRADED",
                "event commit must contain one through four events",
                false,
            ));
        }
        Ok(Self { events, collector_state })
    }

    pub fn events(&self) -> &[EventEnvelope] {
        &self.events
    }

    pub fn collector_state(&self) -> Option<&CollectorState> {
        self.collector_state.as_ref()
    }
}
```

Derive `Clone`, `Debug`, `PartialEq`, `Serialize`, and `Deserialize` where applicable, including `CollectorStatus`, Collector state, and all EventCommit members. Use private `RawCpuMemorySample` and `RawDiskSample` Serde structs plus `#[serde(try_from = "RawCpuMemorySample")]` and `#[serde(try_from = "RawDiskSample")]` on the checked DTOs so deserialization cannot bypass validation. Use `u64` for bytes/revisions, `i64` for SQLite millisecond timestamps, `f64` for percentages, and checked constructors that reject non-finite/out-of-range values and inconsistent totals.

- [ ] **Step 4: Run domain and dependent-crate tests**

Run:

```bash
cargo test -p pca-domain
cargo test -p pca-provider-contracts
cargo check --workspace
```

Expected: all exit 0; no dependent crate relies on the removed synchronous `emit` signature.

- [ ] **Step 5: Commit the domain boundary**

```bash
git add crates/domain crates/provider-contracts
git commit -m "refactor: make event persistence asynchronous"
```

---

### Task 3: Add the Immutable S2 Collector-State Migration

**Files:**
- Create: `crates/db-local/migrations/0002_s2_collector_state.sql`
- Modify: `crates/db-local/src/lib.rs:13-31`
- Modify: `crates/db-local/src/migrations.rs:5-31`
- Modify: `crates/db-local/src/repository.rs:157-187`
- Modify: `crates/db-local/src/actor.rs:54-305`
- Modify: `crates/db-local/tests/runtime_store.rs:52-150`
- Modify: `scripts/verify_migrations.py:12-22`
- Modify: `scripts/tests/test_engineering_gates.py:53-67`
- Modify: `packages/contracts/fixtures/runtime-status.local-healthy.json`
- Modify: `crates/test-contracts/tests/fixtures.rs`
- Modify: `scripts/verify-s1a-live.sh:416-425`
- Modify: `scripts/tests/test_s1a_live_verification.py`
- Modify: `agent/core/tests/process_lifecycle.rs`

**Interfaces:**
- Consumes: `pca_domain::CollectorState`.
- Produces:

```rust
DbActorHandle::load_collector_states(&self) -> Result<Vec<CollectorState>, DbError>
DbActorHandle::upsert_collector_state(&self, state: &CollectorState) -> Result<(), DbError>
pub const S2_COLLECTOR_STATE_MIGRATION: &str
```

- [ ] **Step 1: Write failing migration and repository tests**

Extend `runtime_store.rs` to expect schema version 2 and the exact table:

```rust
#[tokio::test]
async fn collector_state_survives_reopen_but_runtime_status_is_data_not_policy() {
    let (_directory, path) = database_path();
    let db = DbActorHandle::open(&path, "0.2.0").await.expect("open database");
    let expected = collector_state(CollectorStatus::Running);
    db.upsert_collector_state(&expected).await.expect("persist state");
    db.shutdown().await.expect("close database");

    let db = DbActorHandle::open(&path, "0.2.1").await.expect("reopen database");
    assert_eq!(
        db.load_collector_states().await.expect("load state"),
        vec![expected]
    );
}
```

Add a migration upgrade test that applies only `0000` and `0001`, opens through DbActor, and asserts `0002` is added without changing existing Event/Outbox rows.

- [ ] **Step 2: Run migration tests and verify they fail**

Run:

```bash
cargo test -p pca-db-local --test runtime_store collector_state
python3 scripts/verify_migrations.py .
```

Expected: Rust compile failure for missing APIs and Python failure because the expected chain does not yet include `0002_s2_collector_state.sql`.

- [ ] **Step 3: Add migration and state-only DbActor requests**

Create the table with exact constraints:

```sql
CREATE TABLE IF NOT EXISTS collector_states (
    collector_key TEXT PRIMARY KEY NOT NULL,
    status TEXT NOT NULL CHECK (
        status IN (
            'disabled', 'permission_required', 'initializing', 'running',
            'paused', 'degraded', 'unsupported', 'error'
        )
    ),
    version TEXT NOT NULL,
    desired_revision INTEGER NOT NULL DEFAULT 0 CHECK (desired_revision >= 0),
    applied_revision INTEGER NOT NULL DEFAULT 0 CHECK (applied_revision >= 0),
    last_event_at_ms INTEGER,
    last_health_at_ms INTEGER,
    last_error_code TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);
```

Do not add an index beyond the primary key: the slice reads the tiny definition-keyed table as a whole. Add `LoadCollectorStates` and `UpsertCollectorState` requests to the existing bounded DbActor queue and add the table to startup smoke queries.

- [ ] **Step 4: Update migration registry and replay gates**

Set `MAX_SUPPORTED_SCHEMA_VERSION` to 2, append immutable migration ID `0002`, export the migration constant, and update both Python expected-file lists. Update current-runtime status fixtures and S1A live verification from database schema version 1 to 2. Do not change Event payload `schema_version: 1`, and do not edit the contents of migrations `0000` or `0001`.

- [ ] **Step 5: Run migration, replay, and state tests**

Run:

```bash
cargo test -p pca-db-local --test runtime_store
cargo test -p pca-test-contracts
python3 scripts/verify_migrations.py .
python3 -m unittest scripts.tests.test_engineering_gates -v
python3 -m unittest scripts.tests.test_s1a_live_verification -v
```

Expected: all exit 0; empty, upgrade, replay, checksum, and future-schema tests report schema version 2.

- [ ] **Step 6: Commit the migration and state repository**

```bash
git add \
  crates/db-local \
  packages/contracts/fixtures/runtime-status.local-healthy.json \
  crates/test-contracts/tests/fixtures.rs \
  scripts/verify_migrations.py \
  scripts/verify-s1a-live.sh \
  scripts/tests/test_engineering_gates.py \
  scripts/tests/test_s1a_live_verification.py \
  agent/core/tests/process_lifecycle.rs
git commit -m "feat: persist S2 collector state"
```

---

### Task 4: Make Event, Outbox, and Collector State One Transaction

**Files:**
- Modify: `crates/db-local/src/actor.rs:54-305`
- Modify: `crates/db-local/src/repository.rs:7-156`
- Modify: `crates/db-local/tests/runtime_store.rs`

**Interfaces:**
- Consumes: `pca_domain::EventCommit` and the state APIs from Task 3.
- Produces:

```rust
DbActorHandle::commit_events(&self, commit: &EventCommit) -> Result<(), DbError>
DbActorHandle::active_outbox_depth(&self) -> Result<u64, DbError>
```

The existing `append_event_with_outbox` remains for S1A lifecycle callers and delegates to the same insertion helpers without a Collector state.

- [ ] **Step 1: Write failing atomicity, idempotency, and depth tests**

Add tests for:

```rust
#[tokio::test]
async fn collector_commit_is_atomic_for_events_outbox_and_state() {
    let db = open_database().await;
    let commit = commit_with_metric_and_running_state("metric-1");
    db.commit_events(&commit).await.expect("commit collector batch");
    assert_eq!(db.count_event_and_outbox("metric-1").await.unwrap(), (1, 1));
    assert_eq!(db.load_collector_states().await.unwrap()[0].status, CollectorStatus::Running);
}

#[tokio::test]
async fn active_depth_excludes_only_acked_rows() {
    seed_outbox_states(&path, &["pending", "sending", "acked", "conflict", "dead_letter"]);
    assert_eq!(db.active_outbox_depth().await.unwrap(), 4);
}
```

Install a real SQLite trigger that rejects the Collector-state upsert after Event/Outbox insertion and assert the Event, Outbox, and prior state all roll back. Retry the identical `EventCommit` twice and assert one Event, one Outbox, and one final state.

- [ ] **Step 2: Run targeted DbActor tests and verify they fail**

Run:

```bash
cargo test -p pca-db-local --test runtime_store collector_commit
cargo test -p pca-db-local --test runtime_store active_depth
```

Expected: compile failure because `commit_events` and `active_outbox_depth` do not exist.

- [ ] **Step 3: Implement one short transaction**

In `repository.rs`, serialize every payload before opening the transaction. Then:

```rust
let transaction = connection.transaction()?;
for event in commit.events() {
    insert_event(&transaction, event)?;
    insert_stable_outbox(&transaction, event)?;
}
if let Some(state) = commit.collector_state() {
    upsert_collector_state_in(&transaction, state)?;
}
transaction.commit()?;
```

Use `event:{event_id}` as the stable Outbox ID. `EventCommit` keeps its fields private and exposes read-only accessors, so the repository consumes only commits that passed the one-through-four constructor bound.

Implement depth with:

```sql
SELECT COUNT(*) FROM sync_outbox WHERE state <> 'acked'
```

- [ ] **Step 4: Run all database tests**

Run:

```bash
cargo test -p pca-db-local
```

Expected: all tests pass, including pre-existing S1A idempotency, lock timeout, cancellation, and shutdown tests.

- [ ] **Step 5: Commit the transactional sink**

```bash
git add crates/db-local
git commit -m "feat: commit collector events and state atomically"
```

---

### Task 5: Build the Bounded `sysinfo` Sampling Adapter

**Files:**
- Modify: `Cargo.toml:1-24`
- Modify: `Cargo.lock`
- Create: `crates/system-collector/Cargo.toml`
- Create: `crates/system-collector/src/lib.rs`
- Create: `crates/system-collector/src/source.rs`
- Create: `crates/system-collector/src/sampler_actor.rs`
- Create: `crates/system-collector/tests/sysinfo_smoke.rs`
- Modify: `scripts/verify_boundaries.py:50-78`
- Modify: `scripts/tests/test_engineering_gates.py`

**Interfaces:**
- Consumes: `CpuMemorySample`, `DiskSample`, and `SystemMetricSample` from Task 2.
- Produces:

```rust
pub enum MetricGroup { CpuMemory, Disk }
pub enum SystemSampleErrorKind { Retryable, Unsupported, Fatal }
pub struct SystemSampleError {
    pub kind: SystemSampleErrorKind,
    pub code: &'static str,
    pub message: String,
}
pub trait SystemMetricsSource: Send + 'static {
    fn sample_cpu_memory(&mut self) -> Result<CpuMemorySample, SystemSampleError>;
    fn sample_disk(&mut self) -> Result<DiskSample, SystemSampleError>;
}
pub struct SamplerHandle {
    requests: Option<mpsc::Sender<SampleRequest>>,
    owner_stopped: Option<oneshot::Receiver<()>>,
    owner_thread: Option<thread::JoinHandle<()>>,
}
impl SamplerHandle {
    pub async fn sample(&self, group: MetricGroup) -> Result<SystemMetricSample, SystemSampleError>;
    pub async fn shutdown(self) -> Result<(), SystemSampleError>;
}
pub fn start_sampler<S: SystemMetricsSource>(source: S) -> SamplerHandle;
pub struct SysinfoMetricsSource;
impl SysinfoMetricsSource {
    pub fn new(data_dir: PathBuf) -> Result<Self, SystemSampleError>;
}
```

- [ ] **Step 1: Add failing source and privacy tests**

Create tests around a fake mount list helper in `source.rs`:

```rust
#[test]
fn deepest_mount_prefix_selects_only_the_pca_data_volume() {
    let mounts = vec![
        disk("/", 1_000, 500),
        disk("/System/Volumes/Data", 900, 450),
        disk("/Volumes/External", 2_000, 1_500),
    ];
    let selected = select_data_volume(Path::new("/System/Volumes/Data/Users/a/PCA/Data"), &mounts)
        .expect("select data volume");
    assert_eq!(selected.total_bytes, 900);
    assert_eq!(selected.available_bytes, 450);
}
```

Add a Cargo manifest assertion test or engineering-gate assertion that the dependency is exactly:

```toml
sysinfo = { version = "=0.33.1", default-features = false, features = ["system", "disk"] }
```

Add an engineering-gate test that creates a synthetic `crates/system-collector` depending on `../db-local` and asserts `verify_boundaries.py` reports `forbidden dependency: crates/system-collector -> db-local`. Extend the Collector boundary to reject `cloud-client`, `db-local`, and `agent/core`; the production System crate may depend only on domain and platform-neutral libraries.

- [ ] **Step 2: Run the new crate tests and verify setup fails**

Run:

```bash
cargo test -p pca-system-collector
```

Expected: Cargo reports that package `pca-system-collector` does not exist.

- [ ] **Step 3: Register the crate and implement the source**

Add the crate to workspace members. Use:

```rust
use sysinfo::{
    CpuRefreshKind, Disks, MemoryRefreshKind, Pid, ProcessRefreshKind,
    ProcessesToUpdate, RefreshKind, System, MINIMUM_CPU_UPDATE_INTERVAL,
};
```

`SysinfoMetricsSource::new` performs the first CPU/process refresh and records `Instant`. `sample_cpu_memory` waits only the remaining `MINIMUM_CPU_UPDATE_INTERVAL` on its dedicated blocking thread, refreshes global CPU, memory, and only the current PID, and returns:

```rust
let logical = system.cpus().len();
let host_cpu = f64::from(system.global_cpu_usage()).clamp(0.0, 100.0);
let agent_cpu =
    (f64::from(process.cpu_usage()) / logical as f64).clamp(0.0, 100.0);
```

Reject zero logical CPUs, a missing current process, non-finite values, missing data-volume mount, and inconsistent byte totals with typed `SystemSampleError` values. Refresh only required System/process fields. Do not call `System::new_all`.

- [ ] **Step 4: Isolate blocking sampling behind a bounded actor**

Use a Tokio MPSC request queue with capacity 4 and `blocking_recv` on one named owner thread:

```rust
enum SampleRequest {
    Sample {
        group: MetricGroup,
        response: oneshot::Sender<Result<SystemMetricSample, SystemSampleError>>,
    },
}
```

The async handle awaits the bounded send and oneshot. Dropped/canceled responses are skipped before sampling when possible. `shutdown` closes the queue and joins without blocking an async executor worker.

- [ ] **Step 5: Add the real macOS smoke test**

The test creates a temporary `Data/` directory, samples twice through the real actor, and asserts:

```rust
assert!(cpu.host.cpu_usage_percent.is_finite());
assert!((0.0..=100.0).contains(&cpu.host.cpu_usage_percent));
assert!((0.0..=100.0).contains(&cpu.agent.cpu_usage_percent));
assert!(cpu.host.memory_used_bytes <= cpu.host.memory_total_bytes);
assert!(disk.available_bytes <= disk.total_bytes);
```

Serialize both payloads and assert the JSON contains none of the temporary path, current PID string, `mount_point`, `filesystem`, `command`, or `environment`.

- [ ] **Step 6: Run source, smoke, feature, and boundary checks**

Run:

```bash
cargo test -p pca-system-collector --test sysinfo_smoke -- --test-threads=1
cargo test -p pca-system-collector
cargo tree -p pca-system-collector -e features
python3 scripts/verify_boundaries.py .
python3 -m unittest scripts.tests.test_engineering_gates -v
```

Expected: tests pass; the feature tree shows `sysinfo/system` and `sysinfo/disk`, not its default feature set or Rayon.

- [ ] **Step 7: Commit the sampler**

```bash
git add \
  Cargo.toml \
  Cargo.lock \
  crates/system-collector \
  scripts/verify_boundaries.py \
  scripts/tests/test_engineering_gates.py
git commit -m "feat: sample system metrics with sysinfo"
```

---

### Task 6: Add Independent Schedules, Suppression, and Retry

**Files:**
- Create: `crates/system-collector/src/runtime.rs`
- Modify: `crates/system-collector/src/lib.rs`
- Create: `crates/system-collector/tests/runtime.rs`
- Modify: `crates/system-collector/Cargo.toml`

**Interfaces:**
- Consumes: `SamplerHandle` and `MetricGroup` from Task 5.
- Produces:

```rust
pub enum SystemObservation {
    Sampled { sample: SystemMetricSample, observed_at_ms: i64 },
    Failed { group: MetricGroup, error: SystemSampleError, observed_at_ms: i64 },
}
pub struct SystemCollectorHandle;
impl SystemCollectorHandle {
    pub fn set_suppressed(&self, suppressed: bool);
    pub async fn shutdown(self) -> Result<(), SystemSampleError>;
}
pub fn start_system_collector(
    sampler: SamplerHandle,
    observation_capacity: usize,
) -> (SystemCollectorHandle, mpsc::Receiver<SystemObservation>);
```

- [ ] **Step 1: Write paused-time failing schedule tests**

Use `#[tokio::test(start_paused = true)]` and a deterministic fake source:

```rust
#[tokio::test(start_paused = true)]
async fn emits_immediately_then_on_independent_periods() {
    let (handle, mut observations) = test_runtime(always_succeeds());
    assert_groups(&mut observations, &[MetricGroup::CpuMemory, MetricGroup::Disk]).await;

    tokio::time::advance(Duration::from_secs(30)).await;
    assert_eq!(next_group(&mut observations).await, MetricGroup::CpuMemory);

    tokio::time::advance(Duration::from_secs(270)).await;
    assert_groups(&mut observations, &[MetricGroup::CpuMemory, MetricGroup::Disk]).await;
    handle.shutdown().await.unwrap();
}
```

Add tests for the exact retry delays, 300-second cap, independent group recovery, suppression, immediate fresh sampling after suppression clears, skipped missed ticks, bounded receiver shutdown, and no post-shutdown observations.

- [ ] **Step 2: Run runtime tests and verify they fail**

Run:

```bash
cargo test -p pca-system-collector --test runtime
```

Expected: compile failure because `runtime` and `SystemCollectorHandle` do not exist.

- [ ] **Step 3: Implement the two group loops**

Use one Tokio task per group and a shared watch receiver for suppression/shutdown. Normal periods are constants:

```rust
pub const CPU_MEMORY_INTERVAL: Duration = Duration::from_secs(30);
pub const DISK_INTERVAL: Duration = Duration::from_secs(5 * 60);
pub const RETRY_DELAYS: [Duration; 5] = [
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
    Duration::from_secs(240),
    Duration::from_secs(300),
];
```

Set `MissedTickBehavior::Skip`. Sample immediately at startup. On failure, wait the next retry delay instead of the normal interval. On success, reset the retry index. While suppressed, do not request the blocking actor. When suppression clears, perform one fresh current sample per group and reset the normal schedules; never loop through missed ticks.

- [ ] **Step 4: Run deterministic runtime and crate tests**

Run:

```bash
cargo test -p pca-system-collector --test runtime
cargo test -p pca-system-collector
```

Expected: all pass under paused time without real 30-second or 5-minute waits.

- [ ] **Step 5: Commit scheduling**

```bash
git add crates/system-collector
git commit -m "feat: schedule system metric collection"
```

---

### Task 7: Implement Registry, Event Factory, EventSink, and Offline Integration

**Files:**
- Create: `agent/core/src/collector_registry.rs`
- Create: `agent/core/src/event_factory.rs`
- Create: `agent/core/src/event_sink.rs`
- Create: `agent/core/src/system_runtime.rs`
- Modify: `agent/core/src/main.rs:1-3`
- Modify: `agent/core/Cargo.toml`

**Interfaces:**
- Consumes: Tasks 1-6 contracts, `DbActorHandle`, `SystemCollectorHandle`, and `SystemObservation`.
- Produces:

```rust
pub(crate) struct CollectorIdentity {
    pub(crate) workspace_id: Uuid,
    pub(crate) device_id: Uuid,
}
pub(crate) struct CollectorStatusTransition {
    pub(crate) previous_status: CollectorStatus,
    pub(crate) status: CollectorStatus,
    pub(crate) desired_config_revision: u64,
    pub(crate) applied_config_revision: u64,
    pub(crate) reason: &'static str,
    pub(crate) error_code: Option<&'static str>,
}
pub(crate) struct DiskHealthChange {
    pub(crate) active: bool,
    pub(crate) available_bytes: u64,
    pub(crate) threshold_bytes: u64,
}
pub(crate) struct CollectorRegistry;
pub(crate) struct RegistryUpdate {
    pub(crate) state: CollectorState,
    pub(crate) transition: Option<CollectorStatusTransition>,
    pub(crate) health_change: Option<DiskHealthChange>,
    pub(crate) sampling_suppressed: bool,
}
impl CollectorRegistry {
    pub(crate) fn restore(
        prior: Option<CollectorState>,
        identity_available: bool,
        outbox_depth: u64,
        now_ms: i64,
    ) -> (Self, RegistryUpdate);
    pub(crate) fn record_sample(
        &mut self,
        sample: &SystemMetricSample,
        occurred_at_ms: i64,
    ) -> RegistryUpdate;
    pub(crate) fn record_failure(
        &mut self,
        group: MetricGroup,
        error: &SystemSampleError,
        observed_at_ms: i64,
    ) -> RegistryUpdate;
    pub(crate) fn apply_outbox_depth(
        &mut self,
        depth: u64,
        now_ms: i64,
    ) -> RegistryUpdate;
    pub(crate) fn status(&self) -> CollectorStatus;
}
pub(crate) struct DbEventSink {
    database: Arc<DbActorHandle>,
}
pub(crate) struct SystemRuntimeHandle;
pub(crate) enum SystemRuntimeError {
    Database(DbError),
    Domain(DomainError),
    Collector(SystemSampleError),
    Clock,
    WorkerStopped,
}
impl SystemRuntimeHandle {
    pub(crate) async fn start(
        database: Arc<DbActorHandle>,
        identity: Option<CollectorIdentity>,
        data_dir: PathBuf,
    ) -> Result<Self, SystemRuntimeError>;
    #[cfg(test)]
    pub(crate) async fn start_with_source<S: SystemMetricsSource>(
        database: Arc<DbActorHandle>,
        identity: Option<CollectorIdentity>,
        source: S,
    ) -> Result<Self, SystemRuntimeError>;
    pub(crate) async fn shutdown(self) -> Result<(), SystemRuntimeError>;
}
```

Keep Registry, factory, sink, orchestration, and two-hour tests in `#[cfg(test)]` modules beside these private binary modules. Do not add a public `agent/core` library solely for tests.

- [ ] **Step 1: Write pure Registry state-machine tests**

Test exact boundaries and multiple reasons:

```rust
#[test]
fn backpressure_uses_hysteresis_and_restart_uses_high_water_only() {
    let mut registry = running_registry();
    assert!(!registry.apply_outbox_depth(10_000, now()).sampling_suppressed);
    assert!(registry.apply_outbox_depth(10_001, now()).sampling_suppressed);
    assert!(registry.apply_outbox_depth(8_000, now()).sampling_suppressed);
    assert!(!registry.apply_outbox_depth(7_999, now()).sampling_suppressed);
    assert!(!CollectorRegistry::restore(Some(prior_state()), true, 9_000, now())
        .1
        .sampling_suppressed);
}

#[test]
fn one_recovery_does_not_clear_another_degradation_reason() {
    let mut registry = running_registry();
    registry.record_failure(MetricGroup::CpuMemory, &retryable_error(), now());
    registry.apply_outbox_depth(10_001, now());
    registry.record_sample(&cpu_memory_sample(), now());
    assert_eq!(registry.status(), CollectorStatus::Degraded);
}
```

Also test:

- unpaired disabled `0/0`;
- initialization waiting for both first groups;
- unsupported/error classifications;
- repeated status without duplicate transition;
- last-event/health semantics;
- clearing `last_error_code` only after complete recovery;
- first healthy disk sample emits no health change;
- first low disk sample emits one active `DISK_SPACE_LOW` change;
- repeated low samples emit no additional change;
- recovery emits one inactive change;
- a process restart with an immediately low first sample reasserts one active change.

- [ ] **Step 2: Run Registry tests and verify they fail**

Run:

```bash
cargo test -p pca-agentd registry
```

Expected: compile failure because Registry modules do not exist.

- [ ] **Step 3: Implement strict Event construction**

Create factory functions:

```rust
pub(crate) fn metric_event(
    identity: &CollectorIdentity,
    sample: &SystemMetricSample,
    event_id: Uuid,
    occurred_at: OffsetDateTime,
    created_at: OffsetDateTime,
) -> Result<EventEnvelope, DomainError>;

pub(crate) fn status_event(
    identity: &CollectorIdentity,
    transition: &CollectorStatusTransition,
    event_id: Uuid,
    occurred_at: OffsetDateTime,
    created_at: OffsetDateTime,
) -> Result<EventEnvelope, DomainError>;

pub(crate) fn health_event(
    identity: &CollectorIdentity,
    change: &DiskHealthChange,
    event_id: Uuid,
    occurred_at: OffsetDateTime,
    created_at: OffsetDateTime,
) -> Result<EventEnvelope, DomainError>;
```

Use `system.metric_sampled`, `collector.status_changed`, and `system.health_changed` exactly. Serialize typed payloads to a JSON object, reject any non-object result, set schema version 1, sensitivity `normal`, empty attachments, and no idempotency key. Generate the Event ID once before calling EventSink and retain the entire pending `EventCommit` across timeout retry; each attempt passes `pending_commit.clone()` so a timeout cannot discard the stable IDs.

- [ ] **Step 4: Implement the five-second DbEventSink**

Implement the object-safe domain trait:

```rust
impl EventSink for DbEventSink {
    fn commit<'a>(&'a self, commit: EventCommit) -> EventSinkFuture<'a> {
        Box::pin(async move {
            tokio::time::timeout(
                Duration::from_secs(5),
                self.database.commit_events(&commit),
            )
            .await
            .map_err(|_| DomainError::new("COLLECTOR_TIMEOUT", "event commit timed out", true))?
            .map_err(map_database_error)
        })
    }
}

fn map_database_error(_error: DbError) -> DomainError {
    DomainError::new(
        "COLLECTOR_DEGRADED",
        "collector persistence unavailable",
        true,
    )
}
```

Map every DbActor commit error to retryable `COLLECTOR_DEGRADED` with the fixed public message `collector persistence unavailable`; keep the detailed `DbError` only in redacted local tracing. Unit-test the deadline with paused Tokio time and a helper future that never completes. Test that retry passes the same Event IDs to a recording sink.

- [ ] **Step 5: Implement System orchestration**

`SystemRuntimeHandle::start` does the following:

1. Load prior state.
2. With no identity, state-only upsert `disabled` and do not create a sampler.
3. With identity, persist `initializing` plus one status Event.
4. Start the System Collector and a 30-second Outbox-depth monitor.
5. Convert each observation into a Registry update.
6. Build a bounded `EventCommit` containing the metric, optional status transition, optional disk-health transition, and final state.
7. For a repeated failure with no Event, use state-only upsert and never advance `last_event_at_ms`.
8. If EventSink fails or times out, add a persistence degradation transition to that same pending commit, replace its final state with `degraded`, suppress new sampling, and retry only this one retained commit at 30/60/120/240/300-second capped delays.
9. Create the added persistence status Event ID once; every retry clones the same rebuilt commit. The maximum remains four Events.
10. After the pending commit succeeds, emit one persistence-recovered status transition, then remove suppression if no other reason remains.
11. Send suppression changes to the Collector handle.
12. On shutdown, stop schedules, drain no synthetic samples, join the sampler actor, and release all DbActor references.

Add a recording EventSink test that fails twice and succeeds once. Assert that no new sample is requested while the commit is pending, all three attempts carry identical Event IDs, only one durable batch is observed, and recovery does not clear a simultaneous sampling/backpressure reason.

- [ ] **Step 6: Add the two-hour virtual offline integration test**

Start the runtime with a Fake source, valid identity, paused Tokio time, and a real temporary DbActor. Advance time in 240 increments of 30 seconds, yielding after each increment so the runtime observes every scheduled deadline; a single two-hour jump would intentionally exercise missed-tick skipping instead of the offline cadence. After exactly two virtual hours, stop cleanly and query SQLite:

```rust
assert_eq!(metric_count(&connection, "cpu_memory"), 241);
assert_eq!(metric_count(&connection, "disk"), 25);
assert_eq!(orphan_event_count(&connection), 0);
assert_eq!(duplicate_event_id_count(&connection), 0);
assert_eq!(event_count(&connection), outbox_count(&connection));
```

Count status/health Events separately so they do not alter the two metric assertions. Add an unpaired integration test asserting durable disabled state with zero `system.%` and `collector.status_changed` rows.

- [ ] **Step 7: Run Core integration tests**

Run:

```bash
cargo test -p pca-agentd
```

Expected: Registry, timeout, unpaired, valid identity, and two-hour virtual tests pass.

- [ ] **Step 8: Commit Core orchestration**

```bash
git add agent/core
git commit -m "feat: run system collector through registry"
```

---

### Task 8: Wire Debug Identity and Process-Level Durability

**Files:**
- Modify: `agent/core/src/config.rs:8-195`
- Modify: `agent/core/src/app.rs:1-430`
- Modify: `crates/db-local/src/actor.rs:14-52`
- Modify: `crates/db-local/src/repository.rs`
- Create: `agent/core/tests/system_collector_process.rs`
- Create: `agent/core/tests/collector_commit_kill.rs`
- Modify: `agent/core/tests/process_lifecycle.rs`
- Modify: `agent/core/Cargo.toml`

**Interfaces:**
- Consumes: `SystemRuntimeHandle::start`, `CollectorIdentity`, and DbActor Collector commit.
- Produces debug-only CLI pairs:

```text
--process-test-workspace-id <uuid>
--process-test-device-id <uuid>

--process-test-collector-barrier-ready <runtime-root/Run/file>
--process-test-collector-barrier-release <runtime-root/Run/file>
```

- [ ] **Step 1: Write failing CLI-security tests**

Under `process-test-hooks`, assert:

- identity flags must appear together;
- both values parse as non-nil UUIDs;
- flags require `run` plus an explicit safe `--runtime-root`;
- collector barrier paths are distinct siblings inside `Run/`;
- production/default builds reject all four new flag names with exit code 2.

Use fixed valid IDs:

```text
018f3f4a-2d9b-7d21-a310-2c49d9b43c13
018f3f4a-2d9b-7d21-a310-2c49d9b43c14
```

- [ ] **Step 2: Run config/process tests and verify they fail**

Run:

```bash
cargo test -p pca-agentd --test process_lifecycle
cargo test -p pca-agentd --features process-test-hooks --test process_lifecycle
```

Expected: tests fail because the flags are not recognized or represented in `RunConfig`.

- [ ] **Step 3: Add feature-gated config and app wiring**

Add optional `ProcessTestIdentityConfig` and Collector barrier config under `#[cfg(feature = "process-test-hooks")]`. In a normal build, `RunConfig::collector_identity()` always returns `None`. In the feature build it returns `Some(CollectorIdentity)` only for the validated pair.

Add `system_runtime: Option<SystemRuntimeHandle>` to `RuntimeResources`. Start it after DbActor health and lifecycle startup, using `config.paths.data_dir`. Stop and join it before lifecycle, Bridge, checkpoint, and DbActor shutdown. Add `SystemCollector` and `SystemCollectorCleanup` failure stages without changing existing exit-code values.

- [ ] **Step 4: Add a Collector-transaction process barrier**

Keep the existing S1A Event/Outbox barrier unchanged. Add a separate optional hook used only by `commit_events` when `collector_state.is_some()`. Place it after all Event and Outbox inserts and before the Collector-state upsert:

```rust
for event in &commit.events {
    insert_event(&transaction, event)?;
    insert_stable_outbox(&transaction, event)?;
}
wait_at_collector_commit_barrier_if_configured(hooks)?;
upsert_collector_state_in(&transaction, state)?;
transaction.commit()?;
```

The hook is absent from release builds and uses the same absolute-sibling, regular-file, no-symlink, ten-second safety rules as the existing barrier.

- [ ] **Step 5: Add real process System sampling test**

Spawn feature-built `pca-agentd` with a temporary runtime root and valid identity flags. Poll SQLite for at most five seconds until:

- `collector_states.system` is `running`;
- one `cpu_memory` metric exists;
- one `disk` metric exists;
- every selected Event has one Outbox row;
- payload JSON contains neither the temporary root path nor current PID.

Terminate the child, assert exit 0, restart without identity flags, and assert the durable System state becomes `disabled` and no additional System metric is emitted.

- [ ] **Step 6: Add the kill-boundary test**

Spawn with valid identity plus Collector barrier flags. Wait for the ready file, send `SIGKILL`, reopen SQLite, and query only the first `collector.registry` transition and `collector_states.system`:

```rust
assert!(
    matches!(counts, (0, 0, 0) | (1, 1, 1)),
    "partial collector transaction: {counts:?}"
);
```

Existing lifecycle Event rows are outside this assertion. Confirm the forced kill retains the crash marker.

- [ ] **Step 7: Run feature and release-boundary process tests**

Run:

```bash
cargo test -p pca-agentd --test process_lifecycle
cargo test -p pca-agentd --features process-test-hooks \
  --test process_lifecycle \
  --test system_collector_process \
  --test collector_commit_kill
cargo build -p pca-agentd --release
```

Expected: all tests and release build pass; the default binary rejects test-only flags.

- [ ] **Step 8: Commit process integration**

```bash
git add agent/core crates/db-local
git commit -m "test: prove system collector process boundaries"
```

---

### Task 9: Update Gates, Documentation, Performance Evidence, and Run Full Regression

**Files:**
- Create: `scripts/s2_system_performance.py`
- Create: `scripts/verify-s2-system-performance.py`
- Create: `scripts/tests/test_s2_system_performance.py`
- Modify: `scripts/verify-full.sh`
- Modify: `.github/workflows/ci.yml`
- Modify: `docs/PRODUCT_TECH_SPEC_V1.1.md:1048-1055`
- Modify: `docs/PRODUCT_TECH_SPEC_V1.1.md:2252-2265`
- Modify: `ARCHITECTURE.md:140-170`
- Modify: `PERFORMANCE.md:15-25`
- Modify: `tasks/S2_CORE_COLLECTORS.md`

**Interfaces:**
- Consumes: completed System slice and all verification commands.
- Produces: repeatable local performance evidence, CI coverage for feature-gated process tests, updated data dictionary/architecture, and an explicit partial-S2 status.

- [ ] **Step 1: Write failing performance-script unit tests**

Test parsing and thresholds without launching a process:

```python
def test_summary_enforces_existing_budget(self) -> None:
    summary = summarize_samples([(0.4, 80 * 1024), (0.8, 90 * 1024)])
    self.assertLess(summary.average_cpu_percent, 1.0)
    self.assertLess(summary.peak_rss_kib, 120 * 1024)

def test_over_budget_sample_fails(self) -> None:
    with self.assertRaises(BudgetExceeded):
        enforce_budget(summarize_samples([(1.2, 90 * 1024)]))
```

- [ ] **Step 2: Run the performance unit test and verify it fails**

Run:

```bash
python3 -m unittest scripts.tests.test_s2_system_performance -v
```

Expected: import failure because the verifier does not exist.

- [ ] **Step 3: Implement the bounded local performance verifier**

The script must:

1. require an explicit feature-built agent binary path and safe temporary runtime root;
2. start agentd with the fixed valid test identity;
3. wait for `collector_states.system = running`;
4. discard a ten-second warm-up;
5. sample `/bin/ps -p PID -o %cpu=,rss=` every five seconds for 60 seconds;
6. fail if average CPU is not below 1.0 or peak RSS is not below 122,880 KiB;
7. terminate and reap the exact child in bounded cleanup;
8. print sample count, average CPU, and peak RSS.

Put parsing, summarization, budget enforcement, process ownership, and bounded cleanup in importable `scripts/s2_system_performance.py`. Keep `verify-s2-system-performance.py` as a thin `main()` entrypoint so unit tests never launch the live probe.

Do not make this timing-sensitive performance probe a required shared-CI gate. Run it on the target Apple Silicon Mac during completion verification.

- [ ] **Step 4: Add process tests to deterministic gates**

Append to `scripts/verify-full.sh` and the macOS CI job:

```bash
cargo test -p pca-agentd --features process-test-hooks \
  --test process_lifecycle \
  --test system_collector_process \
  --test collector_commit_kill
```

Add a separate CI `rust-msrv` job using `dtolnay/rust-toolchain@1.82.0` and `cargo check --workspace --all-targets`. Keep the existing stable job for current Clippy/rustfmt coverage.

Keep the real 60-second performance verifier outside CI. Existing `cargo test --workspace` continues to cover domain, database, scheduler, Registry, and virtual-time tests.

- [ ] **Step 5: Update authoritative documentation**

Document:

- System CPU/memory and disk payload fields and intervals;
- `collector_states.last_health_at_ms`, timestamps, and no extra index;
- Event/Outbox/CollectorState transaction boundary;
- exact 10,000/8,000 hysteresis;
- exact `sysinfo` dependency/features and blocking actor;
- privacy exclusions;
- Battery/Power and Network deferral.

In `tasks/S2_CORE_COLLECTORS.md`, add a dated “System vertical slice complete” subsection while leaving the overall objective and exit gate open. Do not mark Activity, Screenshot, permission revoke, Attachment, or full S2 complete.

- [ ] **Step 6: Run focused formatting and verification**

Run:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo +1.82.0 check --workspace --all-targets
cargo test --workspace
cargo test -p pca-agentd --features process-test-hooks \
  --test process_lifecycle \
  --test system_collector_process \
  --test collector_commit_kill
pnpm typecheck
pnpm test
python3 scripts/verify_migrations.py .
python3 scripts/verify_boundaries.py .
python3 -m unittest scripts.tests.test_engineering_gates scripts.tests.test_s2_system_performance -v
```

Expected: every command exits 0.

- [ ] **Step 7: Run full S1A and platform regression**

Run:

```bash
./scripts/verify-full.sh
swift test --package-path platform/macos
xcodebuild test \
  -project platform/macos/PersonalComputerAgent.xcodeproj \
  -scheme PersonalComputerAgent \
  -destination 'platform=macOS,arch=arm64' \
  CODE_SIGNING_ALLOWED=NO
python3 -m unittest \
  scripts.tests.test_s1a_packaging \
  scripts.tests.test_s1a_live_verification \
  scripts.tests.test_run_with_timeout \
  scripts.tests.test_process_inspector -v
```

Expected: all exit 0. If native-focus Swift signal tests require isolation, run the three commands exactly as separated in `.github/workflows/ci.yml`; do not hide a failure by broad retries.

- [ ] **Step 8: Run target-Mac performance acceptance**

Build the debug feature binary and run:

```bash
cargo build -p pca-agentd --features process-test-hooks
python3 scripts/verify-s2-system-performance.py \
  --agent target/debug/pca-agentd
```

Expected: at least 12 measured samples, average CPU below 1.0%, peak RSS below 122,880 KiB, exit 0.

- [ ] **Step 9: Inspect dependency and privacy boundaries**

Run:

```bash
cargo tree -p pca-system-collector -e features
rg -n "mount_point|filesystem|command_line|environment|SSID|battery|network" \
  crates/system-collector agent/core/src contracts packages/contracts
git diff --check
git status --short
```

Expected: dependency tree enables only the approved `sysinfo` features; `mount_point` appears only in volume selection and privacy tests, while the other search hits are tests or explicit rejection/privacy assertions rather than emitted payload fields; no whitespace errors or unrelated files.

- [ ] **Step 10: Commit gates and documentation**

```bash
git add \
  .github/workflows/ci.yml \
  scripts/s2_system_performance.py \
  scripts/verify-full.sh \
  scripts/verify-s2-system-performance.py \
  scripts/tests/test_s2_system_performance.py \
  docs/PRODUCT_TECH_SPEC_V1.1.md \
  ARCHITECTURE.md \
  PERFORMANCE.md \
  tasks/S2_CORE_COLLECTORS.md
git commit -m "docs: verify S2 system collector slice"
```

---

## Final Completion Checklist

- [ ] Production/default agentd rejects test identity flags and persists `system=disabled`.
- [ ] Debug valid identity produces immediate CPU/memory and disk Events at revision `0/0`.
- [ ] CPU/memory and disk schedules, retry cap, suppression, restart, and no-catch-up behavior are deterministic.
- [ ] Every metric/status/health Event has exactly one stable Outbox row.
- [ ] Event/Outbox/CollectorState process-kill test proves all-or-none durability.
- [ ] Two-hour virtual offline test reports 241 CPU/memory and 25 disk metrics with no loss or duplicate.
- [ ] Active Outbox depth and 10,000/8,000 hysteresis are tested at exact boundaries.
- [ ] Real `sysinfo` smoke and privacy assertions pass on macOS.
- [ ] Exact `sysinfo 0.33.1` feature tree contains no default multithread/Rayon, Network, User, or Component feature.
- [ ] Target-Mac active collection remains below average 1% CPU and 120 MB peak RSS.
- [ ] Format, Clippy `-D warnings`, Rust tests, process tests, TypeScript tests, contracts, migrations, boundaries, Swift, S1A packaging, and S1A live-verifier regression all pass.
- [ ] Documentation states that only the System vertical slice is complete and the full S2 task remains open.
