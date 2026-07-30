# S1A Local Runtime and Self-Install DMG Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build an Apple Silicon, macOS 13+ self-use DMG that installs a user-level PCA Rust runtime and resident Swift PlatformBridge, preserves local SQLite facts across upgrades, and starts automatically after login.

**Architecture:** One Apple Development-signed SwiftUI app copies its complete bundle from the DMG into the current user's Application Support directory, relaunches from the installed path, and registers an embedded `SMAppService` LaunchAgent. `launchd` owns the Rust `agentd`; Rust owns runtime state and SQLite and supervises a resident Swift Bridge over an authenticated `0600` Unix socket. Program files, persistent data, and ephemeral runtime files are separate so upgrade rollback cannot erase user data.

**Tech Stack:** Full Xcode with Swift 6 support, SwiftUI, ServiceManagement, Security, IOKit/AppKit power notifications, Rust stable 1.82+ with Tokio, rusqlite, Serde, HMAC-SHA256, Python 3.9 standard library, Bash, `codesign`, `hdiutil`, GitHub Actions.

## Global Constraints

- The approved design is `docs/superpowers/specs/2026-07-31-s1a-local-runtime-installer-design.md`.
- Target only Apple Silicon `arm64` and macOS 13 or later.
- The install root is exactly `$HOME/Library/Application Support/PersonalComputerAgent`.
- `App/`, `Data/`, and `Run/` are separate; upgrades replace only `App/`.
- Use a user-level `SMAppService` LaunchAgent. Do not add a root LaunchDaemon or privileged helper.
- Use free Apple Development / Personal Team signing for the self-use DMG. Do not add Developer ID, notarization, Sparkle, or automatic quarantine removal.
- Rust `agentd` owns runtime state, SQLite, Event Store, Outbox, heartbeat, and Bridge supervision.
- Swift owns Installer/Setup, `SMAppService`, Keychain access, TCC status reads, power notifications, and the Bridge server.
- Swift must not access business SQLite or Cloud APIs.
- S1A adds lifecycle Events only. Do not add Activity, Screenshot, Browser, File, Location, or WeChat Collectors.
- S1A has no Cloud API, PostgreSQL, pairing, R2, batch sync, remote command, or local persistent status UI.
- IPC is length-prefixed JSON over a Unix socket with directory mode `0700`, socket mode `0600`, version negotiation, deadlines, nonce challenge, and an HMAC proof using a Keychain secret.
- Secrets, nonces, raw Bridge payloads, Apple account data, Team private keys, and signing passwords must never enter Git or logs.
- Every migration is immutable; Event and Outbox rows commit in one transaction.
- Do not use shell interpolation to execute runtime subprocesses. Use fixed executable paths and argument arrays.
- Existing S0 structural/full gates must stay green after every task.

---

### Task 1: Install the Xcode Toolchain and Freeze the Installation Decision

**Files:**
- Create: `docs/adr/ADR-0005-user-level-self-install-channel.md`
- Create: `docs/INSTALLATION_CHANNELS.md`
- Create: `tasks/S1A_LOCAL_RUNTIME_INSTALLER.md`
- Create: `tasks/S1B_CLOUD_CONTROL_PLANE.md`
- Modify: `ARCHITECTURE.md`
- Modify: `tasks/S1_RUST_CORE_SWIFT_BRIDGE.md`

**Interfaces:**
- Consumes: the approved S1A design and the existing `/Applications` product-spec decision.
- Produces: an explicit self-use channel decision that later tasks may implement without treating `/Applications` and Application Support as simultaneous targets.

- [ ] **Step 1: Prove the current Xcode preflight is RED**

Run:

```bash
xcode-select -p
xcodebuild -version
```

Expected current result: `xcode-select` prints `/Applications/Xcode.app/Contents/Developer`; `xcodebuild` exits 69 because the newly selected Xcode license has not yet been accepted.

- [ ] **Step 2: Complete first-run setup for the installed full Xcode**

Xcode is installed and selected. The developer must run the administrator-authorized first-launch commands in Terminal:

```bash
sudo xcode-select --switch /Applications/Xcode.app/Contents/Developer
sudo xcodebuild -license accept
xcodebuild -runFirstLaunch
xcodebuild -version
swift --version
```

Expected: `xcodebuild -version` and Swift 6 version output exit 0. This step changes only the development machine; target Macs do not install Xcode.

- [ ] **Step 3: Write the ADR and channel documentation**

`ADR-0005` must state:

```text
Decision: the S1A self-use channel installs to
~/Library/Application Support/PersonalComputerAgent/App/PersonalComputerAgent.app.

The future public channel may return to /Applications, but no implementation
may support both locations implicitly. S1A uses a user LaunchAgent and never root.
```

