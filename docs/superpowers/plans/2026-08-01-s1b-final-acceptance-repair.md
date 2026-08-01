# S1B Final Acceptance Repair Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (\`- [ ]\`) syntax for tracking.

**Goal:** Make local S1B acceptance prove Agent-owned PKCE and observe required canary data planes before SQLite cleanup.

**Architecture:** The loopback Hono Cloud remains the sole state owner. A live Rust helper creates the real pairing session after generating a verifier; Cloud records only session/state metadata for the Node driver to authorize the callback. The callback code is streamed to that same helper process, which scans paired SQLite artifacts before checkpointing.

**Tech Stack:** TypeScript, Hono, Node test runner, Rust 1.82/Tokio/reqwest/SQLite, existing S1B process acceptance and full-gate scripts.

## Global Constraints

- Do not deploy, push, alter Railway, change a secret/domain/variable, or modify production protocol, migration, Keychain ACL, or Setup IPC.
- Keep the current Dashboard private-proxy correction unchanged.
- Only the Agent generates the PKCE verifier. Node and Cloud receive its challenge during session start; Cloud receives the verifier only in the real exchange request.
- Callback state and verifier are distinct opaque values. Agent stdin may never contain verifier, session/state, access token, or refresh token.
- Synthetic sensitive values must not occur in stdout, stderr, status JSON, SQLite/WAL/SHM, final credential-double artifacts, or fixture source.
- A runtime message canary must occur in an observed non-credential HTTP JSON response body; headers alone are insufficient.
- The helper scans paired SQLite/WAL/SHM before final checkpoint. The Node driver retains its post-revocation artifact scan.
- Keep one shared Cloud state for pairing, authorization callback, exchange, config/audit/control and revoke; no snapshots or always-revoked test client.

---

### Task 1: Repair PKCE ownership and paired-state canary proof

