# ADR-0002: Event-first Local Store and Transactional Outbox

Status: Accepted  
Date: 2026-07-30

## Decision

Collectors emit immutable EventEnvelope objects. Event Store, local projections and Sync Outbox are committed in one SQLite transaction.

## Invariants

- Collector never calls Cloud API.
- Event ID is generated on device.
- Retry is idempotent.
- ACK follows server transaction commit.
- Binary attachment upload is a separate resumable flow.
- Event facts are append-only.
