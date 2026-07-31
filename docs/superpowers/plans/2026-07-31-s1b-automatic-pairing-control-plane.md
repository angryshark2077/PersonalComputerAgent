# S1B Automatic Pairing and Control Plane Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Pair a self-use macOS installation through a Setup-only localhost callback, keep device credentials in Keychain, and let the authenticated Owner Workspace audit and deliver Network and WeChat-outbound Collector configuration without S1B business Event sync.

**Architecture:** Cloud API owns pairing-session issuance, authorization-code exchange, device credential rotation/revocation, control revisions, and presence. Swift Setup/Repair owns the one-time localhost listener, browser launch, and Keychain write; Rust Agent Core reads those credentials and runs a bounded 30-second heartbeat/control loop. Cloud configuration is the only remote input and is applied by Agent Core, never by a Collector or PlatformBridge.

**Tech Stack:** Rust 1.82/Tokio/Serde, Swift 6/SwiftUI/Foundation/Security, Hono/Zod/Better Auth, PostgreSQL/Drizzle, Next.js/React, JSON Schema Draft 2020-12, pnpm, SQLite WAL, macOS Keychain.

## Global Constraints

- Approved designs: `docs/superpowers/specs/2026-07-31-s1b-network-control-plane-design.md`, `docs/superpowers/specs/2026-07-31-wechat-outbound-message-collector-design.md`, ADR-0006, and ADR-0007.
- Setup/Repair is the only browser launcher and the only local HTTP listener; `agentd` never opens a browser or binds loopback HTTP.
- The callback binds only `127.0.0.1`, accepts one callback for at most five minutes, validates exact path/state/PKCE, and carries no long-lived credential in the URL.
- Access/refresh credentials, device key material, Bridge secret, and WeChat KeyMaterial stay in Keychain/Cloud secret storage only. They must never reach SQLite payloads, Event payloads, diagnostics, tests, or ordinary logs.
- S1B adds no business Event sync, Collector source, attachment/R2 transfer, remote command, Network location inference, or WeChat database access.
- Production remains unpaired until a valid S1B credential exists. Existing debug-only S2 test identity remains test-only and cannot become a release input.
- Owner Cloud authorization is limited to `network` and `communication.wechat` configuration on the paired device's own Workspace. Every change needs an immutable actor/time/old/new/revision audit row; it cannot bypass macOS TCC.
- Private-account bootstrap is fixed for S1B: Better Auth email/password registration creates the sole Owner Workspace for the first account. S1B exposes no public Workspace creation, invitation, role-management, or multi-tenant membership UI.
- Cloud control polls every 30 seconds while healthy. Transient retry backs off with jitter and caps at five minutes. No request queue or retry loop is unbounded.
- Raw Network and WeChat collection are not enabled by S1B itself; it only carries desired revisions. Later slices enforce 30-day Network raw retention and 90-day WeChat body/display-name retention.
- Use only fixed, audited dependencies: Better Auth, Drizzle ORM, `pg`, `@hono/zod-validator`, Next.js, React, and Rust `reqwest` with rustls. Record license/version/lockfile impact in the dependency review. Do not modify `.env` or commit secrets.
- Every migration is immutable; use a new local migration after `0002_s2_collector_state.sql` and a new Cloud migration after `0000_baseline.sql`.

---

## File Structure

