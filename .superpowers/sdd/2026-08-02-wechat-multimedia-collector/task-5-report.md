# Task 5 Report: Paired WeChat Provider Lifecycle and Local Spool

## Result

Task 5 is complete with the approved production limitation: Agent Core now owns a bounded, joined communication-provider runtime over an injected factory, while the production app installs a fail-closed unavailable factory until a separate source-row task supplies a verified versioned source factory.

No Cloud sync, R2, Dashboard, retention worker, Railway, deployment, or production source-row adapter was added.

## Files changed

- `Cargo.lock` — records the Agent package's existing-workspace provider-contract dependency and the exact direct `rustix` dependency.
- `agent/core/Cargo.toml` — adds `pca-provider-contracts`, exact `rustix = 0.38.44` with `fs`, and Tokio filesystem support.
- `agent/core/src/app.rs` — owns the communication runtime, applies paired control notifications, installs the unavailable production factory, and joins the runtime during cleanup.
- `agent/core/src/cloud_control.rs` — consumes the strict `CommunicationScopeV2`, publishes monotonic validated revisions, and publishes fail-closed disable notifications for invalid/revoked control.
- `agent/core/src/collector_registry.rs` — reuses the communication module's existing 10,000/8,000 outbox hysteresis constants for system collection.
- `agent/core/src/communication.rs` — implements the serial lifecycle, cancellation/join, retry, backpressure, safe local spool, and atomic communication commit boundary.
- `agent/core/src/lib.rs` — exports the communication runtime to the binary and integration tests.
- `agent/core/tests/cloud_control_process.rs` — covers exact v2 scope parsing, monotonic notifications, stale revisions, and invalid-control fail-closed behavior.
- `agent/core/tests/communication_process.rs` — covers all lifecycle, cancellation, retry, backpressure, spool, and no-advance failure cases.
- `crates/provider-contracts/src/lib.rs` — adds the narrow non-`Debug` normalized record envelope, completed-media descriptor, and provider factory boundary.
- `crates/wechat-provider/src/eligibility.rs` — converts provider-private records into validated normalized envelopes.
- `crates/wechat-provider/src/fixtures.rs` — supplies fixture-only account, sequence, and completed-media evidence.
- `crates/wechat-provider/src/lib.rs` — returns normalized records through `CommunicationProvider`.
- `crates/wechat-provider/src/source.rs` — keeps account, source sequence, and completed-media source proof inside the provider-private source model.
- `crates/wechat-provider/tests/provider_contract.rs` — verifies normalized fixture output and completed-media descriptors.
- `.superpowers/sdd/2026-08-02-wechat-multimedia-collector/task-5-report.md` — this report.

## TDD evidence

### RED

The brief's literal command was run first:

```text
cargo test -p pca-agent-core --test communication_process
```

It exited 101 because this workspace has no package named `pca-agent-core`. The actual Agent Core package name is `pca-agentd`; this is a documentation/package-name correction, not a skipped test.

The genuine implementation RED used the actual package:

```text
cargo test -p pca-agentd --test communication_process
```

It exited 101 with unresolved `pca_agentd::communication`, `CommunicationProviderFactory`, `CompletedMediaSource`, and `NormalizedCommunicationRecord`, proving the new runtime and boundary did not exist. A subsequent control-notification RED failed because `CloudControlHandle::communication_controls` did not yet exist.

### GREEN

After implementation and the final cancellation-boundary review:

```text
cargo test -p pca-agentd --test communication_process -- --test-threads=1
```

Exited 0: 10 passed, 0 failed.

```text
cargo test -p pca-agentd --test cloud_control_process
```

Exited 0: 6 passed, 0 failed.

```text
cargo test -p pca-wechat-provider
```

Exited 0: 15 tests passed across unit and integration targets; doc tests also passed.

## Lifecycle and control decisions

- Unpaired, disabled, revoked, stale, and invalid control cannot reach provider factory creation or source probing.
- Only the exact strict v2 communication scope can publish an enabled revision. Stale revisions do not replace the current valid control; contract-invalid control publishes a fail-closed disabled state.
- One supervisor owns one provider at a time. Discovery, polling, retry waits, and control changes are serialized; no detached or overlapping provider tasks are created.
- Disable, unpair, revision replacement, and shutdown cancel in-flight provider futures, call `stop`, and join the owner.
- Media preparation is cancellable. Once a complete commit has passed the final pre-commit control check, the DbActor commit is awaited atomically before a later queued control is processed.
- Retryable provider failures use bounded 30-second, 1-minute, 2-minute, 4-minute, then 5-minute delays. Terminal failures wait for a newer control instead of spinning.
- The production app deliberately installs `UnavailableCommunicationProviderFactory`. Exact enabled v2 control therefore reaches a redacted `WECHAT_CAPABILITY_UNAVAILABLE`/unsupported state without probing guessed paths, spinning, or committing.

