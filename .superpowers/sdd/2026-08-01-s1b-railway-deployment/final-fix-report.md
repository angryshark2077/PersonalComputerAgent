# S1B Railway final fix report

Date: 2026-08-01

## Status

All three final-review findings were addressed in the repository without a
Railway deployment, external account access, secret write, Collector addition,
or public product-scope change.

## Implemented fixes

### 1. Process-level S1B acceptance

- Added a test-only Rust Agent process binary, gated by the existing
  `process-test-hooks` feature.
- Added one orchestrated acceptance harness that exercises:
  1. a real one-use loopback HTTP callback test double;
  2. the real in-memory Hono pairing exchange;
  3. the real Dashboard API client for configuration, audit reads, and revoke;
  4. a spawned Rust process using `AgentPairingService`, a file-backed test
     Keychain identity, real temporary SQLite, and `CloudControlRuntime`;
  5. revision `1` delivery and durable local revision;
  6. revoke followed by the next immediate Agent control request, Keychain
     deletion, local pairing deletion, and disabled `network` and
     `communication.wechat` states.
- Credentials and a message-body canary are generated at runtime. The harness
  asserts they never appear in child stdout/stderr, either JSON status file,
  SQLite database/WAL artifacts, or repository fixture files. The fake
  Keychain is the sole at-rest credential test double and is deleted on revoke.
- Wired the harness into `scripts/verify-full.sh` and added a fixture gate that
  proves the helper is built and the acceptance script executes before the
  final success marker.

### 2. Docker build-context exclusions

- Extended the root `.dockerignore` to exclude `.superpowers`, Rust `target`,
  Swift `platform/macos/.build`, SQLite database/WAL/SHM files, and log files in
  addition to the existing environment, dependency, build, worktree, and Git
  exclusions.
- Extended the existing shell regression test to require every added pattern.

### 3. Dashboard readiness

- Preserved the empty build-time Next configuration when
  `CLOUD_API_INTERNAL_ORIGIN` is unavailable, so image construction does not
  require a Railway runtime variable.
- Moved environment validation into one server-side module shared by Next
  configuration and runtime readiness.
- Made Dashboard `/healthz` dynamic and fail closed with HTTP `503` and exactly
  `{ "status": "not_ready" }` when the private API origin is missing, malformed,
  public, or when a public browser API origin is configured.
- A valid Railway private HTTP origin still returns HTTP `200` and exactly
  `{ "status": "ok" }`.
- Updated the Railway field dictionary, runbook, security boundary, and S1B
  task evidence to describe the readiness and process-acceptance behavior.

## Test-first evidence

The new checks were observed failing before the corresponding implementation:

- `bash scripts/tests/test_dockerignore.sh` exited `1` with
  `missing .dockerignore pattern: .superpowers`.
- The focused Dashboard test exited `1` because the new readiness module did
  not exist.
- `bash scripts/tests/test_verify_full_railway_gate.sh` exited `1` because the
  full gate did not build or execute an S1B acceptance harness.
- The process acceptance command exited `1` with `ENOENT` for the deliberately
  missing Agent helper binary.

## Focused verification

All of the following completed with exit code `0`:

```bash
PATH="/Users/jacob/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" \
  cargo fmt --all --check

PATH="/Users/jacob/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" \
  cargo clippy -p pca-agentd --features process-test-hooks \
  --bin pca-s1b-acceptance-agent -- -D warnings

PATH="/Users/jacob/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" \
  cargo test -p pca-agentd --test cloud_control_process
# 3 passed, 0 failed

PATH="/Users/jacob/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" \
  cargo build -p pca-agentd --features process-test-hooks \
  --bin pca-s1b-acceptance-agent

PCA_S1B_ACCEPTANCE_AGENT="$PWD/target/debug/pca-s1b-acceptance-agent" \
  pnpm --filter @pca/cloud-api exec tsx \
  "$PWD/scripts/tests/s1b_pairing_acceptance.ts"
# S1B process acceptance passed.

pnpm --filter @pca/web-dashboard test
# 14 passed, 0 failed

pnpm --filter @pca/web-dashboard typecheck
pnpm --filter @pca/web-dashboard build
# build passed; /healthz is a dynamic route

bash scripts/tests/test_dockerignore.sh
bash scripts/tests/test_verify_full_railway_gate.sh
git diff --check
```