```text
packages/contracts/
├── device-pairing.schema.json                 # Request/response DTOs without real secrets
├── agent-control-snapshot.schema.json         # Revisioned config returned to device
├── fixtures/pairing-start.valid.json
├── fixtures/agent-control-snapshot.valid.json
├── src/types.ts
├── src/validate.ts
└── tests/contracts.test.ts

packages/db-cloud/
├── migrations/0001_s1b_control_plane.sql      # Device/control tables only
└── src/{schema.ts,repository.ts}

apps/cloud-api/src/
├── index.ts                                   # Route registration only
├── auth.ts                                    # Better Auth session → Owner principal port
├── pairing.ts                                 # Session/code/exchange use cases
├── control.ts                                 # Device token, heartbeat, config use cases
├── repository.ts                              # Cloud repository port + Drizzle adapter
└── test/*.test.ts

apps/web-dashboard/src/app/
├── layout.tsx
├── page.tsx
└── devices/[deviceId]/page.tsx                # Pair authorization + collector toggles/audit

crates/keychain/src/{lib.rs,macos.rs,macos_tests.rs}
crates/keychain/tests/device_credentials.rs
agent/core/src/{app.rs,cloud_control.rs,config.rs}
agent/core/tests/cloud_control_process.rs
crates/db-local/migrations/0003_s1b_pairing_state.sql
crates/db-local/src/{actor.rs,lib.rs,repository.rs}
crates/db-local/tests/pairing_state.rs

platform/macos/PersonalComputerAgent/
├── PairingCoordinator.swift
├── PairingCallbackServer.swift
├── PairingModels.swift
├── InstallerViewModel.swift
└── PersonalComputerAgentApp.swift
platform/macos/PersonalComputerAgentTests/PairingCoordinatorTests.swift
```

`pairing.ts` and `control.ts` contain use cases; Hono handlers only validate
and map HTTP. `cloud_control.rs` owns Agent polling and is kept separate from
the existing local heartbeat/lifecycle code. No new generic plugin, command,
or remote-control abstraction is introduced.

---

### Task 1: Freeze pairing and control contracts before adding a route

**Files:**
- Create: `contracts/device-pairing.schema.json`
- Create: `contracts/agent-control-snapshot.schema.json`
- Create: matching copies and valid/invalid fixtures under `packages/contracts/`
- Modify: `packages/contracts/src/types.ts`, `packages/contracts/src/validate.ts`, `packages/contracts/tests/contracts.test.ts`, `contracts/README.md`

**Interfaces:**
- Produces `DevicePairingStart`, `DevicePairingExchange`, and `AgentControlSnapshot` for Swift, Rust, and Hono.
- `AgentControlSnapshot` has `device_id`, `workspace_id`, `revoked`, `configuration_revision`, and exactly the two configured Collector keys `network` and `communication.wechat`.

- [ ] **Step 1: Add failing JSON contract tests**

```ts
test("pairing and control contracts reject unknown collector scope", () => {
  assert.equal(validateContract("device-pairing", fixture("pairing-start.valid.json")).valid, true);
  const invalid = fixture("agent-control-snapshot.valid.json") as {
    collectors: Record<string, unknown>;
  };
  invalid.collectors["screen"] = { enabled: true };
  assert.equal(validateContract("agent-control-snapshot", invalid).valid, false);
});
```

- [ ] **Step 2: Run the focused test and prove it is RED**

```bash
pnpm --filter @pca/contracts test
```

Expected: TypeScript rejects the missing `ContractSchemaName` members.

- [ ] **Step 3: Implement the strict schemas and DTOs**

Use these wire shapes; fixture tokens are synthetic fixed strings and never
look like a production credential:

```ts
export interface DevicePairingStart {
  device_public_key: string;
  code_challenge: string;
  callback_uri: string;
}

export interface AgentControlSnapshot {
  device_id: string;
  workspace_id: string;
  revoked: boolean;
  configuration_revision: number;
  collectors: {
    network: { enabled: boolean };
    "communication.wechat": { enabled: boolean; direction: "outgoing"; message_type: "text"; sync_mode: "full" };
  };
}
```

`callback_uri` must be exactly `http://127.0.0.1:<1-65535>/pca/pair/callback`.
Both schemas use `additionalProperties: false`; the snapshot rejects negative
revisions and a `communication.wechat` scope other than the approved fixed
values.

- [ ] **Step 4: Run the contract gates and commit**

```bash
pnpm --filter @pca/contracts typecheck
pnpm --filter @pca/contracts test
git add contracts packages/contracts
git commit -m "feat: add pairing and control contracts"
```

Expected: valid fixtures pass; invalid localhost, unknown scope, negative
revision, and broadened WeChat scope fail.

