# ADR-0001: Rust Core Runtime + Swift macOS Bridge

Status: Accepted  
Date: 2026-07-30

## Context

Agent is a long-running background runtime with concurrent collectors, durable SQLite, outbox, command, provider lifecycle and future cross-platform needs. Apple frameworks and TCC are best accessed through Swift.

## Decision

- Rust stable + Tokio owns runtime, state machines, Event, SQLite, Sync, Command and Provider.
- Swift 6.x owns Apple API, TCC, Power, Setup/Repair, SMAppService and Sparkle coordination.
- The boundary is a versioned local protocol over 0600 Unix Domain Socket.
- No opaque in-process FFI for V0.

## Consequences

- Independent crash recovery.
- Contract fixtures required for Rust and Swift.
- Protocol version compatibility becomes a release gate.
- Swift cannot duplicate business state.