**Files:**
- Modify: \`apps/cloud-api/src/test/support/s1b-acceptance-cloud.ts\`
- Modify: \`agent/core/tests/support/s1b_acceptance_agent.rs\`
- Modify: \`scripts/tests/s1b_pairing_acceptance.ts\`
- Modify: \`docs/runbooks/S1B_PAIRING_REPAIR.md\`
- Modify: \`docs/data/S1B_RAILWAY_DEPLOYMENT_FIELDS.md\`
- Test: \`scripts/tests/s1b_pairing_acceptance.ts\`
- Test: \`agent/core/tests/cloud_control_process.rs\`

**Interfaces:**
- \`S1bAcceptanceCloud.waitForPairingStart(): Promise<S1bPairingHandoff>\` returns only session ID and callback state observed from the Agent's real session-start request.
- \`inspect().pkce\` exposes only \`{ pairingStarts, verifierDiffersFromCallbackState, challengeMatched }\`, never the verifier.
- The live pairing helper receives one initial stdin JSON object \`{ "api_origin": string }\`, then one callback JSON object \`{ "callback_code": string }\`; both reject unknown keys.
- The pair status adds \`paired_state_canary_checked: true\` only after the helper scans its own SQLite main/WAL/SHM prior to checkpoint.

- [ ] **Step 1: Add failing acceptance assertions**

Replace the Node-created handoff with an Agent process that starts pairing, then wait for Cloud-observed metadata:

\`\`\`ts
const agent = startPairingAgent(runtimeRoot, statusPath, cloud.origin);
const handoff = await cloud.waitForPairingStart();
const redirect = await authorizePairing(fetcher, cloud.origin, handoff.sessionId, handoff.callbackState);
agent.sendCallbackCode(await cloud.acceptCallback(redirect, handoff.callbackState));
assert.deepEqual(cloud.inspect().pkce, {
  pairingStarts: 1,
  verifierDiffersFromCallbackState: true,
  challengeMatched: true,
});
\`\`\`

Require an observed non-credential JSON body containing the message canary, and require the paired status field before final scans. Add a negative helper-input assertion for the old \`{session_id, callback_state}\` input shape.

- [ ] **Step 2: Prove the old harness fails**

Run:

\`\`\`bash
node --test scripts/tests/s1b_pairing_acceptance.ts
PATH="/Users/jacob/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" cargo test -p pca-agentd --features process-test-hooks --test cloud_control_process
\`\`\`

Expected: the revised process test fails because Node starts pairing, callback state is reused as verifier, and no JSON response body carries the canary.

- [ ] **Step 3: Implement Agent-owned session start and exchange**

The Rust helper generates a random base64url verifier and a different callback state, computes the SHA-256/base64url challenge, and calls the real \`POST /v1/device-pairing/sessions\` endpoint through \`AcceptanceHttpClient\`. It retains the verifier in memory, reads the second stdin line containing only the callback code, and sends that retained verifier to the existing real exchange endpoint. It must not print or persist verifier/state/code/credentials.

Use exact input types:

\`\`\`rust
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartInput { api_origin: String }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CallbackInput { callback_code: String }
\`\`\`

The revoke process keeps reading only the stored synthetic credential and does not need pairing metadata.

- [ ] **Step 4: Observe real requests and add a JSON-body canary**

The Cloud wrapper observes the helper's real standard session-start request, records callback state/challenge internally, and resolves \`waitForPairingStart\` only after the real app returns its session ID. It observes exchange input only to record the nonsecret PKCE proof booleans, then delegates both paths to real app handlers.

Add one test-only loopback \`GET /test/s1b/non-credential-canary\` endpoint whose JSON body includes the runtime-generated message canary. Record the body in \`nonCredentialJson\`; call it once from Node. Do not use a header as the sole canary carrier or send the canary through Agent parsing or SQLite.

- [ ] **Step 5: Scan paired state before checkpoint**

At the end of \`pair_and_apply\`, after durable pairing/control checks but before the top-level \`database.checkpoint().await?\`, read \`agent.sqlite\`, \`agent.sqlite-wal\`, and \`agent.sqlite-shm\` if present. Reject byte matches of the known synthetic callback code, verifier, access token, or refresh token. Set \`paired_state_canary_checked: true\` only after this succeeds. Keep the parent's final post-revoke scan for all Cloud-provided sensitive values and final artifact absence.

- [ ] **Step 6: Document and verify**

Update the runbook and field dictionary: this remains local synthetic acceptance, but now proves Agent-owned PKCE and pre-checkpoint temporary SQLite canary coverage. It still does not prove deployed Railway, TLS, PostgreSQL, Keychain ACLs, or Setup IPC.

Run:

\`\`\`bash
PATH="/Users/jacob/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" cargo test -p pca-agentd --features process-test-hooks --test cloud_control_process
node --test scripts/tests/s1b_pairing_acceptance.ts
pnpm --filter @pca/cloud-api typecheck
pnpm --filter @pca/web-dashboard typecheck
PATH="/Users/jacob/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" PCA_DISABLE_TOOLCHAIN_FALLBACK=1 CLOUD_API_INTERNAL_ORIGIN=http://pca-cloud-api.railway.internal:8080 ./scripts/verify-full.sh
git diff --check
git status --short
\`\`\`

Expected: focused checks/typechecks and full gate pass; full gate prints \`FULL VERIFICATION PASSED\`; no prohibited canary is emitted or persisted.

- [ ] **Step 7: Commit**

\`\`\`bash
git add apps/cloud-api/src/test/support/s1b-acceptance-cloud.ts agent/core/tests/support/s1b_acceptance_agent.rs scripts/tests/s1b_pairing_acceptance.ts docs/runbooks/S1B_PAIRING_REPAIR.md docs/data/S1B_RAILWAY_DEPLOYMENT_FIELDS.md
git commit -m "test: harden S1B pairing acceptance proof"
\`\`\`

## Plan self-review

- One task repairs both P1 findings without product or deployment scope.
- The Cloud remains the only shared pairing/control authority and the verifier crosses no boundary except its challenge and exchange proof.
- The task has exact interfaces, state transitions, canary checks, tests and documentation boundary, with no deferred implementation placeholders.