### Task 2: Add immutable Cloud and Local pairing/control state

**Files:**
- Create: `packages/db-cloud/migrations/0001_s1b_control_plane.sql`
- Create: `packages/db-cloud/src/schema.ts`, `packages/db-cloud/src/repository.ts`
- Modify: `packages/db-cloud/package.json`, root lockfile
- Create: `crates/db-local/migrations/0003_s1b_pairing_state.sql`
- Modify: `crates/db-local/src/{actor.rs,lib.rs,repository.rs}`
- Create: `crates/db-local/tests/pairing_state.rs`

**Interfaces:**
- Produces Cloud `ControlRepository` methods `create_pairing_session`, `consume_authorization_code`, `rotate_device_credentials`, `load_control_snapshot`, `record_heartbeat`, and `append_config_audit`.
- Produces local `DbActorHandle::{load_pairing_state,save_pairing_state,save_control_revision,clear_pairing_state}`. Local state contains only device/workspace IDs, `credential_ref`, credential generation, and applied revision.

- [ ] **Step 1: Write migration replay and non-secret-state tests**

```rust
#[tokio::test]
async fn pairing_state_never_persists_token_material() {
    let db = test_database().await;
    db.save_pairing_state(&PairingState::paired(
        device_id(), workspace_id(), "keychain://pca/device/current", 7,
    )).await.unwrap();
    assert_eq!(db.load_pairing_state().await.unwrap().credential_ref, "keychain://pca/device/current");
    assert!(!database_text(&db).await.contains("refresh_token"));
}
```

- [ ] **Step 2: Run the local test and prove it is RED**

```bash
cargo test -p pca-db-local --test pairing_state
```

Expected: missing migration, DTO, and DbActor methods.

- [ ] **Step 3: Write the two immutable migrations**

Cloud migration creates the Better Auth user/session/account tables plus one
`workspaces` table and `workspace_members` table required to scope the first
Owner. It also creates only the S1B domain tables `devices`, `device_credential_generations`,
`pairing_sessions`, `pairing_authorization_codes`, `collector_configs`,
`collector_config_audit`, and `device_heartbeats`. Store token/code hashes,
expiry, consumed/revoked timestamps, and Workspace foreign keys; never store
plain credentials. Add indexes for active session expiry, device Workspace
lookup, audit chronology, and last heartbeat.

Local migration creates singleton `pairing_state` with UUID device/workspace
columns, `credential_ref`, non-negative generation/revision, `paired_at_ms`,
and no token/body column. It must remain empty until the Agent validates a
Keychain credential.

- [ ] **Step 4: Implement repositories behind testable ports**

```ts
export interface ControlRepository {
  createPairingSession(input: PairingSessionInput): Promise<PairingSession>;
  consumeAuthorizationCode(input: CodeExchangeInput): Promise<DeviceCredentialGrant>;
  controlSnapshot(deviceId: string, workspaceId: string): Promise<AgentControlSnapshot>;
}
```

Add a `typecheck` script (`tsc --noEmit`) to `@pca/db-cloud`. Use
Drizzle/PostgreSQL only in the adapter; route tests use an in-memory port
with the same uniqueness, expiry, Workspace, and consumed-code semantics.
Add `drizzle-orm`, `pg`, and `drizzle-kit` only after recording their
MIT/Apache-2.0 license and lockfile diff in the commit message body.

- [ ] **Step 5: Verify migration and repository boundaries, then commit**

```bash
python3 scripts/verify_migrations.py .
cargo test -p pca-db-local --test pairing_state
cargo test -p pca-db-local
pnpm --filter @pca/db-cloud typecheck
git add packages/db-cloud crates/db-local scripts pnpm-lock.yaml
git commit -m "feat: persist pairing and control state"
```

Expected: fresh and upgrade local databases replay through `0003`; SQL source
contains hashes/references only, not credential plaintext.

### Task 3: Implement Cloud pairing, credential rotation, and control endpoints

