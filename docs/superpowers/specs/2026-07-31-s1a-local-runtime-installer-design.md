# S1A Local Runtime and Self-Install DMG Design

**Status:** User-approved design  
**Date:** 2026-07-31  
**Target:** Apple Silicon, macOS 13 and later

## 1. Goal

S1A delivers a self-use macOS DMG that installs and starts a durable local PCA runtime without administrator privileges. The installed runtime consists of a resident Rust `agentd`, a resident Swift PlatformBridge, a local SQLite fact/outbox store, and a user-approved `SMAppService` LaunchAgent.

S1A proves installation, login persistence, process supervision, local transaction durability, sleep/wake handling, safe replacement, rollback, and uninstall behavior. It does not add business Collectors or a cloud control plane.

## 2. Roadmap Decomposition

The previously separate S1 and S3 outcomes will be delivered as ordered, independently accepted slices:

1. **S1A — Local Runtime and Installer:** DMG, installer, LaunchAgent, Rust runtime, Swift Bridge, SQLite, local lifecycle heartbeat.
2. **S1B — Cloud Control Plane:** one-time pairing, Keychain device credentials, Cloud API, PostgreSQL, real heartbeat, and Dashboard online/offline state.
3. **S2 — Core Collectors:** Activity, System, and Screenshot into the local Event Store and Outbox.
4. **S3 — Complete Sync:** authenticated batch sync, idempotency, offline recovery, R2 upload, and real Dashboard data.

R2 and complete sync remain after Collectors because S1A has no real attachment payload to upload.

## 3. Frozen Decisions

| Area | Decision |
|---|---|
| Distribution | Self-use, unnotarized DMG |
| CPU | Apple Silicon `arm64` only |
| Minimum OS | macOS 13 |
| Signing | Free Apple Development / Personal Team |
| Installer UI | Single-page, state-driven SwiftUI app |
| Installed location | `~/Library/Application Support/PersonalComputerAgent/App/PersonalComputerAgent.app` |
| Persistent data | `~/Library/Application Support/PersonalComputerAgent/Data/` |
| Ephemeral runtime | `~/Library/Application Support/PersonalComputerAgent/Run/` |
| Background model | User-level `SMAppService` LaunchAgent |
| Rust runtime | Resident `agentd`, started by `launchd` |
| Swift Bridge | Resident while `agentd` is running; supervised and reconnected by Rust |
| Local management UI | None after installation |
| Future status UI | S1B Web Dashboard |
| Uninstall | Local terminal command; default preserves data |
| Root service | Explicitly excluded |

## 4. Source-Spec Delta

The current product specification says the bundle is installed at `/Applications/PersonalComputerAgent.app`. The later user-approved S1A decision replaces that location for the self-use channel with a per-user Application Support location.

Before implementation code is added, the implementation plan must create an ADR for this channel-specific installation decision and update the Markdown architecture/install documentation. The source DOCX is retained as historical input; implementation must not silently treat both locations as active.

This decision does not authorize a root LaunchDaemon. Both Rust and Swift processes run as the logged-in user.

## 5. Bundle and Directory Layout

The DMG contains one self-installing app:

```text
PersonalComputerAgent.dmg
└── Install Personal Computer Agent.app
    ├── Contents/MacOS/PersonalComputerAgent
    ├── Contents/Resources/bin/pca-agentd
    ├── Contents/Resources/bin/PCAPlatformBridge
    ├── Contents/Library/LaunchAgents/com.pca.agentd.plist
    └── signed resources and metadata
```

The installed layout separates replaceable code from persistent data:

```text
~/Library/Application Support/PersonalComputerAgent/
├── App/PersonalComputerAgent.app
├── Data/
│   ├── agent.sqlite3
│   ├── logs/
│   ├── backups/
│   └── crash-marker.json
└── Run/
    ├── agent.lock
    ├── bridge.sock
    └── runtime-status.json
```

The root directory and `Data`/`Run` directories are mode `0700`. The database, socket, crash marker, and sensitive runtime files are mode `0600`. Upgrade replaces only `App/`; it never uses the install directory itself as the data directory.

## 6. Component Boundaries

### 6.1 Swift Installer / Setup

The SwiftUI app has one state-driven screen. Before installation it explains the target location, user-level background behavior, and local-data boundary, then presents one primary action: **Install and Start**.

It is responsible for:

- detecting whether it runs from the DMG or the installed path;
- staging and validating a replacement app bundle;
- atomically installing or upgrading `App/PersonalComputerAgent.app`;
- relaunching from the installed path before registering the service;
- calling `SMAppService.agent(plistName:)` from the installed signed bundle;
- opening Login Items settings and waiting when status is `requiresApproval`;
- starting the service and waiting for a bounded local health result;
- showing actionable install, approval, startup, or rollback failure states.

It does not remain resident after a successful install and provides no menu-bar item or persistent local status dashboard.

### 6.2 Rust `agentd`

Rust owns:

