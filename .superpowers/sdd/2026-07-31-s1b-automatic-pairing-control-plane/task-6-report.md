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

## Final build-quality verification (2026-08-01)

This verification pass made no runtime, authentication, Cloud, Collector, or
deployment change. The existing public `pca-keychain` APIs already contain the
required error documentation: the focused strict lint passed, so no API
documentation edit was necessary or made.

| Command | Exit | Result |
| --- | ---: | --- |
| `cargo fmt --all --check` | 0 | passed with Rust stable 1.97.1 |
| `cargo build --workspace` | 0 | passed with Rust stable 1.97.1 |
| `cargo test --workspace` | 0 | passed with Rust stable 1.97.1; includes the S1B control and Keychain tests |
| `PATH=/Users/jacob/.rustup/toolchains/1.82.0-aarch64-apple-darwin/bin:$PATH cargo test --workspace` | 0 | passed with the declared minimum Rust 1.82.0 toolchain |
| `cargo clippy -p pca-keychain --all-targets -- -D warnings` | 0 | focused strict lint passed with Rust stable 1.97.1 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 101 | **blocked/failing**: `clippy::doc_markdown` requires backticks around `WeChat` in `crates/db-local/src/actor.rs:411`; this file is outside the permitted `pca-keychain` public-API documentation-only edit scope |
| `cargo test -p pca-agentd --features process-test-hooks --test process_lifecycle --test system_collector_process --test collector_commit_kill` | 0 | 12 process tests passed |
| `swift build --package-path platform/macos` | 0 | passed (Swift 6.3.3) |
| `swift run --package-path platform/macos BridgeContractVerifier` | 0 | Bridge contract fixture passed |
| `xcodebuild test -project platform/macos/PersonalComputerAgent.xcodeproj -scheme PersonalComputerAgent -only-testing:PersonalComputerAgentTests/PairingCoordinatorTests -derivedDataPath /tmp/pca-verify-full-pairing` | 0 | 2 pairing callback tests passed |
| `pnpm install --frozen-lockfile` | 0 | lockfile current |
| `pnpm typecheck` | 0 | all five TypeScript workspace projects passed |
| `pnpm test` | 0 | Dashboard 11, contracts 15, db-cloud 8, and cloud-api 16 tests passed; domain package has 0 tests |
| `python3 scripts/verify_migrations.py .` | 0 | local and Cloud migration chains passed |
| `python3 scripts/verify_cloud_migrations.py .` | 0 | PostgreSQL 17.10 fresh, replay, upgrade, and Owner-FK checks passed |
| `python3 scripts/verify_boundaries.py .` | 0 | dependency boundaries passed |
| `git diff --check` | 0 | no whitespace errors before this report update |

The repository-standard `./scripts/verify-full.sh` was run with the available
stable Rust toolchain and reached the same workspace-clippy failure after its
structural gate passed. It therefore does not pass as a whole and S1B must not
be declared fully verified until the documented `pca-db-local` lint finding is
addressed by the owner of that file. The installed Rust 1.82.0 toolchain has
`cargo` and `rustc`, but lacks the `rustfmt` and `clippy` components; that is a
toolchain-component limitation, not a passing 1.82 lint result.