## Backpressure and spool decisions

- Outbox collection pauses above 10,000 active rows and resumes only below 8,000; the communication and existing system runtimes share the same constants.
- The media spool has a 6 GiB hard limit. A declared copy that would cross it is rejected before source copying begins, and copying resumes only when current usage is strictly below 5 GiB.
- Completed source media is opened component-by-component without following symlinks. Relative paths, parent traversal, non-files, missing files, and symlinked source components fail closed.
- The private spool root is opened component-by-component with `openat`, `O_NOFOLLOW`, and `O_DIRECTORY`, checked for owner-private permissions, and retained as the pinned directory boundary for usage traversal and attempt cleanup.
- Each copy uses a unique same-root owner-private temporary file, hashes and counts bytes while streaming, flushes and fsyncs, publishes Task 4's deterministic lowercase SHA-256 flat filename without replacing an existing hash, fsyncs the root, then reopens and validates the final file.
- A newly published final file remains attempt-owned until DbActor commit succeeds. Cancellation, later-attachment failure, final validation/fsync failure, or DbActor failure removes every attempt-owned temporary and final name through the pinned directory handle. A pre-existing validated deduplicated file is never attempt-owned or deleted, and successfully committed spool files are retained.
- DbActor receives a communication commit only after every final spool file exists and validates. Hash, size, source-open, copy, quota, high-water, and envelope failures create no Event, Outbox, message, attachment-spool, or Cursor rows.

## Verification

All commands use the actual package name `pca-agentd` where the brief incorrectly says `pca-agent-core`.

| Command | Result |
| --- | --- |
| `cargo fmt --check` | exit 0 |
| `cargo test -p pca-agentd --test communication_process -- --test-threads=1` | exit 0; 10 passed |
| `cargo test -p pca-agentd --test cloud_control_process` | exit 0; 6 passed |
| `cargo test -p pca-wechat-provider` | exit 0; 15 passed plus doc tests |
| `cargo test -p pca-agentd` | exit 0; 54 passed plus doc tests |
| `cargo test --workspace` | exit 0; all workspace and doc tests passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | exit 0 |
| `git diff --check` | exit 0 |

The workspace suite intentionally launches a child crash-marker test that prints an inner `FAILED` result after a deliberate panic. Its enclosing assertion passed, and the workspace command exited 0.

## Self-review

- Changed files map directly to the brief, normalized provider boundary, fixture behavior, dependency admission, and required report.
- No WeChat private source type crosses into Agent Core, and no account id or sequence is derived from `source_key`.
- Normalized records and completed-media descriptors intentionally have no `Debug` implementation; runtime errors and state codes do not expose body, conversation name, source path, key material, or media bytes.
- No source probe occurs before paired exact-v2 enablement and backpressure gates.
- No cursor or communication projection can advance independently of Event and Outbox because Task 4's DbActor transaction remains the sole commit API.
- Existing system lifecycle and collection tests remain green.

## Residual risks and deferred work

- A verified versioned production WeChat source factory is not yet available. Production therefore remains intentionally fail closed with `WECHAT_CAPABILITY_UNAVAILABLE`; fixture-backed tests prove this runtime seam but are not evidence of live SQLCipher source-row extraction.
- Cloud/R2 completion state and the seven-day post-Cloud local-media deletion policy remain deferred to Tasks 9/10. Task 5 has no authority or state with which to perform that deletion.

## Independent review fix round 1

Reviewed base: `7b43aef`.

### Finding closure

