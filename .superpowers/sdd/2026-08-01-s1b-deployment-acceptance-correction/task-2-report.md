# Task 2 Report: Shared S1B Callback-to-Revocation Acceptance

## Result

Implemented one local synthetic Cloud service as the sole owner of pairing
session/code, exchange-issued credential state, device configuration/audit and
revocation. The Rust acceptance Agent receives only `api_origin`, `session_id`,
`callback_state` and `callback_code` on stdin. It exchanges the callback and
sends both pre-revocation and post-revocation control requests through real
loopback HTTP. Dashboard helpers authorize, read, configure, audit and revoke
through that same service.

The harness no longer injects final credentials, hard-codes an authorization
code, supplies a static control snapshot, or swaps in an always-revoked client.
Observed request state proves one exchange, a `200` control response at revision
1, Dashboard revocation of the exact exchanged device, and then a `401` control
response for that same device.

## Changed files

- `apps/cloud-api/src/test/support/s1b-acceptance-cloud.ts`
  - Starts a real loopback Hono server backed by one `MemoryControlRepository`.
  - Generates pairing/callback state, captures the Cloud-generated one-use code,
    gates first control until Dashboard revision 1 exists, and exposes redacted
    test inspection of exchanges/control requests.
  - Keeps raw synthetic credentials only in transient test instrumentation;
    the underlying repository stores their hashes.
- `agent/core/tests/support/s1b_acceptance_agent.rs`
  - Replaces fixed pairing/snapshot/revocation doubles with a loopback-only
    `reqwest` client for exchange, refresh and heartbeat/control.
  - Denies unknown stdin fields, writes returned credentials to the file-backed
    Keychain double, persists only non-secret pairing state to temporary SQLite,
    and uses the real Cloud response to perform revoke cleanup.
- `scripts/tests/s1b_pairing_acceptance.ts`
  - Orchestrates one shared callback-to-revocation flow across Agent, Dashboard
    and Cloud.
  - Scans runtime-generated callback/code/credential/header canaries across
    Agent stdout/stderr, non-credential Cloud/Dashboard JSON, both status JSON
    files, SQLite main/WAL/SHM and fixture/test support sources.
- `scripts/verify-full.sh`
  - Runs the feature-gated Cloud-control process test explicitly and executes
    the shared acceptance with Node's test runner plus the repository `tsx`
    loader before the final success marker.
- `scripts/tests/test_verify_full_railway_gate.sh`
  - Proves the Node test invocation and shared acceptance remain before
    `FULL VERIFICATION PASSED`.
- `docs/runbooks/S1B_PAIRING_REPAIR.md`
  - Documents what the local synthetic flow proves and what still requires a
    live installation.
- `docs/data/S1B_RAILWAY_DEPLOYMENT_FIELDS.md`
  - Describes the build-bound Dashboard readiness from Task 1 and the local-only
    shared acceptance boundary.
- `.superpowers/sdd/2026-08-01-s1b-deployment-acceptance-correction/task-2-report.md`
  - This report.

`agent/core/Cargo.toml` was intentionally unchanged: all required dependencies
(`reqwest`, `serde`, `time`, `tokio`, and `uuid`) were already declared, so a
manifest edit would have added no behavior.

## TDD evidence

- RED: restricted the Agent payload to the approved four fields while the old
  helper still required directly injected credentials.
  - Command:
    `PCA_S1B_ACCEPTANCE_AGENT="$PWD/target/debug/pca-s1b-acceptance-agent" pnpm --filter @pca/cloud-api exec tsx "$PWD/scripts/tests/s1b_pairing_acceptance.ts"`
  - Exit `1`: `acceptance Agent phase pair-control exited 1`.
- RED: required the full gate to invoke the acceptance through Node's test
  runner.
  - Command: `bash scripts/tests/test_verify_full_railway_gate.sh`
  - Exit `1`: `expected S1B acceptance to run through node --test with the tsx loader`.