`INSTALLATION_CHANNELS.md` must list the fixed App/Data/Run paths, Apple Development signing, manual Gatekeeper/background approval, and local uninstall command. Split the old S1 task card into S1A and S1B without changing the approved S2/S3 order.

- [ ] **Step 4: Verify documentation consistency**

Run:

```bash
rg -n "/Applications|Application Support|LaunchDaemon|LaunchAgent|S1A|S1B" \
  ARCHITECTURE.md docs/INSTALLATION_CHANNELS.md docs/adr/ADR-0005-user-level-self-install-channel.md tasks
git diff --check
```

Expected: the self-use channel has one exact location; Root is explicitly excluded; the older `/Applications` decision is described only as a different/future channel rather than an active S1A path.

- [ ] **Step 5: Commit**

```bash
git add ARCHITECTURE.md docs/INSTALLATION_CHANNELS.md docs/adr/ADR-0005-user-level-self-install-channel.md tasks
git commit -m "docs: freeze S1A self-install channel"
```

---

### Task 2: Freeze Runtime Status and Authenticated Bridge Fixtures

**Files:**
- Create: `packages/contracts/runtime-status.schema.json`
- Create: `packages/contracts/fixtures/runtime-status.local-healthy.json`
- Create: `packages/contracts/fixtures/bridge-handshake.challenge.json`
- Create: `packages/contracts/fixtures/bridge-handshake.response.json`
- Modify: `packages/contracts/src/types.ts`
- Modify: `packages/contracts/tests/contracts.test.ts`
- Modify: `crates/domain/src/lib.rs`
- Modify: `crates/test-contracts/tests/fixtures.rs`
- Create: `platform/macos/Sources/BridgeProtocol/HandshakePayload.swift`
- Modify: `platform/macos/Sources/BridgeContractVerifier/main.swift`

**Interfaces:**
- Consumes: S0 `BridgeEnvelope`, `AgentStatus`, `BridgeMessageKind`, `JSONValue`, and shared fixture directories.
- Produces: `RuntimeStatusEnvelope`, `BridgeStatus`, `HandshakeChallenge`, and `HandshakeResponse` with identical snake_case wire fields across TypeScript, Rust, and Swift.

- [ ] **Step 1: Add failing TypeScript contract tests**

Add literal assertions:

```ts
test("local runtime status uses the canonical health fields", () => {
  const value = readJson("runtime-status.local-healthy.json");
  assert.equal(value.agent_status, "unpaired");
  assert.equal(value.bridge_status, "ready");
  assert.equal(value.local_healthy, true);
  assert.equal(typeof value.heartbeat_at, "string");
});

test("handshake fixtures never carry the shared secret", () => {
  for (const name of ["bridge-handshake.challenge.json", "bridge-handshake.response.json"]) {
    const value = JSON.stringify(readJson(name));
    assert.equal(value.includes("shared_secret"), false);
  }
});
```

- [ ] **Step 2: Run TypeScript tests and verify RED**

Run: `pnpm --filter @pca/contracts test`

Expected: fail because the three fixtures and runtime schema do not exist.

- [ ] **Step 3: Add exact wire types and fixtures**

Use these fields:

```ts
export type BridgeStatus =
  | "disconnected" | "handshaking" | "ready"
  | "degraded" | "incompatible" | "stopped";

export interface RuntimeStatusEnvelope {
  agent_status: AgentStatus;
  bridge_status: BridgeStatus;
  local_healthy: boolean;
  heartbeat_at: string;
  process_id: number;
  app_version: string;
  schema_version: number;
}

export interface HandshakeChallenge {
  phase: "challenge";
  nonce: string;
  agent_version: string;
}

export interface HandshakeResponse {
  phase: "response";
  nonce: string;
  proof: string;
  bridge_version: string;
}
```

The challenge fixture is a Bridge request with capability `bridge.handshake`; the response echoes the base64 nonce and contains a synthetic base64 HMAC proof. No fixture contains a real credential.

- [ ] **Step 4: Add Rust and Swift mappings and negative assertions**

Rust `AgentStatus` and the new `BridgeStatus` derive snake_case Serde. Add fixture decoding assertions for every field. Swift adds `HandshakeChallenge` and `HandshakeResponse` as `Codable`, `Sendable`, and `Equatable`; the executable verifier decodes both shared fixtures and rejects a mismatched phase.

- [ ] **Step 5: Run all contract gates**

```bash
pnpm typecheck
pnpm test
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p pca-test-contracts
swift run --package-path platform/macos BridgeContractVerifier
```