**Files:**
- Create: `apps/cloud-api/src/{auth.ts,pairing.ts,control.ts,repository.ts}`
- Modify: `apps/cloud-api/src/index.ts`, `apps/cloud-api/package.json`, root lockfile
- Create: `apps/cloud-api/src/test/{pairing.test.ts,control.test.ts}`

**Interfaces:**
- `POST /v1/device-pairing/sessions` accepts `DevicePairingStart` and returns a five-minute opaque session ID.
- Authenticated `POST /v1/device-pairing/sessions/:id/authorize` binds the selected/default Workspace and returns one redirect URI containing code and state only.
- `POST /v1/device-pairing/exchange`, `POST /v1/devices/token/refresh`, and `POST /v1/agent/control` use device credentials; control returns `AgentControlSnapshot` plus server time.

- [ ] **Step 1: Add Hono request tests against an in-memory repository**

```ts
test("a pairing code is single use and PKCE bound", async () => {
  const app = createApp({ repository: new MemoryControlRepository(), owner: owner("ws-a") });
  const session = await app.request("/v1/device-pairing/sessions", {
    method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(start),
  });
  const { session_id } = await session.json() as { session_id: string };
  const authorized = await app.request(`/v1/device-pairing/sessions/${session_id}/authorize`, { method: "POST" });
  assert.equal(authorized.status, 302);
  assert.equal((await app.request("/v1/device-pairing/exchange", exchangeRequest)).status, 200);
  assert.equal((await app.request("/v1/device-pairing/exchange", exchangeRequest)).status, 409);
});
```

- [ ] **Step 2: Run the API tests and prove they are RED**

```bash
pnpm --filter @pca/cloud-api test
```

Expected: the package has no test script or route factory yet.

- [ ] **Step 3: Implement security-critical use cases**

Generate 32-byte opaque authorization codes, access credentials, and refresh
credentials with the runtime CSPRNG; persist only SHA-256 hashes. Bind an
authorization code to session ID, code challenge, device public-key hash,
Workspace, expiry, and single-use consumed timestamp. Require an authenticated
Better Auth Owner principal for authorize/config/revoke routes. Require device
Bearer credentials for refresh/control and rotate refresh credentials on every
successful refresh. Return the standard error envelope for `PAIRING_EXPIRED`,
`PAIRING_REPLAYED`, `PKCE_INVALID`, `DEVICE_REVOKED`, and `WORKSPACE_FORBIDDEN`.

The control endpoint records heartbeat timestamps/outbox summary only and
returns complete monotonic configuration revisions. It must never accept a
client-supplied public-IP header or a business Event in S1B.

- [ ] **Step 4: Add audited Owner configuration and revoke routes**

```ts
app.put("/v1/devices/:deviceId/collector-config", requireOwner, async (c) => {
  const revision = await control.setCollectorConfig({
    actorId: c.get("principal").userId,
    workspaceId: c.get("principal").workspaceId,
    deviceId: c.req.param("deviceId"),
    config: await c.req.json(),
  });
  return c.json({ configuration_revision: revision });
});
```

Validate that the only accepted keys are the two contract scopes, and that
WeChat accepts only the fixed outgoing/text/full values. Revoke invalidates
all credential generations atomically and leaves an audit row.

- [ ] **Step 5: Run API/security tests and commit**

```bash
pnpm --filter @pca/cloud-api typecheck
pnpm --filter @pca/cloud-api test
git add apps/cloud-api packages/contracts packages/db-cloud pnpm-lock.yaml
git commit -m "feat: add device pairing control API"
```

Expected: cross-Workspace access is 403; expired/replayed codes fail; URL/log
fixtures contain no access or refresh credential; a revoked device is rejected.

### Task 4: Build the minimum authenticated Dashboard pairing/configuration UI

**Files:**
- Modify: `apps/web-dashboard/package.json`, root lockfile
- Create: `apps/web-dashboard/src/app/{layout.tsx,page.tsx,providers.tsx}`
- Create: `apps/web-dashboard/src/app/pair/page.tsx`
- Create: `apps/web-dashboard/src/app/devices/[deviceId]/page.tsx`
- Create: `apps/web-dashboard/src/lib/{api.ts,auth.ts}`
- Create: `apps/web-dashboard/test/{pair-page.test.ts,device-config.test.ts}`