## Full repository gate

The ordinary command was attempted first:

```bash
PATH="/Users/jacob/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" \
  PCA_DISABLE_TOOLCHAIN_FALLBACK=1 ./scripts/verify-full.sh
```

It exited `101` in the unchanged
`pca-agent-runtime::runtime_foundation` parallel test process. The
`instance_lock_rejects_a_second_owner_until_the_first_is_dropped` test observed
`AlreadyRunning` while the same test executable was also running its
self-spawned crash-marker children. No file under `crates/agent-runtime` is in
this final-fix diff.

The failure was investigated rather than blindly retried:

- the instance-lock test passed alone;
- the complete `runtime_foundation` target passed `15/15` with
  `RUST_TEST_THREADS=1`;
- the standard parallel target reproduced the same interference.

The complete repository gate was then run with only the Rust test scheduler
serialized:

```bash
PATH="/Users/jacob/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" \
  PCA_DISABLE_TOOLCHAIN_FALLBACK=1 RUST_TEST_THREADS=1 \
  ./scripts/verify-full.sh
```

It exited `0` with `FULL VERIFICATION PASSED`. This included strict workspace
clippy, all Rust tests and doctests, the process-level S1B acceptance, Swift
build and Bridge verification, the two PairingCoordinator XCTest cases, all
five TypeScript workspace typechecks, Dashboard `14`, contracts `15`,
db-cloud `8`, and cloud-api `20` tests, local and PostgreSQL migration replay,
and dependency-boundary verification. PostgreSQL `17.10` fresh, replay,
upgrade, and Owner foreign-key checks passed.

A later pre-commit rerun of the same serialized full command exited `101` in
another unchanged timing-sensitive test,
`pca-system-collector::sampler_actor::canceled_queued_response_is_skipped_before_sampling`
(`left: 1`, `right: 0`). That exact test immediately passed alone with
`RUST_TEST_THREADS=1`. No file under `crates/system-collector` is in this diff.
Accordingly, this report records one complete full-gate pass plus two separately
reproduced pre-existing scheduler-sensitive failures; it does not claim the
ordinary parallel full gate is currently stable.

## Docker verification

Both intended image commands were attempted:

```bash
docker build -f deploy/railway/Dockerfile.cloud-api \
  -t pca-cloud-api:final-fix-verify .
docker build -f deploy/railway/Dockerfile.dashboard \
  -t pca-dashboard:final-fix-verify .
```

Both exited `127` because `docker` is not installed in this environment. The
service package builds, Docker exclusion test, and full repository gate passed;
no Docker image success is claimed.

## Files changed

- `.dockerignore`
- `agent/core/Cargo.toml`
- `agent/core/tests/support/s1b_acceptance_agent.rs`
- `apps/web-dashboard/next.config.ts`
- `apps/web-dashboard/src/app/healthz/route.ts`
- `apps/web-dashboard/src/lib/railway-environment.ts`
- `apps/web-dashboard/test/railway-proxy.test.ts`
- `docs/data/S1B_RAILWAY_DEPLOYMENT_FIELDS.md`
- `docs/runbooks/S1B_RAILWAY_DEPLOYMENT.md`
- `scripts/tests/s1b_pairing_acceptance.ts`
- `scripts/tests/test_dockerignore.sh`
- `scripts/tests/test_verify_full_railway_gate.sh`
- `scripts/verify-full.sh`
- `SECURITY.md`
- `tasks/S1B_CLOUD_CONTROL_PLANE.md`
- this report

## Remaining concerns and explicit non-claims

- The existing parallel `runtime_foundation` test interference remains outside
  this fix scope. A subsequent serialized rerun also exposed an unchanged
  System Collector timing test. One serialized complete gate passed, while the
  two failure sites each passed alone; all facts are retained above.
- Docker images were not built because Docker is unavailable.
- No Railway resource, domain, variable, database, migration, or secret was
  read or changed.
- The acceptance harness uses the explicitly approved fake Cloud, loopback
  Setup, Keychain, and temporary SQLite boundaries. It does not prove the
  still-missing signed production Setup-to-Agent UDS handoff or restricted
  production Keychain ACL, and it does not claim live Railway pairing.
- No Network or WeChat Collector source, business Event sync, external
  collector, or public product behavior was added.
