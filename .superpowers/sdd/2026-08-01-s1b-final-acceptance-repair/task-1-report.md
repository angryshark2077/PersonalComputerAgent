# Task 1 Report: Repair PKCE ownership and paired-state canary proof

## Status

PASS. The original two final-review P1 false-proof gaps and the final broad
review's Dashboard build-argument/deploy-order and parent verifier-scan findings
are repaired. No product pairing behavior, deployment, Railway state, secret,
Dashboard production proxy behavior, migration, or dependency was changed.

## Exact changes

- `apps/cloud-api/src/test/support/s1b-acceptance-cloud.ts`
  - Replaced Node-owned `startPairing()` with `waitForPairingStart()`.
  - Observes the real Agent session-start response and returns only session ID
    and callback state to Node.
  - Observes exchange input only long enough to expose the nonsecret PKCE proof
    booleans: one start, verifier differs from callback state, and challenge
    matches.
  - Added one loopback-only JSON canary endpoint and removed the former header
    canary carrier.
  - Kept the existing shared pairing, exchange, control, configuration, audit,
    and revoke handlers as the authority.
- `agent/core/tests/support/s1b_acceptance_agent.rs`
  - Added strict two-line pairing input: `{ "api_origin": string }`, followed
    by `{ "callback_code": string }`; both deny unknown fields.
  - Generates separate random base64url verifier, callback state, and device
    public key; derives SHA-256/base64url challenge and retains the verifier in
    the same process through the real exchange.
  - Revoke reads only the API origin and the already stored synthetic
    credential.
  - Scans `agent.sqlite`, `agent.sqlite-wal`, and `agent.sqlite-shm` if present
    for callback code, verifier, access token, and refresh token before the
    top-level checkpoint. Only then writes
    `paired_state_canary_checked: true`.
- `scripts/tests/s1b_pairing_acceptance.ts`
  - Starts one live Agent before authorization, streams only callback code back
    to it, and rejects the old Node-owned session/state input shape.
  - Requires the exact PKCE proof object, paired-state scan status, exactly one
    observed JSON canary body, preserved shared configuration/revoke behavior,
    and all existing final secret/artifact absence checks.
- `docs/runbooks/S1B_PAIRING_REPAIR.md` and
  `docs/data/S1B_RAILWAY_DEPLOYMENT_FIELDS.md`
  - Document Agent-owned PKCE continuity, JSON-body canary observation,
    pre-checkpoint paired SQLite/WAL/SHM coverage, and the unchanged local-only
    evidence boundary.

### Final broad-review follow-up

- `deploy/railway/Dockerfile.dashboard`
  - Declares and propagates build-time `CLOUD_API_INTERNAL_ORIGIN` and the
    forbidden `NEXT_PUBLIC_CLOUD_API_ORIGIN` before `next build`, so Railway
    build variables reach the existing fail-closed validation.
- `docs/runbooks/S1B_RAILWAY_DEPLOYMENT.md`
  - Keeps Dashboard as an undeployed empty service, deploys and exposes the
    Cloud private origin first, sets the Dashboard private origin, and only then
    connects Dashboard source for its first build.
  - Corrects local Docker verification to pass the required private build arg
    and an explicitly empty forbidden public arg.
- `scripts/tests/railway_dashboard_build_contract.test.mjs`,
  `scripts/verify-full.sh`, and
  `scripts/tests/test_verify_full_railway_gate.sh`
  - Add a Docker-free executable contract proving Dockerfile arg propagation,
    runbook first-build ordering, corrected local build command, and inclusion
    in the full gate.
- `apps/cloud-api/src/test/support/s1b-acceptance-cloud.ts` and
  `scripts/tests/s1b_pairing_acceptance.ts`
  - Merge the live exchange verifier into the existing internal
    `sensitiveValues` set without adding an inspection field.
  - Require the unchanged inspection shape, six sensitive values, and a
    mutation check showing any sensitive value injected into a prohibited
    parent-scanned status plane is rejected. Existing pre-checkpoint
    SQLite/WAL/SHM coverage remains unchanged.

