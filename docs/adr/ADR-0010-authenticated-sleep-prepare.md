# ADR-0010: Authenticated sleep preparation acknowledgement

Status: Accepted
Date: 2026-08-15

## Decision

The signed macOS Platform Bridge uses AppKit's bounded `willSleep` handling window to call an Agent-owned `sleep-control.sock` before it reports the sleep lifecycle event. The socket is a direct child of the private runtime directory, mode `0600`, accepts only `prepare_sleep`, and requires a fresh nonce plus HMAC proof derived from the existing Bridge shared secret.

The Agent stops System and Communication collectors, persists `system.sleep` through the transactional Event Outbox, and checkpoints SQLite/WAL before it returns `{ "ok": true }`. The existing Bridge lifecycle buffer remains the wake/recovery channel: its following sleep event is idempotently ignored, and its wake event resumes the suspended collectors.

## Consequences

- The normal Bridge request protocol remains v1; this is a distinct, narrow local protocol at v1 rather than a breaking extension of the general Bridge envelope.
- A missing secret, bad proof, unavailable Agent, or timeout returns no acknowledgement. Bridge still records the lifecycle event and macOS proceeds with sleep; the system never claims durability it did not receive.
- The operation has a 20-second Agent budget and a 25-second Bridge socket budget, below AppKit's documented 30-second sleep-delay bound. Physical sleep/wake remains an end-to-end acceptance scenario because macOS owns the final power deadline.
- Swift only handles macOS power notification and authenticated IPC. Rust retains collector, SQLite, Event Outbox, and recovery ownership.