**Interfaces:**
- Consumes browser session from Better Auth and Cloud API user endpoints.
- Produces a one-time pairing authorization screen, default/selected Workspace binding, a device page with Network and WeChat-outbound toggles, revision, audit rows, and revoke action.

- [ ] **Step 1: Add failing component/request tests**

```ts
test("device configuration exposes no expanded WeChat scope", async () => {
  const page = await renderDevicePage(snapshotWithWechatEnabled());
  assert.ok(page.getByText("Outgoing text only"));
  assert.equal(page.queryByText("Incoming messages"), null);
});
```

- [ ] **Step 2: Run the Web test and prove it is RED**

```bash
pnpm --filter @pca/web-dashboard test
```

Expected: the current scaffold has neither Next.js nor the page/test modules.

- [ ] **Step 3: Add the narrow Next.js application**

Install only `next`, `react`, `react-dom`, and their required type packages;
record license/version impact. `/pair` requires a Better Auth session, shows a
Workspace selector only when more than one workspace is available, and posts
the selected session to the Cloud API. It must redirect only to the callback
URI returned by the pairing session and must not render a credential.

The device page has two explicit scope cards:

```tsx
<CollectorScopeCard name="Network" detail="SSID, BSSID and local IP" />
<CollectorScopeCard name="WeChat outbound text" detail="Outgoing text only; 90-day retention" />
```

Each mutation waits for the returned revision, refreshes the audit list, and
renders errors from the standard API envelope. The page does not import
Drizzle, a Provider SDK, or any local-system capability.

- [ ] **Step 4: Run Web gates and commit**

```bash
pnpm --filter @pca/web-dashboard typecheck
pnpm --filter @pca/web-dashboard test
git add apps/web-dashboard pnpm-lock.yaml
git commit -m "feat: add pairing and collector control dashboard"
```

Expected: unauthorized routes redirect to sign-in; one Workspace auto-binds;
multi-Workspace selection is explicit; changes display actor/time/revision.

### Task 5: Extend Keychain and implement the Setup-only pairing callback

**Files:**
- Modify: `crates/keychain/src/{lib.rs,macos.rs,macos_tests.rs}`, `crates/keychain/tests/credential_store.rs`
- Create: `crates/keychain/tests/device_credentials.rs`
- Create: `platform/macos/PersonalComputerAgent/{PairingModels.swift,PairingCallbackServer.swift,PairingCoordinator.swift}`
- Modify: `platform/macos/PersonalComputerAgent/{InstallerViewModel.swift,PersonalComputerAgentApp.swift}`
- Create: `platform/macos/PersonalComputerAgentTests/PairingCoordinatorTests.swift`

**Interfaces:**
- Produces a versioned Keychain record `com.pca.device/current-v1` containing device/workspace IDs, credential generation, expiry metadata, access credential, and refresh credential.
- Produces Swift `PairingCoordinator.startIfUnpaired() async -> PairingResult` and `PairingCoordinator.repair() async -> PairingResult`.

- [ ] **Step 1: Write Keychain and callback failure tests**

```swift
@Test func callbackRejectsWrongStateAndStopsListener() async throws {
    let coordinator = PairingCoordinator.fake(state: "expected")
    await #expect(throws: PairingError.stateMismatch) {
        try await coordinator.accept(URL(string: "http://127.0.0.1/pca/pair/callback?code=x&state=wrong")!)
    }
    #expect(await coordinator.listenerIsClosed)
}
```

```rust
#[test]
fn device_credentials_reject_bridge_identity_and_missing_refresh_secret() {
    assert!(DeviceCredential::decode(b"not-json").is_err());
    assert!(DeviceCredential::new(device_id(), workspace_id(), "", "refresh").is_err());
}
```

- [ ] **Step 2: Run focused Rust and Swift tests and prove they are RED**