Expected: all commands exit 0; existing Appendix C/D registry coverage stays unchanged.

- [ ] **Step 6: Commit**

```bash
git add packages/contracts crates/domain crates/test-contracts platform/macos
git commit -m "feat: freeze S1A runtime and handshake contracts"
```

---

### Task 3: Implement the Immutable S1A SQLite Slice

**Files:**
- Create: `crates/db-local/migrations/0001_s1a_runtime.sql`
- Create: `crates/db-local/src/actor.rs`
- Create: `crates/db-local/src/error.rs`
- Create: `crates/db-local/src/migrations.rs`
- Create: `crates/db-local/src/repository.rs`
- Modify: `crates/db-local/src/lib.rs`
- Modify: `crates/db-local/Cargo.toml`
- Create: `crates/db-local/tests/runtime_store.rs`
- Modify: `scripts/verify_migrations.py`
- Modify: `scripts/tests/test_engineering_gates.py`

**Interfaces:**
- Consumes: `pca_domain::EventEnvelope` and ordered SQL files whose numeric prefixes are unique.
- Produces: `DbActorHandle::open`, `append_event_with_outbox`, `set_agent_state`, `health`, and `checkpoint` async methods.

- [ ] **Step 1: Write failing migration and atomicity tests**

Use this public surface:

```rust
let db = DbActorHandle::open(&database_path, "0.1.0").await?;
db.append_event_with_outbox(&event).await?;
let counts = db.count_event_and_outbox(&event.event_id).await?;
assert_eq!(counts, (1, 1));
```

Add tests for: empty database migration, replay without schema change, duplicate event idempotency, a forced Outbox insert failure rolling back the Event, locked database timeout, integrity failure, and unsupported future schema version.

- [ ] **Step 2: Run tests and verify RED**

Run: `cargo test -p pca-db-local --test runtime_store`

Expected: compile failure because `DbActorHandle` and `0001_s1a_runtime.sql` do not exist.

- [ ] **Step 3: Add the minimum migration**

Create only these tables and indexes:

```sql
CREATE TABLE local_meta (...);
CREATE TABLE agent_state (... CHECK (singleton_id = 1));
CREATE TABLE events_local (... payload_json TEXT NOT NULL ...);
CREATE TABLE sync_outbox (... event_id TEXT NOT NULL UNIQUE ...);
CREATE TABLE diagnostic_events (... redacted_json TEXT NOT NULL ...);
CREATE INDEX idx_events_local_occurred_at ON events_local(occurred_at_ms);
CREATE INDEX idx_sync_outbox_state_created ON sync_outbox(state, created_at_ms);
```

Use foreign keys from Outbox to Event. Do not add Collector or projection tables without an S1A producer.

- [ ] **Step 4: Implement a dedicated database thread**

Add `rusqlite` with the `bundled` feature and a bounded Tokio MPSC request channel. The thread exclusively owns `rusqlite::Connection`; async callers receive results through oneshot channels. On open, set WAL, foreign keys, `busy_timeout=5000`, and `synchronous=NORMAL`, then run migrations, `integrity_check`, `foreign_key_check`, and smoke queries.

`append_event_with_outbox` must use one transaction and `INSERT ... ON CONFLICT DO NOTHING` keyed by stable event/outbox IDs.

- [ ] **Step 5: Extend migration verification**

`verify_migrations.py` must accept `0000_baseline.sql` followed by `0001_s1a_runtime.sql`, reject duplicate or non-monotonic IDs, replay the complete local chain twice, compare schema definitions, and print SHA-256 for every migration.

- [ ] **Step 6: Run database gates**

```bash
python3 -m unittest scripts.tests.test_engineering_gates -v
python3 scripts/verify_migrations.py .
cargo fmt --all --check
cargo clippy -p pca-db-local --all-targets -- -D warnings
cargo test -p pca-db-local
```

Expected: all exit 0, including failure-path tests.

- [ ] **Step 7: Commit**

```bash
git add crates/db-local scripts/verify_migrations.py scripts/tests/test_engineering_gates.py Cargo.lock
git commit -m "feat: add durable S1A local store"
```

---

### Task 4: Build Runtime Paths, State, Lock, Crash Marker, and Heartbeat

**Files:**
- Create: `crates/agent-runtime/Cargo.toml`
- Create: `crates/agent-runtime/src/lib.rs`
- Create: `crates/agent-runtime/src/paths.rs`
- Create: `crates/agent-runtime/src/state.rs`
- Create: `crates/agent-runtime/src/single_instance.rs`
- Create: `crates/agent-runtime/src/crash_marker.rs`
- Create: `crates/agent-runtime/src/heartbeat.rs`
- Create: `crates/agent-runtime/tests/runtime_foundation.rs`
- Modify: `Cargo.toml`