- GREEN: both behaviors pass with the shared implementation and corrected gate.

## Verification

- `PATH="/Users/jacob/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" cargo fmt --all --check`
  - Exit `0`.
- `PATH="/Users/jacob/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" cargo clippy -p pca-agentd --features process-test-hooks --all-targets -- -D warnings`
  - Exit `0`.
- `PATH="/Users/jacob/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" cargo test -p pca-agentd --features process-test-hooks --test cloud_control_process`
  - Initial focused run: exit `101`, 2 passed / 1 failed because the existing
    paused-time test used fixed yields and observed SQLite before asynchronous
    cleanup completed.
  - Investigation: neither `agent/core/src/cloud_control.rs` nor
    `agent/core/tests/cloud_control_process.rs` was modified; the single test
    passed 5/5, the full file passed 10/10, and both executions inside the final
    full gate passed 3/3. No out-of-scope synchronization change was made.
- `PCA_S1B_ACCEPTANCE_AGENT="$PWD/target/debug/pca-s1b-acceptance-agent" pnpm --filter @pca/cloud-api exec node --import tsx --test "$PWD/scripts/tests/s1b_pairing_acceptance.ts"`
  - Exit `0`; 1/1 shared process acceptance passed.
- `pnpm --filter @pca/cloud-api typecheck`
  - Exit `0`.
- `CLOUD_API_INTERNAL_ORIGIN=http://pca-cloud-api.railway.internal:8080 pnpm --filter @pca/web-dashboard typecheck`
  - Exit `0`.
- `CLOUD_API_INTERNAL_ORIGIN=http://pca-cloud-api.railway.internal:8080 pnpm --filter @pca/cloud-api test`
  - Exit `0`; 20/20 passed.
- `CLOUD_API_INTERNAL_ORIGIN=http://pca-cloud-api.railway.internal:8080 pnpm --filter @pca/web-dashboard test`
  - Exit `0`; 18/18 passed.
- `bash scripts/tests/test_verify_full_railway_gate.sh`
  - Exit `0`.
- `grep -R --fixed-strings -- "accept-canary-" scripts/tests agent/core/tests/support apps/cloud-api/src/test/support || true`
  - Exit `0`; only the intended runtime canary generator was present.
- `PATH="/Users/jacob/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" PCA_DISABLE_TOOLCHAIN_FALLBACK=1 CLOUD_API_INTERNAL_ORIGIN=http://pca-cloud-api.railway.internal:8080 ./scripts/verify-full.sh`
  - Exit `0`; ended `FULL VERIFICATION PASSED`.
  - Included Rust workspace tests, feature-gated process tests, shared S1B
    acceptance, Swift build/contract verifier, Xcode pairing tests, TypeScript
    typecheck/tests, local and Cloud migration replay, and dependency boundaries.

## Exact limitations

- This is local synthetic acceptance over loopback HTTP, not production HTTPS
  or Railway networking.
- The shared flow uses the in-memory Cloud repository and a test Owner
  authenticator. PostgreSQL migration and Better Auth tests pass elsewhere in
  the full gate, but are not the persistence/auth path of this one process flow.
- The helper uses a file-backed Keychain double, not the signed macOS Keychain
  ACL or installed Setup-to-Agent transport.
- To preserve the approved four-field Agent input, the test Cloud binds PKCE to
  the generated callback state and the Agent presents that value as the proof.
  Production pairing continues to own a distinct verifier inside Agent Core;
  this harness proves server-side PKCE validation and HTTP exchange, not the
  installed Setup handoff implementation.
- Direct `node --test scripts/tests/s1b_pairing_acceptance.ts` cannot resolve
  this monorepo's TypeScript workspace `.js` source aliases. The full gate uses
  `pnpm --filter @pca/cloud-api exec node --import tsx --test ...`; the same Node
  test runner executes the file with the already-declared package loader.
- No Railway project/service/domain/Variable, deployment, secret, Git remote or
  production Keychain state was read or changed.