```bash
cargo test -p pca-keychain --test device_credentials
swift test --package-path platform/macos --filter PairingCoordinatorTests
```

Expected: missing credential type and pairing coordinator modules.

- [ ] **Step 3: Implement a distinct device credential identity**

Keep the existing Bridge fixed-length credential unchanged. Add separate
validated helpers for the device record; restrict access to the installed
Setup App and installed `agentd` paths, reject zero-length/invalid UTF-8
fields, and map Keychain errors without including record content. Never place
the device credential in `pairing_state` or an Event.

- [ ] **Step 4: Implement the one-time listener and exchange**

`PairingCallbackServer` must bind `NWListener`/`Network` only to IPv4
loopback, generate an unpredictable port and state, accept only
`GET /pca/pair/callback?code=...&state=...`, return a static success page, and
close on the first terminal result. `PairingCoordinator` creates an Ed25519
device key pair and PKCE verifier, POSTs the session request, calls
`NSWorkspace.shared.open`, validates the callback, exchanges the code over
`URLSession`, writes the Keychain record, and asks the installed Agent to
restart. Log only stage/error code/request ID.

- [ ] **Step 5: Wire pairing into the existing Setup UI and verify**

Add a `pairing` state after local installation health is reached. On an
already-paired device Setup skips browser launch. On cancellation/expiry it
returns to a retry-only repair state and leaves any prior valid Keychain record
untouched.

```bash
swift test --package-path platform/macos
cargo test -p pca-keychain
swift build --package-path platform/macos
git add platform/macos crates/keychain
git commit -m "feat: pair devices through setup callback"
```

Expected: callbacks from a non-loopback host, wrong path/state, expired or
second callback fail; test credential records never appear in stdout.

### Task 6: Add bounded Agent credential loading and Cloud-control runtime

**Files:**
- Create: `agent/core/src/cloud_control.rs`
- Modify: `agent/core/src/{app.rs,config.rs,main.rs}` and `agent/core/Cargo.toml`
- Modify: `crates/db-local/src/{actor.rs,repository.rs}`
- Create: `agent/core/tests/cloud_control_process.rs`

**Interfaces:**
- Produces `CloudControlRuntime::start(db, credentials, client) -> CloudControlHandle` and `CloudControlHandle::shutdown().await`.
- `ControlClient` exposes `refresh`, `heartbeat_and_control`, and typed errors (`Transient`, `Revoked`, `InvalidCredential`, `Contract`).

- [ ] **Step 1: Add failing deterministic polling tests**

```rust
#[tokio::test(start_paused = true)]
async fn revocation_clears_pairing_and_disables_sensitive_collectors() {
    let client = FakeControlClient::revoked();
    let runtime = CloudControlRuntime::start(db(), credentials(), Arc::new(client)).await.unwrap();
    tokio::time::advance(Duration::from_secs(30)).await;
    assert!(load_pairing_state().await.is_none());
    assert!(runtime.is_unpaired().await);
    assert_eq!(runtime.applied_revision().await, None);
    runtime.shutdown().await.unwrap();
}
```

- [ ] **Step 2: Run the test and prove it is RED**

```bash
cargo test -p pca-agentd --test cloud_control_process
```

Expected: missing runtime/client and no production credential load path.

- [ ] **Step 3: Implement the smallest control client and state transitions**

Add `reqwest` with `default-features = false` and rustls TLS only; record its
license and Rust-1.82 compatibility. Read credentials from Keychain at startup.
If absent/corrupt, retain `unpaired`; if valid, persist only the reference and
non-secret IDs/revision, set Agent state to `running` or `degraded` according
to local capabilities, and run an immediate control request followed by
30-second skipped ticks. A transient failure uses bounded jittered backoff;
it does not stop local runtime. A confirmed revoked/invalid response deletes
the Keychain device record, clears local pairing state, moves the Agent to
`unpaired`, and disables sensitive Collector configurations.

- [ ] **Step 4: Apply only complete newer snapshots**