**Interfaces:**
- Consumes: exact per-user install root and domain `AgentStatus`/`BridgeStatus`.
- Produces: `RuntimePaths`, `RuntimeStateMachine`, `SingleInstanceGuard`, `CrashMarkerGuard`, and `LocalHeartbeatWriter`.

- [ ] **Step 1: Write failing foundation tests**

Tests must assert:

```rust
let paths = RuntimePaths::under(&temporary_root);
paths.create_securely()?;
assert_eq!(mode(&paths.data_dir) & 0o777, 0o700);
assert_eq!(mode(&paths.run_dir) & 0o777, 0o700);

let first = SingleInstanceGuard::acquire(&paths.lock_file)?;
assert!(matches!(SingleInstanceGuard::acquire(&paths.lock_file), Err(RuntimeError::AlreadyRunning)));
drop(first);
```

Also assert legal/illegal state transitions, crash marker detection after an unclean test exit, atomic heartbeat-file replacement, and no secret/raw payload fields in serialized status.

- [ ] **Step 2: Run tests and verify RED**

Run: `cargo test -p pca-agent-runtime --test runtime_foundation`

Expected: Cargo reports the package is absent.

- [ ] **Step 3: Implement the minimum foundation**

Use `fs2::FileExt::try_lock_exclusive` for the instance lock, `std::os::unix::fs::PermissionsExt` for modes, and write status to a sibling temporary file followed by `rename`. State transitions are an explicit match table; no arbitrary `set_status` function is exposed.

The heartbeat payload is the canonical `RuntimeStatusEnvelope`; write at startup and then every two seconds so the five-second health gate has margin.

- [ ] **Step 4: Run Rust gates**

