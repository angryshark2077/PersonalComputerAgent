# S1B Deployment Acceptance Correction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove the final two S1B/Railway release blockers by making Dashboard proxy readiness build-bound and by proving one shared callback-to-revocation acceptance flow.

**Architecture:** Production Next.js builds require a validated Railway-private API origin and bake its rewrites into the route manifest; readiness therefore reflects a successfully configured build. A test-only local Hono Cloud service owns every state transition used by Setup, Agent, and Dashboard clients, so the acceptance test exercises real shared control/revocation state rather than disconnected fakes.

**Tech Stack:** Next.js 16, TypeScript, Hono, Node test runner, Rust 1.82/Tokio/SQLite, existing Agent Core test hooks, existing S1B full-gate scripts.

## Global Constraints

- No Railway/GitHub deployment, secret, domain, or variable write belongs to this correction.
- Dashboard browser requests remain same-origin relative paths; no CORS or public browser API origin is introduced.
- Production Dashboard builds require a root `http://` `.railway.internal` private API origin with no path/query/fragment; local/test builds pass an explicit valid test origin.
- The process harness uses only synthetic credentials/message canaries, temporary SQLite, file-backed test Keychain, and a local test Cloud service. It must not emit any canary to stdout, stderr, JSON status, SQLite/WAL/SHM, or fixture source.
- The one acceptance flow must use one shared pairing/device/config/revocation state. It must not inject final credentials directly to the Agent, use a hard-coded authorization code, use a static control snapshot, or use an always-revoked client for its verification.
- No Collector source, business Event sync, production local IPC redesign, or remote command is added.

---

### Task 1: Bind Dashboard proxy readiness to the production build

**Files:**
- Modify: `apps/web-dashboard/next.config.ts`
- Modify: `apps/web-dashboard/src/lib/railway-environment.ts`
- Modify: `apps/web-dashboard/src/app/healthz/route.ts`
- Modify: `apps/web-dashboard/test/railway-proxy.test.ts`
- Modify: `apps/web-dashboard/package.json`

**Interfaces:**
- Produces `createNextConfig(internalOrigin: string): NextConfig` only after
  validation of a Railway private origin.
- Produces `requireBuildProxyOrigin(environment: NodeJS.ProcessEnv): string`.
- `GET /healthz` returns `{status:"ok"}` only from a successfully configured
  build; it has no runtime re-read/fallback of `CLOUD_API_INTERNAL_ORIGIN`.

- [ ] **Step 1: Add failing build-bound proxy tests**

```ts
test("production build rejects missing private API origin", () => {
  assert.throws(() => requireBuildProxyOrigin({ NODE_ENV: "production" }), /CLOUD_API_INTERNAL_ORIGIN/);
});

test("Dashboard health cannot become ready from a runtime-only origin", async () => {
  const build = createNextConfigForBuild(validPrivateOrigin);
  assert.equal(build.hasProxyRewrites, true);
  assert.deepEqual(await healthForBuiltConfig(build), { status: "ok" });
});
```

Add a subprocess build assertion that `pnpm --filter @pca/web-dashboard build`
fails without `CLOUD_API_INTERNAL_ORIGIN` and succeeds with
`http://pca-cloud-api.railway.internal:8080`.

- [ ] **Step 2: Run the tests to verify the current false-health behavior**

Run:

```bash
pnpm --filter @pca/web-dashboard test -- --test-name-pattern='production build|runtime-only'
env -u CLOUD_API_INTERNAL_ORIGIN pnpm --filter @pca/web-dashboard build
```

Expected: tests/build expose that the old build can omit rewrites and later
health checks a runtime environment value.

- [ ] **Step 3: Implement build-time required origin and static readiness**

`next.config.ts` must call `requireBuildProxyOrigin(process.env)` when
`NODE_ENV === "production"`, then generate only:

```ts
{ source: "/api/auth/:path*", destination: `${origin}/api/auth/:path*` }
{ source: "/v1/:path*", destination: `${origin}/v1/:path*` }
```

The health handler must use a build-time module constant exported by
`railway-environment.ts`, not `process.env` inside the request handler.
Production missing/invalid origin terminates build; a successful production
build makes `/healthz` return `200 {status:"ok"}`. Retain explicit test/local
configuration helpers rather than a permissive production fallback.

- [ ] **Step 4: Run Dashboard verification**

Run:

```bash
CLOUD_API_INTERNAL_ORIGIN=http://pca-cloud-api.railway.internal:8080 pnpm --filter @pca/web-dashboard test
CLOUD_API_INTERNAL_ORIGIN=http://pca-cloud-api.railway.internal:8080 pnpm --filter @pca/web-dashboard typecheck
CLOUD_API_INTERNAL_ORIGIN=http://pca-cloud-api.railway.internal:8080 pnpm --filter @pca/web-dashboard build
env -u CLOUD_API_INTERNAL_ORIGIN pnpm --filter @pca/web-dashboard build
```

Expected: tests/typecheck/configured build pass; unconfigured production build
fails with the required origin error.

- [ ] **Step 5: Commit the build/readiness correction**

```bash
git add apps/web-dashboard/next.config.ts apps/web-dashboard/src/lib/railway-environment.ts \
  apps/web-dashboard/src/app/healthz/route.ts apps/web-dashboard/test/railway-proxy.test.ts \
  apps/web-dashboard/package.json
git commit -m "fix: require Dashboard private proxy at build time"
```

