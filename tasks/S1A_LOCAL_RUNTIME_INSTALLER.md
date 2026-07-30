# S1A · Local Runtime and Self-Install DMG

## Must read

- `docs/superpowers/specs/2026-07-31-s1a-local-runtime-installer-design.md`
- `docs/adr/ADR-0005-user-level-self-install-channel.md`
- `docs/INSTALLATION_CHANNELS.md`
- Spec §11, §12, §18, §20.3, §24-25

## Objective

Build a self-use Apple Silicon macOS 13+ DMG that installs a durable local
Rust `agentd` and resident Swift PlatformBridge without administrator
privileges or cloud-control-plane scope.

## Fixed boundaries

- Install only to `$HOME/Library/Application Support/PersonalComputerAgent`.
- Keep `App/`, `Data/`, and `Run/` separate; upgrades replace only `App/`.
- Sign with Apple Development / Personal Team and use a user
  `SMAppService` LaunchAgent.
- Never use root, a LaunchDaemon, privileged helper, Developer ID,
  notarization, Sparkle, or a quarantine bypass.
- S1A emits lifecycle Events only. It has no collectors, pairing, Cloud API,
  PostgreSQL, R2, batch sync, remote commands, or persistent local status UI.

## Deliverables

- State-driven SwiftUI installer with safe install, replacement, rollback,
  manual approval, and local uninstall flows.
- `agentd` runtime state, single-instance lock, crash marker, heartbeat,
  SQLite Event Store, and transactional lifecycle Outbox.
- Resident Swift PlatformBridge with authenticated, versioned JSON-over-Unix
  socket handshake, TCC status reads, and sleep/wake notifications.
- User LaunchAgent registration and bounded local health verification.

## Exit gate

- A signed self-use DMG installs to the fixed per-user path without admin
  privileges or hidden approval bypasses.
- Login persistence starts `agentd`; heartbeat is locally healthy within five
  seconds without waiting for providers.
- Bundle replacement rolls back without touching persistent data.
- Bridge failure degrades only Bridge-dependent capability; local SQLite facts
  and lifecycle Outbox transactions remain durable.
- Default uninstall removes `App/` and `Run/` but preserves `Data/`.