```bash
cargo fmt --all --check
cargo clippy -p pca-agent-runtime --all-targets -- -D warnings
cargo test -p pca-agent-runtime
```

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/agent-runtime
git commit -m "feat: add S1A runtime foundation"
```

---

### Task 5: Add a Keychain Credential Port Without File Fallback

**Files:**
- Create: `crates/keychain/Cargo.toml`
- Create: `crates/keychain/src/lib.rs`
- Create: `crates/keychain/src/macos.rs`
- Create: `crates/keychain/tests/credential_store.rs`
- Modify: `Cargo.toml`
- Create: `platform/macos/Sources/BridgeProtocol/KeychainCredentialStore.swift`

**Interfaces:**
- Consumes: service `com.pca.bridge`, account `shared-secret-v1`, and a 32-byte secret.
- Produces: Rust `CredentialStore` plus `MacOSKeychainStore`, and Swift `KeychainCredentialStore`, with `load`, `store`, and `delete` operations.

- [ ] **Step 1: Write failing Rust behavior tests**

Define:

```rust
pub trait CredentialStore: Send + Sync {
    fn load(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, CredentialError>;
    fn store(&self, service: &str, account: &str, secret: &[u8]) -> Result<(), CredentialError>;
    fn delete(&self, service: &str, account: &str) -> Result<(), CredentialError>;
}
```

Use an in-memory test implementation to assert create, overwrite, read, delete, and unavailable behavior. Add a source scan that fails if production code writes the shared secret below `Data/` or `Run/`.

- [ ] **Step 2: Run and verify RED**

Run: `cargo test -p pca-keychain`

Expected: package absent.

- [ ] **Step 3: Implement macOS Keychain adapters**

Rust uses the `security-framework` crate generic-password APIs. Swift uses Security `SecItemCopyMatching`, `SecItemAdd`/`SecItemUpdate`, and `SecItemDelete`. Neither adapter logs OSStatus-associated query values or falls back to a plaintext file.

- [ ] **Step 4: Run focused gates**

```bash
cargo clippy -p pca-keychain --all-targets -- -D warnings
cargo test -p pca-keychain
swift build --package-path platform/macos
```

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/keychain platform/macos/Sources/BridgeProtocol/KeychainCredentialStore.swift
git commit -m "feat: add shared Keychain credential port"
```

---

### Task 6: Implement Authenticated Rust Bridge Framing and Supervision

**Files:**
- Create: `crates/bridge-client/Cargo.toml`
- Create: `crates/bridge-client/src/lib.rs`
- Create: `crates/bridge-client/src/framing.rs`
- Create: `crates/bridge-client/src/auth.rs`
- Create: `crates/bridge-client/src/client.rs`
- Create: `crates/bridge-client/src/supervisor.rs`
- Create: `crates/bridge-client/tests/fake_bridge.rs`
- Modify: `Cargo.toml`

**Interfaces:**
- Consumes: `BridgeEnvelope`, Keychain `CredentialStore`, installed Bridge executable path, socket path, and protocol version `1`.
- Produces: `BridgeClient::connect_and_handshake`, `BridgeClient::request`, and `BridgeSupervisor::run` with status notifications.

- [ ] **Step 1: Write failing framing and fake-server tests**

Frame format is exactly:

```text
4-byte unsigned big-endian JSON byte length
UTF-8 JSON bytes
```

Tests cover fragmented reads, two frames in one read, zero/oversized length rejection, invalid JSON, deadline expiration, mismatched nonce, invalid HMAC, incompatible version, child crash, and successful reconnect.

- [ ] **Step 2: Run and verify RED**

Run: `cargo test -p pca-bridge-client --test fake_bridge`

Expected: package absent.

- [ ] **Step 3: Implement framing and HMAC handshake**

Use `hmac` + `sha2`; calculate `HMAC-SHA256(secret, nonce || protocol_version || agent_version)`. Compare proofs with constant-time verification from the HMAC API. Limit frames to 1 MiB. Generate a fresh 32-byte nonce for every connection and enforce handshake/request deadlines with `tokio::time::timeout`.

- [ ] **Step 4: Implement bounded supervision**

Spawn only the fixed Bridge executable with a fixed `--socket <absolute-path>` argument array. Backoff sequence is capped at 30 seconds and reset after a stable ready period. Supervisor emits `Disconnected`, `Handshaking`, `Ready`, `Degraded`, or `Incompatible`; incompatible protocol does not restart-loop.

- [ ] **Step 5: Run gates**

```bash
cargo fmt --all --check
cargo clippy -p pca-bridge-client --all-targets -- -D warnings
cargo test -p pca-bridge-client
python3 scripts/verify_boundaries.py .
```

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/bridge-client
git commit -m "feat: add authenticated Bridge client supervisor"
```

---

### Task 7: Implement the Resident Swift PlatformBridge

**Files:**
- Create: `platform/macos/Sources/PlatformBridge/BridgeServer.swift`
- Create: `platform/macos/Sources/PlatformBridge/FrameCodec.swift`
- Create: `platform/macos/Sources/PlatformBridge/HandshakeHandler.swift`
- Create: `platform/macos/Sources/PlatformBridge/CapabilityProbe.swift`
- Create: `platform/macos/Sources/PlatformBridge/PowerMonitor.swift`
- Create: `platform/macos/Sources/PlatformBridge/main.swift`
- Create: `platform/macos/Tests/PlatformBridgeTests/FrameCodecTests.swift`
- Create: `platform/macos/Tests/PlatformBridgeTests/HandshakeTests.swift`
- Create: `platform/macos/Tests/PlatformBridgeTests/CapabilityProbeTests.swift`
- Modify: `platform/macos/Package.swift`

**Interfaces:**
- Consumes: `--socket` absolute path, the shared Keychain secret, protocol version `1`, and canonical Bridge JSON types.
- Produces: a resident `PCAPlatformBridge` executable serving one authenticated Rust client at a time.

- [ ] **Step 1: Write failing Swift tests**

Tests assert fragmented/oversized frame behavior, correct HMAC fixture response, invalid proof rejection, unknown protocol rejection, TCC status mapping to `not_determined|granted|denied|restricted|unavailable`, and synthetic sleep/wake event mapping.

- [ ] **Step 2: Run and verify RED**

Run: `swift test --package-path platform/macos --filter PlatformBridgeTests`

Expected: fail because the target and source types are absent. Full Xcode must now execute the tests rather than only building them; verify test counts in output.

- [ ] **Step 3: Implement a strict-concurrency Bridge server**

Use an actor to own the listener and connection. Reject socket paths outside the approved `Run/` root, unlink only an existing socket owned by the current user, set socket mode `0600`, and remove it on graceful shutdown. Decode only length-prefixed JSON and close immediately on failed authentication.

`CapabilityProbe` reads status only; it never triggers a system permission prompt. `PowerMonitor` emits canonical Bridge events for sleep and wake.

- [ ] **Step 4: Run Swift gates with mutation proof**

```bash
swift build --package-path platform/macos
swift test --package-path platform/macos
swift run --package-path platform/macos BridgeContractVerifier
```

Temporarily mutate one handshake expectation, prove the focused test exits non-zero, restore it, then rerun all three commands to exit 0.

- [ ] **Step 5: Commit**

```bash
git add platform/macos
git commit -m "feat: add resident Swift PlatformBridge"
```

---

### Task 8: Compose `agentd` and Prove Lifecycle Durability

**Files:**
- Modify: `agent/core/Cargo.toml`
- Replace: `agent/core/src/main.rs`
- Create: `agent/core/src/app.rs`
- Create: `agent/core/src/config.rs`
- Create: `agent/core/src/lifecycle.rs`
- Create: `agent/core/tests/process_lifecycle.rs`
- Create: `agent/core/tests/event_outbox_kill.rs`

**Interfaces:**
- Consumes: Runtime foundation, DbActor, Keychain store, Bridge supervisor, and installed bundle-relative executable paths.
- Produces: the real `pca-agentd` binary with `run`, `health`, `prepare-sleep`, and graceful shutdown behavior.

- [ ] **Step 1: Write failing process tests**

Tests launch `pca-agentd` under a temporary root passed through the explicit test-only `--runtime-root` CLI argument. Assert: health file within five seconds, second instance rejection before database open, lifecycle Event + Outbox pair, Bridge-degraded status with a missing fake binary, and graceful SIGTERM checkpoint.

The kill test injects a barrier between Event and Outbox statements inside a test transaction, kills the process, reopens SQLite, and asserts counts are `(0, 0)` or `(1, 1)`, never `(1, 0)`.

- [ ] **Step 2: Run and verify RED**

Run: `cargo test -p pca-agentd --test process_lifecycle --test event_outbox_kill`

Expected: current scaffold does not accept the runtime arguments or write health state.

- [ ] **Step 3: Implement startup composition**

Startup order is fixed: paths/permissions → lock → tracing/crash marker → DbActor/migrations/health → Keychain → Bridge supervisor → lifecycle Event Bus → heartbeat. State becomes locally healthy `unpaired` when the database is healthy even if Bridge status is degraded.

Use `tokio::signal` for termination. On Bridge sleep event, stop accepting lifecycle side effects, drain the bounded queue, checkpoint WAL, record `SYSTEM_SLEEP`, and acknowledge. On wake, record `SYSTEM_WAKE` and refresh capability status.

- [ ] **Step 4: Run Agent and workspace gates**

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
python3 scripts/verify_boundaries.py .
```

- [ ] **Step 5: Commit**

```bash
git add agent/core Cargo.lock
git commit -m "feat: compose durable S1A agent runtime"
```

---

### Task 9: Build the Self-Installing SwiftUI App and DMG

**Files:**
- Create: `platform/macos/PersonalComputerAgent.xcodeproj/project.pbxproj`
- Create: `platform/macos/PersonalComputerAgent/PersonalComputerAgentApp.swift`
- Create: `platform/macos/PersonalComputerAgent/InstallerView.swift`
- Create: `platform/macos/PersonalComputerAgent/InstallerViewModel.swift`
- Create: `platform/macos/PersonalComputerAgent/InstallCoordinator.swift`
- Create: `platform/macos/PersonalComputerAgent/BundleValidator.swift`
- Create: `platform/macos/PersonalComputerAgent/ServiceController.swift`
- Create: `platform/macos/PersonalComputerAgent/UninstallCommand.swift`
- Create: `platform/macos/PersonalComputerAgent/Info.plist`
- Create: `platform/macos/PersonalComputerAgent/PersonalComputerAgent.entitlements`
- Create: `platform/macos/PersonalComputerAgent/Resources/com.pca.agentd.plist`
- Create: `platform/macos/PersonalComputerAgentTests/InstallCoordinatorTests.swift`
- Create: `scripts/build-s1a-dmg.sh`
- Create: `scripts/verify-s1a-bundle.sh`
- Create: `scripts/tests/test_s1a_packaging.py`
- Modify: `.gitignore`
- Modify: `justfile`

**Interfaces:**
- Consumes: release `pca-agentd`, release `PCAPlatformBridge`, Personal Team signing identity, and the exact App/Data/Run paths.
- Produces: `dist/PersonalComputerAgent-S1A-arm64.dmg` containing the single self-installing app.

- [ ] **Step 1: Create the Xcode project with exact settings**

Create one macOS App target `PersonalComputerAgent` and one unit-test target. Set:

```text
MACOSX_DEPLOYMENT_TARGET = 13.0
SWIFT_VERSION = 6.0
ARCHS = arm64
ONLY_ACTIVE_ARCH = NO
ENABLE_HARDENED_RUNTIME = YES
INFOPLIST_FILE = PersonalComputerAgent/Info.plist
CODE_SIGN_ENTITLEMENTS = PersonalComputerAgent/PersonalComputerAgent.entitlements
LSUIElement = true
```

The project references the local `BridgeProtocol` package and copies the two prebuilt binaries plus LaunchAgent plist into their approved Bundle paths. Do not commit `DEVELOPMENT_TEAM`; pass it during local build.

- [ ] **Step 2: Write failing installer and packaging tests**

Swift tests use an isolated root and fake `ServiceController` to assert first install, relaunch-required result, repeat install, downgrade rejection, old-bundle rollback after failed health, and path traversal rejection.

Python tests create a synthetic app and assert `verify-s1a-bundle.sh` rejects: missing Bridge, non-arm64 binary, missing plist, a writable `Data` path inside the bundle, and failed signature verification.

- [ ] **Step 3: Run and verify RED**

```bash
xcodebuild test \
  -project platform/macos/PersonalComputerAgent.xcodeproj \
  -scheme PersonalComputerAgent \
  -destination 'platform=macOS,arch=arm64'
python3 -m unittest scripts.tests.test_s1a_packaging -v
```

Expected: fail because install coordination and packaging scripts are absent/incomplete.

- [ ] **Step 4: Implement safe self-install and service registration**

`InstallCoordinator` accepts injected filesystem/service/health dependencies. It stages to `App/.staging-<UUID>`, validates before stopping the old runtime, moves the old app to `App/.rollback`, atomically renames the staged app, relaunches the installed executable, registers through `SMAppService.agent`, and rolls back on bounded health failure.

The UI has only explicit states: ready, copying, validating, waitingApproval, starting, success, and failed(message, recoveryAction). If service status is `requiresApproval`, call `SMAppService.openSystemSettingsLoginItems()` and poll status with a bounded, cancellable task.

`UninstallCommand` unregisters and stops before deleting exact resolved children. Default removes App/Run only. `--delete-data` prints resolved paths and requires the literal token `DELETE PCA DATA` before deleting Data and PCA-owned Keychain items.

- [ ] **Step 5: Implement deterministic bundle and DMG scripts**

`build-s1a-dmg.sh` accepts:

```text
--team-id TEAMID
--identity "Apple Development: Name (TEAMID)"
--version 0.1.0
--output dist/PersonalComputerAgent-S1A-arm64.dmg
```

It runs Rust Release build, Swift Bridge Release build, `xcodebuild archive`, assembles nested resources, signs nested binaries inside-out, signs the app, runs strict `codesign` and `lipo -archs` checks, creates the DMG with `hdiutil`, mounts it read-only for smoke verification, and detaches it. It never invokes `xattr`, `spctl --master-disable`, `notarytool`, or reads Apple passwords.

- [ ] **Step 6: Run installer, package, and existing gates**

```bash
xcodebuild test -project platform/macos/PersonalComputerAgent.xcodeproj \
  -scheme PersonalComputerAgent -destination 'platform=macOS,arch=arm64'
python3 -m unittest scripts.tests.test_s1a_packaging -v
./scripts/build-s1a-dmg.sh --team-id "$PCA_TEAM_ID" \
  --identity "$PCA_APPLE_DEVELOPMENT_IDENTITY" --version 0.1.0 \
  --output dist/PersonalComputerAgent-S1A-arm64.dmg
./scripts/verify-s1a-bundle.sh dist/PersonalComputerAgent-S1A-arm64.dmg
./scripts/verify-full.sh
```

Expected: all exit 0. Identity variables are supplied interactively or by the local shell and are never committed.

- [ ] **Step 7: Commit**

```bash
git add platform/macos scripts/build-s1a-dmg.sh scripts/verify-s1a-bundle.sh \
  scripts/tests/test_s1a_packaging.py .gitignore justfile
git commit -m "feat: add S1A self-installing DMG"
```

---

### Task 10: Prove Real Installation, Login Recovery, and Pack Reproducibility

**Files:**
- Create: `scripts/verify-s1a-live.sh`
- Create: `docs/runbooks/S1A_SELF_USE_INSTALL.md`
- Modify: `.github/workflows/ci.yml`
- Modify: `README.md`
- Modify selectively: `/Users/jacob/Projects/PCA/repo-template/**`
- Modify: `/Users/jacob/Projects/PCA/scripts/verify-pack.sh`
- Create: `/Users/jacob/Projects/PCA/scripts/tests/test_s1a_pack_verification.py`

**Interfaces:**
- Consumes: the signed local DMG, approved background item, clean test roots, and the verified product repository.
- Produces: repeatable CI/runtime gates, a real-macOS evidence log, and a PCA template that regenerates the S1A source baseline without build artifacts or signing secrets.

- [ ] **Step 1: Add CI gates that do not require private signing material**

On macOS CI, run Rust workspace gates, Swift Package tests, Xcode `CODE_SIGNING_ALLOWED=NO` build/tests, migration/boundary gates, and packaging failure fixtures. Do not upload a DMG and do not add certificate/private-key secrets to GitHub Actions in S1A.

- [ ] **Step 2: Add the live verification script and runbook**

`verify-s1a-live.sh` accepts `--dmg <absolute-path>` and performs read-only preflight, opens the DMG, waits for the user-approved installation, then verifies:

```text
installed app path exists
SMAppService/launchctl reports the expected user job
pca-agentd and PCAPlatformBridge run as the current UID, never root
runtime-status.json becomes locally healthy within five seconds
socket and database modes are 0600
App/Data/Run separation is exact
```

The runbook includes manual Gatekeeper approval, Login Items approval, logout/login verification, Bridge kill/recovery, upgrade rollback fixture, default uninstall, and confirmed complete uninstall. It explicitly forbids globally disabling Gatekeeper.

- [ ] **Step 3: Execute real-macOS acceptance**

Run:

```bash
./scripts/verify-s1a-live.sh --dmg "$PWD/dist/PersonalComputerAgent-S1A-arm64.dmg"
./scripts/verify-full.sh
git status --short
```

Then manually log out and back in once, rerun `verify-s1a-live.sh --installed`, kill only the resolved Bridge PID, and verify status changes degraded → ready while the Rust PID and SQLite integrity remain stable.

Expected: every S1A Definition of Done item has an exit-code-backed or manual macOS evidence entry.

- [ ] **Step 4: Write the failing PCA regeneration test**

The pack test bootstraps a clean temporary repository, compares the canonical S1A Rust/Swift/contract/migration/tooling tree with the product repository while excluding `.git`, `target`, `.build`, `DerivedData`, `node_modules`, `dist`, `.superpowers`, `.db`, and logs, then runs structural verification.

Run from `/Users/jacob/Projects/PCA`:

```bash
python3 -m unittest scripts.tests.test_s1a_pack_verification -v
```

Expected before sync: fail with a tree-hash mismatch.

- [ ] **Step 5: Synchronize only verified source back to the PCA template**

Copy the verified source/config/test skeleton into `repo-template` without signing identities, Keychain material, generated Xcode user data, DMG files, databases, logs, or build output. Update `verify-pack.sh` and the manifest to name S1A as a source baseline, not a pre-signed public installer.

- [ ] **Step 6: Run clean-generation and final gates**

```bash
cd /Users/jacob/Projects/PCA
python3 -m unittest discover -s scripts/tests -v
./scripts/verify-pack.sh

cd /Users/jacob/Projects/PersonalComputerAgent
./scripts/verify-full.sh
git diff --check
git status --short
```

Expected: all commands exit 0 and the product worktree is clean except for the final evidence document about to be committed.

- [ ] **Step 7: Record exact evidence and commit**

Append commands, exit codes, test counts, tool versions, signed DMG SHA-256, install path, process UIDs, login-restart result, Bridge recovery result, and known self-use Gatekeeper limitation to this plan under `## Verification Evidence`.

```bash
git add .github scripts docs README.md docs/superpowers/plans/2026-07-31-s1a-local-runtime-installer.md
git commit -m "docs: record S1A verification evidence"
```

---

## Plan Self-Review

- Spec coverage: Tasks 1-10 cover the source-spec delta, Xcode prerequisite, contracts, App/Data/Run separation, database slice, runtime state, Keychain, authenticated IPC, resident Bridge, Agent composition, graphical installation, upgrade rollback, local uninstall, Development signing, DMG packaging, live login recovery, CI, and PCA template regeneration.
- Scope boundary: S1B Cloud, S2 Collectors, S3 complete sync/R2, Root, Developer ID, notarization, Sparkle, Intel, menu-bar UI, remote command, and remote uninstall are explicitly excluded.
- Type consistency: `RuntimeStatusEnvelope`, `BridgeStatus`, `CredentialStore`, `DbActorHandle`, `RuntimePaths`, `BridgeClient`, and `BridgeSupervisor` are defined before their consumers.
- Failure coverage: duplicate process, lock, corrupt/locked DB, migration/version failure, partial transaction kill, missing/crashed/incompatible Bridge, invalid HMAC, denied background approval, invalid bundle, downgrade, failed upgrade health, and destructive uninstall confirmation all have concrete tests.
- Placeholder scan: no TBD, TODO, “implement later,” undefined similar-task reference, or unowned mandatory decision remains.