- the single-instance lock and crash marker;
- runtime state transitions;
- structured, redacted local logging;
- SQLite DbActor and immutable migration runner;
- Event Bus, lifecycle Event Store, and Sync Outbox transaction;
- local heartbeat metadata;
- Bridge launch, handshake, deadline enforcement, restart, and reconnect;
- graceful termination and sleep preparation.

Rust does not call Apple UI frameworks, request TCC permissions, access Cloud APIs, or implement business Collectors in S1A.

### 6.3 Swift PlatformBridge

Swift owns:

- the local Bridge server;
- protocol handshake and capability response;
- TCC status reads without prompting;
- power sleep/wake notifications;
- versioned error mapping.

Swift does not access SQLite, Cloud APIs, Outbox state, or business runtime state.

## 7. Installation and Upgrade Lifecycle

### 7.1 First Install

1. The user manually allows the unnotarized, Apple Development-signed Installer if Gatekeeper requires it.
2. Installer creates the per-user root, `App`, `Data`, and `Run` directories with restrictive permissions.
3. Installer copies itself to a sibling staging path under `App/`.
4. Installer validates bundle structure, `arm64` architecture, version metadata, and code signatures.
5. The staged bundle is atomically moved into the installed path.
6. Installer relaunches the installed executable in setup mode and exits the DMG copy.
7. The installed app registers the embedded LaunchAgent through `SMAppService`.
8. If approval is required, it opens System Settings and waits without bypassing the decision.
9. After enablement, it starts `agentd` and waits for the bounded S1A health result.
10. The UI reports success and exits.

The Installer never removes quarantine attributes, disables Gatekeeper, edits background-item databases, simulates approval, or uses `launchctl` to bypass `SMAppService`.

### 7.2 Cover Upgrade

1. Detect the installed version and reject downgrade by default.
2. Stage and fully validate the new bundle while the old runtime remains active.
3. Request a graceful Agent/Bridge stop with a timeout.
4. Move the current bundle to a sibling rollback path.
5. Atomically activate the new bundle and preserve `Data/` and Keychain entries.
6. Re-register if required, start, and run health checks.
7. On success, delete the rollback bundle.
8. On failure, restore the old bundle, restart it, and retain redacted diagnostic evidence.

No database schema is contracted during a bundle replacement. A migration failure follows the database recovery path rather than deleting data.

## 8. Uninstall Semantics

The installed executable exposes a fixed local command:

```text
"$HOME/Library/Application Support/PersonalComputerAgent/App/PersonalComputerAgent.app/Contents/MacOS/PersonalComputerAgent" --uninstall
```

Default uninstall:

- unregisters the `SMAppService` LaunchAgent;
- stops Agent and Bridge;
- removes `App/` and `Run/`;
- preserves `Data/` and Keychain credentials.

Complete uninstall adds `--delete-data`, displays the exact target paths and credential scope, and requires an explicit confirmation token before deleting `Data/` and PCA-owned Keychain items. It never deletes parent Application Support directories or unrelated credentials.

Remote uninstall is excluded because it is unsafe as the only recovery path and unavailable while the device is offline.

## 9. Runtime and IPC

### 9.1 Startup

1. `launchd` starts Rust `agentd` through the registered user LaunchAgent.
2. Rust acquires the single-instance lock before opening SQLite.
3. Rust initializes redacted tracing and crash-marker recovery.
4. DbActor opens SQLite with WAL, foreign keys, `busy_timeout=5000`, and `synchronous=NORMAL`.
5. The immutable migration chain, `integrity_check`, `foreign_key_check`, and smoke queries run.
6. Rust launches the resident PlatformBridge and completes a bounded protocol handshake.
7. Rust starts the lifecycle Event Bus and local heartbeat.
8. Runtime becomes `unpaired` but locally healthy; S1B later adds pairing and cloud status.

### 9.2 IPC

S1A uses length-prefixed JSON over a Unix Domain Socket under `Run/`:

- directory mode `0700`, socket mode `0600`;
- canonical `protocol_version`, `request_id`, `message_kind`, `capability`, `deadline_ms`, `payload`, and `error` fields;
- mutual nonce challenge using a random Bridge shared secret stored in Keychain;
- unknown protocol versions and missing deadlines are rejected;
- secrets, nonces, and raw payloads are not logged;
- every request has a bounded deadline;
- reconnect uses bounded exponential backoff with jitter.

JSON is the only S1A wire encoding. MessagePack is not added until measured need justifies a second encoding.

## 10. Local Database Slice

S1A adds the minimum immutable migration after `0000_baseline.sql` for:

- `local_meta` — schema/runtime metadata;
- `agent_state` — current durable runtime state and transition metadata;
- `events_local` — append-only lifecycle Events;
- `sync_outbox` — lifecycle Event upload intent for future S3 consumption;
- `diagnostic_events` — bounded, redacted operational records.

A lifecycle Event and its Outbox row are written in one SQLite transaction. Projection and Collector tables that have no S1A producer are not created early.