1. **Disable/revoke final authorization race — closed.** App startup creates exactly one shared `CommunicationAuthorization`. Cloud control, communication runtime, and every pairing path use that gate. Disable, invalid control, revoke, shutdown, and pairing replacement take its write side and invalidate the monotonic generation before system sync, Keychain deletion, DB cleanup, or watch forwarding. Final communication commit obtains a matching-generation read permit and holds it through DbActor commit; whichever side acquires the lock first defines the linearization order. Enabled authorization is installed only after exact-v2 validation and applied-revision persistence. Integrated tests cover Cloud disable during blocked/failing system sync and revoke during failing credential cleanup with no communication Event, Outbox, projection, attachment, or Cursor commit.
2. **Finalized-but-uncommitted spool ownership — closed.** `AttemptSpoolLease` retains every new temporary and deterministic final name through commit. Cancellation after first final publication, second-attachment failure, DB rejection, final reopen/fsync errors, and control invalidation drop the armed lease and unlink only its names through the pinned root. Successful DbActor commit disarms ownership. A pre-existing validated deduplicated hash survives a failed attempt.
3. **Batch backpressure — closed.** The runtime queries authoritative active Outbox depth before every returned record and before media preparation. At exactly 10,000 the first record may commit; the next sees 10,001, stops the batch, and enters paused hysteresis. A separate test moves depth with System events while the first media record is copying and proves the later record receives no spool file, Event, Outbox, projection, or Cursor advance. Resume remains strictly below 8,000.
4. **Retry classification — closed.** Retry now requires `retryable=true` and one of the provider-approved canonical codes: `WECHAT_WAITING_SOURCE`, `WECHAT_DATABASE_UNAVAILABLE`, or `WECHAT_PROBE_TIMEOUT`. Capability, unsupported, config, stop, malformed, non-WeChat, unknown canonical WeChat, and `retryable=false` errors do not spin. Tests prove exact `30/60/120/240/300/300` timing, the five-minute cap, and reset after success or a newer control. A test-only yielding guard prevents Tokio's paused clock from auto-advancing while external DbActor synchronization completes; the 29-second/30-second boundaries are unchanged.
5. **MIME/kind validation — closed.** Audio, image, and video manifests must respectively use `audio/*`, `image/*`, and `video/*` before the spool root is opened or any copy begins. Mismatch persists nothing and leaks no spool name.
6. **Spool ancestor symlinks — closed.** Source files and the spool root are opened one component at a time with `openat` and no-follow flags. Spool usage iterates from the pinned directory descriptor and opens each flat entry relative to it. Tests reject explicit source and spool ancestor symlinks and deterministically replace the spool pathname during streaming to prove cleanup stays scoped to the displaced pinned directory.
7. **Restart control restoration — closed.** Persisted exact-v2 enabled control is published once when its applied revision equals the server revision, including publication before the communication subscriber is created. Strictly stale revisions remain rejected.
8. **Provider stop errors — closed.** A `stop()` error records only redacted degraded `WECHAT_STOP_FAILED`, quarantines the existing provider, and prevents replacement or overlap. The owner remains alive to process later disable/revoke/shutdown commands and does not assume the failed provider released resources.

### Fix-round RED evidence

- Phase A full communication run exited 101 with 13 passed and one failure: `failed_provider_stop_blocks_enabled_replacement_and_prevents_overlap` returned `WorkerStopped` after the old provider's `stop()` error.
- B1 focused command `cargo test -p pca-agentd --test communication_process b1_ -- --test-threads=1` exited 101 with 1 passed and 6 failed. The failures demonstrated leaked final files after cancellation, DB rejection, and second-attachment failure; accepted MIME mismatch; followed spool ancestor symlink; and cleanup through a replaced pathname.
- B2 focused command `cargo test -p pca-agentd --test communication_process b2_ -- --test-threads=1` exited 101 with 1 passed and 4 failed. The exact retry schedule already passed; failures demonstrated missing per-record batch gating, missing System-event depth gating, missing control reset, and retry of capability-unavailable.
- The added approved-code regression exited 101 with `WECHAT_UNKNOWN_RETRY` creating two factories instead of one, proving that canonical shape alone was too broad.

### Fix-round GREEN and final verification

All final commands ran against the completed fix and exited 0:

| Command | Final result |
| --- | --- |
| `cargo test -p pca-agentd --test communication_process -- --test-threads=1` | 26 passed, 0 failed |
| `cargo test -p pca-agentd --test cloud_control_process` | 8 passed, 0 failed |
| `cargo test -p pca-agentd --test pairing_ipc` | 4 passed, 0 failed |
| `cargo test -p pca-wechat-provider` | 15 passed, 0 failed; doc tests passed |
| `cargo test -p pca-agentd` | 72 passed, 0 failed; doc tests passed |
| `cargo test --workspace` | exit 0; all workspace and doc tests passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | exit 0 |
| `cargo fmt --check` | exit 0 |
| `git diff --check` | exit 0 |

The workspace crash-marker test intentionally launches a child that prints an inner panic/`FAILED`; its enclosing test passed and the workspace command exited 0.

### Deferred Minor and residual risk

- The reviewer's duplicate/existing-hash quota-overcharge Minor was **not** naturally addressed. The ownership lease correctly distinguishes and preserves pre-existing hashes, but quota admission still sums every declared attachment before deduplication. This remains deferred in the review ledger and does not reopen any of the eight required findings.
- The verified versioned production source factory remains unavailable and production remains intentionally fail closed, as recorded above. No source-row extraction was added in this fix round.
