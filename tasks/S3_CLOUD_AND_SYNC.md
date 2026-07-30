# S3 · Cloud and Sync

## Must read

Spec §6.3, §11.3, §17.1-17.5, §18-22, §25.

## Objective

Pair devices and synchronize Event/Attachment/Health data idempotently.

## Deliverables

- Better Auth Web session.
- one-time device pairing flow.
- device credential storage in Keychain.
- Hono Agent API with OpenAPI.
- PostgreSQL Drizzle schema/migrations.
- Batch Sync request/response.
- ACK, duplicate and reject semantics.
- retry classes and dead letter.
- R2 presigned upload/init/complete.
- config pull.
- command queue and result.
- heartbeat/presence.
- Workspace scope middleware.

## Tests

- pairing token expires and is one-time.
- duplicate event returns duplicate/accepted deterministically.
- partial batch rejection.
- 429/5xx/offline retry.
- attachment hash mismatch.
- cross-workspace access rejected.
- command duplicate/idempotency.
- revoked device rejected.
- tombstone skeleton round-trip.

## Exit gate

- duplicate uploads do not duplicate Event/projection.
- object upload complete verifies hash.
- offline queue recovers.
- Workspace isolation test passes.