```rust
fn apply_snapshot(current: u64, snapshot: AgentControlSnapshot) -> Result<Option<AppliedControl>, ControlError> {
    if snapshot.configuration_revision <= current { return Ok(None); }
    snapshot.validate_exact_scopes()?;
    Ok(Some(AppliedControl::from(snapshot)))
}
```

Persist the applied revision atomically before notifying future Network or
WeChat runtimes. Do not start their source code in S1B; absent implementations
remain disabled/unavailable while the revision is durable.

- [ ] **Step 5: Run Agent regression gates and commit**

```bash
cargo fmt --all --check
cargo clippy -p pca-agentd --all-targets -- -D warnings
cargo test -p pca-agentd --test cloud_control_process
cargo test -p pca-agentd --features process-test-hooks --test process_lifecycle --test system_collector_process
git add agent/core crates/db-local Cargo.toml Cargo.lock
git commit -m "feat: run authenticated cloud control loop"
```

Expected: release parses no test identity flags; no credential reaches SQLite;
Cloud outage leaves local lifecycle and System Collector durability intact.

### Task 7: Verify the complete S1B slice and update authoritative docs

**Files:**
- Modify: `tasks/S1B_CLOUD_CONTROL_PLANE.md`, `tasks/BACKLOG_V0.md`, `ARCHITECTURE.md`, `SECURITY.md`, `PERFORMANCE.md`
- Create: `docs/data/s1b-control-plane.md`, `docs/runbooks/S1B_PAIRING_REPAIR.md`
- Modify: `scripts/verify_migrations.py`, `scripts/verify_boundaries.py`, `scripts/verify-full.sh` only when a new executable gate is required

**Interfaces:**
- Produces a documented pairing repair/revocation procedure and data dictionary for all Cloud/Local fields, indexes, retention, and secrets boundary.

- [ ] **Step 1: Add a process-level pairing acceptance harness**

```text
fresh Keychain + unpaired Agent
→ Setup receives one valid loopback callback
→ Keychain receives credential bundle
→ Agent heartbeat/control returns revision 1
→ Dashboard audit shows actor/device/revision
→ revoke
→ next Agent control request clears pairing and disables sensitive configuration
```

The harness uses fake Cloud endpoints, synthetic credentials, temporary
Keychain identity/test doubles, and a temporary SQLite root. It must assert no
credential or message body appears in stdout, JSON status, SQLite text, or
fixture files.

- [ ] **Step 2: Add migration, boundary, and failure assertions**

```bash
python3 scripts/verify_migrations.py .
python3 scripts/verify_boundaries.py .
rg -n "device_access_token|refresh_secret|authorization_code" agent crates apps packages platform docs/data docs/runbooks
```

Expected: only field names in schema/documentation and redaction tests appear;
no source logs or persisted SQL columns contain plaintext secret values.

- [ ] **Step 3: Run the full repository gate**

```bash
./scripts/verify-full.sh
```

Expected: exit 0, including Rust/Swift/TypeScript, contracts, migrations, and
dependency-boundary checks. If a real PostgreSQL/Better Auth deployment is not
configured, report that external deployment separately; do not claim a live
Cloud pairing was verified from an in-memory test double.

- [ ] **Step 4: Commit the verified S1B slice**

```bash
git add tasks ARCHITECTURE.md SECURITY.md PERFORMANCE.md docs/data docs/runbooks scripts
git commit -m "docs: verify S1B pairing control plane"
```

---

## Plan self-review

- The plan covers automatic loopback pairing, PKCE/state/replay handling,
  Keychain-only credentials, Cloud device/config/audit/presence, Dashboard
  authorization, Agent polling/revocation, local non-secret state, migration,
  boundaries, and full-gate evidence.
- It deliberately does not implement Network sampling, WeChat source reading,
  Event upload, geo enrichment, attachments, remote commands, Telegram,
  ChatGPT, or a Keylogger.
- After S1B acceptance, create separate plans for the S2B WeChat outbound
  Provider local slice and S3 synchronization/projection slice; do not merge
  their business Event code into this plan.
