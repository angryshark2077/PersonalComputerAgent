# Task 1 Report: Repair PKCE ownership and paired-state canary proof

## Status

PASS. The two final-review P1 false-proof gaps are repaired in the local
synthetic S1B acceptance harness only. No product pairing behavior, deployment,
Railway state, secret, Dashboard production proxy, migration, or dependency was
changed.

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

## TDD evidence

- The brief's bare `node --test scripts/tests/s1b_pairing_acceptance.ts`
  command exited 1 before loading the test because bare Node cannot resolve the
  workspace `.js` import aliases in this checkout.
- The repository's real equivalent command, with the cloud-api workspace and
  `tsx` loader, exited 1 against the old helper with the expected failure:
  `timed out waiting for Agent-owned pairing start`.
- The unchanged focused Rust control-process baseline passed 3/3 before the
  helper implementation.

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