## TDD evidence

- The brief's bare `node --test scripts/tests/s1b_pairing_acceptance.ts`
  command exited 1 before loading the test because bare Node cannot resolve the
  workspace `.js` import aliases in this checkout.
- The repository's real equivalent command, with the cloud-api workspace and
  `tsx` loader, exited 1 against the old helper with the expected failure:
  `timed out waiting for Agent-owned pairing start`.
- The unchanged focused Rust control-process baseline passed 3/3 before the
  helper implementation.
- The final follow-up Railway contract test failed 3/3 before implementation:
  missing Docker args, missing required deploy-order lines, and missing local
  build args.
- The final follow-up S1B process test failed with `5 !== 6`, proving the live
  verifier was not yet present in the parent sensitive set.

## Fresh verification

- `PATH="/Users/jacob/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" cargo test -p pca-agentd --features process-test-hooks --test cloud_control_process`
  - Exit 0; 3 passed, 0 failed.
- `cargo build -p pca-agentd --features process-test-hooks --bin pca-s1b-acceptance-agent` followed by
  `PCA_S1B_ACCEPTANCE_AGENT="$PWD/target/debug/pca-s1b-acceptance-agent" pnpm --filter @pca/cloud-api exec node --import tsx --test "$PWD/scripts/tests/s1b_pairing_acceptance.ts"`
  - Exit 0; S1B process acceptance 1 passed, 0 failed.
- `pnpm --filter @pca/cloud-api typecheck`
  - Exit 0.
- `pnpm --filter @pca/web-dashboard typecheck`
  - Exit 0.
- `PATH="/Users/jacob/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" PCA_DISABLE_TOOLCHAIN_FALLBACK=1 CLOUD_API_INTERNAL_ORIGIN=http://pca-cloud-api.railway.internal:8080 ./scripts/verify-full.sh`
  - Exit 0; printed `FULL VERIFICATION PASSED`.
- `git diff --check`
  - Exit 0.
- `node --test scripts/tests/railway_dashboard_build_contract.test.mjs`
  - Exit 0; 3 passed, 0 failed.
- `bash scripts/tests/test_verify_full_railway_gate.sh`
  - Exit 0; the new Railway build contract executes before final success.
- Final follow-up S1B process acceptance with the existing Agent helper build
  - Exit 0; 1 passed, 0 failed; the parent set contains six values and exposes
    no dedicated verifier field.
- The first follow-up full-gate run reached and passed all new checks, then an
  unrelated existing paused-time collector test observed `(2, 1)` instead of
  `(1, 1)`. The exact targeted test immediately passed 1/1, no changed file
  touched that crate, and no collector change was made.
- A single full-gate rerun with the required environment
  - Exit 0; printed `FULL VERIFICATION PASSED`, including the new Railway
    contract 3/3 and repaired S1B process acceptance 1/1.

## Limits and residual concerns

- This is local synthetic acceptance. It does not prove deployed Railway,
  public TLS, PostgreSQL runtime behavior outside the existing temporary gate,
  Better Auth production configuration, signed Setup IPC, macOS Keychain ACLs,
  or a live installed pairing.
- The bare Node command in the brief remains unsuitable for this workspace;
  the repository gate's `pnpm --filter ... node --import tsx` invocation is the
  executable acceptance command.
- The full gate emitted only existing build/deprecation warnings; no failing
  test remained. No canary or credential was emitted or persisted according to
  the acceptance scans.
- No deploy, push, Railway/secret mutation, or Dashboard production proxy
  change was performed.
- Docker is unavailable in this environment. The Dockerfile/runbook contract is
  executable without Docker, but neither corrected image build was run; image
  construction and live Railway build-arg delivery remain unverified.
