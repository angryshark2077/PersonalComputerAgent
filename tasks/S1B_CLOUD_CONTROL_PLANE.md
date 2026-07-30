# S1B · Cloud Control Plane

## Must read

- `docs/superpowers/specs/2026-07-31-s1a-local-runtime-installer-design.md`
- `tasks/S1A_LOCAL_RUNTIME_INSTALLER.md`
- Spec §6, §11.3, §18-22, §24-25

## Objective

Add the minimum cloud control plane to an already healthy S1A local runtime:
one-time device pairing, Keychain-backed device credentials, Cloud API,
PostgreSQL device state, real heartbeat, and Dashboard online/offline state.

## Boundaries

- S1B follows S1A and precedes S2 and S3; S2 remains before S3.
- S1B does not add business Collectors, attachments, R2 upload, batch Event
  sync, remote commands, or a second local status UI.
- Device credentials remain in Keychain; Cloud secrets never enter SQLite,
  Event payloads, or logs.
- The S1A user LaunchAgent and per-user installation channel remain unchanged.

## Deliverables

- One-time pairing flow and revocable device identity.
- Cloud API and PostgreSQL records for device control state.
- Authenticated heartbeat and Dashboard online/offline projection.
- Failure states for unavailable cloud, expired pairing, and revoked device
  that preserve the locally healthy S1A runtime.

## Exit gate

- Pairing is one-time and expiration/revocation are enforced.
- A locally healthy device reports authenticated presence without exposing
  credentials.
- Cloud failure never prevents S1A local startup, lifecycle persistence, or
  user LaunchAgent recovery.
- S2 collector work and S3 full batch sync remain separate subsequent slices.
