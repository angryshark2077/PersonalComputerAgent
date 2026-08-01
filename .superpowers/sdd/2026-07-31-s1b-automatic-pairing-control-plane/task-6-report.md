# Task 6 report: bounded Agent credential/control runtime

## Delivered scope

- Added `pca-agentd::cloud_control`: an authenticated HTTPS-only control port, immediate-then-30-second polling, 15-second request timeout, bounded jittered retry (maximum five minutes), shutdown handle, refresh handling, strict snapshot validation, and monotonic durable revision writes.
- Agent startup validates the device Keychain record before mirroring only its non-secret identity/reference metadata to SQLite. A missing or corrupt record clears a stale local pointer and remains `unpaired`; a valid record with no configured Cloud transport remains local-healthy but `degraded`.
- Confirmed revocation/invalid refresh deletes the Keychain record and atomically clears pairing state while persisting disabled `network` and `communication.wechat` Collector states. S1B starts neither source.
- Moved PKCE verifier and callback-state ownership out of Swift handoff DTOs. Swift now receives Agent-provided `callbackState`, validates it before browser navigation, and returns only `{sessionID, authorizationCode}`. The Rust pairing port owns verifier/session completion and Keychain persistence.
- Pinned the `reqwest` dependency graph to Rust 1.82-compatible `url 2.4.1` and `indexmap 2.2.6`; `reqwest 0.11.27` uses `default-features = false` plus `json` and `rustls-tls` only. `reqwest` is MIT/Apache-2.0; its pinned lockfile selection was compiled with the required Rust 1.82 toolchain.

## Verification

The following commands exited `0`:

```bash
PATH=/Users/jacob/.rustup/toolchains/1.82.0-aarch64-apple-darwin/bin:/opt/homebrew/bin:/usr/bin:/bin cargo test -p pca-agentd --test cloud_control_process
PATH=/Users/jacob/.rustup/toolchains/1.82.0-aarch64-apple-darwin/bin:/opt/homebrew/bin:/usr/bin:/bin cargo test -p pca-agentd
PATH=/Users/jacob/.rustup/toolchains/1.82.0-aarch64-apple-darwin/bin:/opt/homebrew/bin:/usr/bin:/bin cargo test -p pca-agentd --features process-test-hooks --test process_lifecycle --test system_collector_process
xcodebuild test -project platform/macos/PersonalComputerAgent.xcodeproj -scheme PersonalComputerAgent -only-testing:PersonalComputerAgentTests/PairingCoordinatorTests -derivedDataPath /tmp/pca-task6-pairing
git diff --check
```

The deterministic control test proves a revoked response removes both local pairing state and the test Keychain record and leaves both future sensitive Collector keys disabled. The process tests prove existing local lifecycle and System Collector durability remain intact. The Swift target passes its malformed and wrong-state callback checks after the ownership handoff change.

`cargo fmt --all --check` and `cargo clippy -p pca-agentd --all-targets -- -D warnings` were attempted but cannot run because the installed Rust 1.82 toolchain lacks the `rustfmt` and `clippy` components. They were not represented as passing.

## Explicit external bindings intentionally unavailable

1. No approved production Cloud API origin/configuration source exists in this repository. `HttpControlClient` accepts only an HTTPS URL, but Agent startup does not invent one or send a request; it remains `degraded` rather than silently targeting an arbitrary service.
2. No approved authenticated 0600 UDS transport and installed-app Keychain ACL creation binding exists between Swift Setup and Agent Core. The typed Swift and Rust handoff ports are ready and `UnavailablePairingAgentBridge` remains fail-closed; the current macOS Keychain adapter refuses to create an unrestricted device item. Consequently this task does not claim a live Setup-to-Agent-to-Cloud pairing flow.

## Fix round 1 (base `5d315f3`)

- Missing or corrupt Keychain startup now calls the same atomic local operation as revocation: it clears pairing state and disables both `network` and `communication.wechat` durable Collector states. It no longer leaves an enabled sensitive configuration behind.
- Revocation now attempts Keychain deletion, always performs the atomic SQLite cleanup, and only then returns the Keychain error. A failed deletion therefore remains observable without allowing a stale pairing or enabled sensitive Collector state to survive.
- Focused verification exited `0`:

```bash
PATH=/Users/jacob/.rustup/toolchains/1.82.0-aarch64-apple-darwin/bin:/opt/homebrew/bin:/usr/bin:/bin cargo test -p pca-agentd --test cloud_control_process
git diff --check
```

The focused test file now has explicit regressions for corrupt startup records and injected Keychain deletion failures; both assert no pairing state remains and both sensitive Collector keys are `disabled`.
