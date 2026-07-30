# S1 · Rust Core + Swift Bridge

## Must read

Spec §6, §11, §12, §13, §18, §20.3, §24-25.

## Objective

Build a durable resident Rust `agentd` and a minimal Swift PlatformBridge without business Collector scope creep.

## Deliverables

### Rust

- single-instance lock
- structured tracing and crash marker
- Agent state machine
- Keychain Port
- SQLite DbActor
- WAL, foreign keys, busy timeout
- immutable migration runner
- integrity and smoke checks
- Event Bus
- local Event Store and Outbox transaction
- Heartbeat worker
- Bridge client/supervisor
- graceful shutdown and sleep hooks

### Swift

- Bridge server
- protocol handshake
- capability probe
- TCC status read
- Power sleep/wake events
- SMAppService registration
- minimal Setup/Repair placeholder

## Non-goals

- No Screenshot/Activity implementation beyond fixture capability.
- No WeChat.
- No cloud sync beyond heartbeat interface fixture.
- No local daily UI.

## Failure paths

- Bridge missing
- Bridge crashes after handshake
- incompatible protocol
- SQLite locked/corrupt
- migration failure
- Keychain unavailable
- duplicate agent instance
- sleep during queued write

## Exit gate

- Login LaunchAgent starts Rust agentd.
- Heartbeat starts in <5s without waiting for providers.
- Bridge crash degrades only bridge-dependent capabilities and reconnects.
- SQLite Event + Outbox commit survives process kill.
- Migration failure enters repair and restores backup.