### Task 2: Prove the one shared S1B callback-to-revocation flow

**Files:**
- Modify: `scripts/tests/s1b_pairing_acceptance.ts`
- Modify: `agent/core/tests/support/s1b_acceptance_agent.rs`
- Modify: `agent/core/Cargo.toml`
- Create: `apps/cloud-api/src/test/support/s1b-acceptance-cloud.ts`
- Modify: `scripts/verify-full.sh`
- Modify: `scripts/tests/test_verify_full_railway_gate.sh`
- Modify: `docs/runbooks/S1B_PAIRING_REPAIR.md`
- Modify: `docs/data/S1B_RAILWAY_DEPLOYMENT_FIELDS.md`

**Interfaces:**
- Produces `createS1bAcceptanceCloud(): S1bAcceptanceCloud`, with real local
  HTTP endpoints for pairing start/authorize/exchange, heartbeat/control,
  Dashboard device/audit/config/revoke reads, and test state inspection.
- The Agent helper consumes only `{api_origin, session_id, callback_state,
  callback_code}` and performs exchange/heartbeat via the shared local service.
- The acceptance script produces no canary in observed stdout/stderr/JSON/
  SQLite/WAL/SHM/fixture source and exits non-zero for any violation.

- [ ] **Step 1: Write failing shared-flow assertions**

```ts
const cloud = await createS1bAcceptanceCloud();
const handoff = await cloud.startPairing();
await dashboardAuthorize(cloud.origin, handoff.sessionId, handoff.callbackState);
const agent = await spawnAcceptanceAgent({ apiOrigin: cloud.origin, handoff });
assert.equal(await agent.firstControlRevision(), 1);
assert.equal((await dashboardDevice(cloud.origin, agent.deviceId)).configuration_revision, 1);
await dashboardRevoke(cloud.origin, agent.deviceId);
assert.equal(await agent.nextControlResult(), "revoked");
```

Add negative assertions that fail if the harness passes credentials directly on
stdin, uses a literal authorization code, reads a static snapshot, or uses a
separate always-revoked client.

- [ ] **Step 2: Run the current harness to prove it lacks shared state**

Run: `node --test scripts/tests/s1b_pairing_acceptance.ts`

Expected: FAIL the new assertions because the current helper has independent
Cloud/Dashboard/Agent fakes.

- [ ] **Step 3: Implement one local Cloud service and wire all consumers**

The test service owns an in-memory map keyed by its generated session and
device IDs. It creates the authorization code itself during Dashboard
authorization, validates the Agent's PKCE proof during exchange, stores only
synthetic credential hashes, returns the current configuration revision from
heartbeat/control, appends Dashboard configuration audit rows, and returns a
revoked response only after its Dashboard revoke route mutates that same
device.

The Agent helper must call those HTTP endpoints through its test client. It
may receive callback metadata but no final access/refresh credential. It must
write synthetic returned credentials through the file-backed Keychain double
and use temporary SQLite. Its second control request occurs only after the
Dashboard client revokes the exact returned device ID.

Keep the canary scanner but make it scan runtime-generated values from this
one flow across process output, JSON responses/status, SQLite main/WAL/SHM,
and all fixture source paths.

- [ ] **Step 4: Run shared-flow and sensitive-boundary checks**

Run:

```bash
PATH="/Users/jacob/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" \
  cargo test -p pca-agentd --features process-test-hooks --test cloud_control_process
node --test scripts/tests/s1b_pairing_acceptance.ts
grep -R --fixed-strings -- "accept-canary-" scripts/tests agent/core/tests/support || true
```

Expected: process and shared acceptance tests pass; the grep has only intended
test-generator/source assertions and no persisted literal runtime canary.

- [ ] **Step 5: Wire the corrected process test into the full gate and document it**

Keep `scripts/verify-full.sh` ordered so structural/contract checks run before
the process acceptance script and the script runs before `FULL VERIFICATION
PASSED`. Update the runbook/data dictionary to describe this as local
synthetic acceptance, not a live Railway proof.

- [ ] **Step 6: Run full verification and commit the acceptance correction**

Run:

```bash
PATH="/Users/jacob/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" \
  PCA_DISABLE_TOOLCHAIN_FALLBACK=1 CLOUD_API_INTERNAL_ORIGIN=http://pca-cloud-api.railway.internal:8080 \
  ./scripts/verify-full.sh
git diff --check
git status --short
```

Expected: full gate ends `FULL VERIFICATION PASSED`; diff check is clean; no
untracked secret/config artifact exists.

```bash
git add scripts/tests/s1b_pairing_acceptance.ts agent/core/tests/support/s1b_acceptance_agent.rs \
  agent/core/Cargo.toml apps/cloud-api/src/test/support/s1b-acceptance-cloud.ts \
  scripts/verify-full.sh scripts/tests/test_verify_full_railway_gate.sh \
  docs/runbooks/S1B_PAIRING_REPAIR.md docs/data/S1B_RAILWAY_DEPLOYMENT_FIELDS.md
git commit -m "test: prove S1B shared pairing acceptance"
```

## Plan self-review

- Task 1 resolves the build-time/runtime proxy contradiction and has explicit
  positive and negative production-build checks.
- Task 2 replaces disconnected fakes with one service-owned state transition,
  asserts all approved acceptance phases, retains canary boundaries, and runs
  in the full gate.
- The plan does not make any external deployment claim or add product scope.