S1A emits only lifecycle Events such as `AGENT_STARTED`, `BRIDGE_READY`, `SYSTEM_SLEEP`, and `SYSTEM_WAKE`. It does not emit fake Activity, Screenshot, Browser, File, Location, or WeChat data.

## 11. Failure Handling

| Failure | Required behavior |
|---|---|
| Duplicate Agent | Second process does not open SQLite; it exits with a specific diagnostic result. |
| Bridge absent or crashes | Runtime becomes degraded; SQLite and lifecycle processing continue; Bridge restarts with bounded backoff. |
| Protocol incompatible | Bridge-dependent capabilities stay disabled; no unsafe fallback encoding is attempted. |
| Keychain unavailable | Bridge authentication remains unavailable; runtime reports degraded without storing the secret in files. |
| SQLite locked | Retry only within bounded busy timeout; report repair-needed if startup cannot safely proceed. |
| Integrity or migration failure | Enter repair, preserve original DB/backup, and do not start future Collectors or Cloud workers. |
| Sleep during queued write | Stop accepting new side effects, commit/rollback the transaction, checkpoint WAL, then acknowledge sleep preparation. |
| New bundle fails health check | Restore and restart the prior bundle without touching persistent data. |

## 12. Build and Signing

The development Mac requires full Xcode. Target Macs do not require Xcode, Rust, Node, or source code.

- SwiftUI Installer, PlatformBridge, entitlements, and `SMAppService` resources are built by an Xcode project.
- `pca-agentd` is built as an `arm64-apple-darwin` Release binary.
- Nested executables are signed inside-out, then the app bundle is signed with the free Personal Team Apple Development identity.
- Team ID, Apple account data, certificate private keys, and signing passwords are never committed.
- A deterministic packaging script assembles the app and uses `hdiutil` to create the DMG.
- The build rejects unsigned nested code, a non-`arm64` executable, missing plist resources, or a failed strict signature check.
- S1A does not use Developer ID, Apple notarization, automatic quarantine removal, or global Gatekeeper changes.

## 13. Verification Strategy

### Automated

- Rust unit tests: state transitions, single-instance behavior, bounded backoff, crash marker, and error mapping.
- Database tests: empty migration, replay, integrity failure, locked DB, Event + Outbox atomicity, and process-kill durability.
- Swift tests: Bridge handshake, incompatible version, deadline, TCC mapping, sleep/wake mapping, and install-path validation.
- Process integration: kill Bridge and observe reconnect; kill Agent and observe `launchd` recovery; reject duplicate Agent.
- Installer integration using an isolated test root: first install, repeat install, upgrade, downgrade rejection, failed-health rollback, default uninstall, and confirmed complete uninstall.
- Package checks: DMG mount, expected bundle tree, `arm64` inspection, strict code-signature verification, and install smoke test.

### Real macOS acceptance

- A user opens the DMG and completes the graphical installation without an administrator password.
- Gatekeeper and background-item approval remain visible user decisions.
- Agent becomes locally healthy within five seconds without waiting for Bridge-dependent providers or Cloud.
- Closing Installer leaves no Dock, menu-bar, or management UI process.
- Logout/login automatically starts `agentd` and the resident Bridge.
- Killing Bridge degrades and then recovers without stopping Rust or corrupting SQLite.
- Killing Agent during an Event/Outbox scenario leaves either both rows committed or neither row committed.
- Upgrade failure restores the previous bundle; uninstall preserves or explicitly deletes data according to the selected mode.

## 14. Non-Goals

S1A does not include:

- a root LaunchDaemon or privileged helper;
- Developer ID distribution, notarization, or a production update feed;
- Cloud API, PostgreSQL, pairing, remote heartbeat, or Dashboard status;
- Activity, System, Screenshot, Browser, File, Location, or WeChat Collectors;
- R2, batch sync, remote commands, or remote uninstall;
- a menu-bar item, installed Manager shortcut, or persistent local status UI;
- Intel builds or universal binaries.

## 15. Definition of Done

S1A is complete only when all of the following are evidenced on the merged tree:

1. The signed `arm64` DMG installs through the approved SwiftUI flow.
2. Installation uses the per-user Application Support paths and requires no administrator password.
3. The user explicitly controls Gatekeeper and background-item approval.
4. `agentd` starts immediately, becomes locally healthy within five seconds, and starts again after logout/login.
5. PlatformBridge remains resident, authenticates over the local socket, and recovers after forced termination.
6. SQLite migration, integrity, Event + Outbox atomicity, and process-kill tests pass.
7. Upgrade health failure restores the prior bundle without changing persistent data.
8. Local uninstall works with both preserve-data and confirmed delete-data modes.
9. Format, lint, Rust/Swift build, unit, contract, migration, boundary, installer, DMG, and real-login smoke gates pass.
10. No Root, Cloud, Collector, secret, or public-distribution scope is introduced.
