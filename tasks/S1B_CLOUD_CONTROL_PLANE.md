# S1B · Cloud Control Plane

## Must read

- `docs/superpowers/specs/2026-07-31-s1a-local-runtime-installer-design.md`
- `tasks/S1A_LOCAL_RUNTIME_INSTALLER.md`
- Spec §6, §11.3, §18-22, §24-25

## Objective and implementation status

Add the minimum cloud control plane to an already healthy S1A local runtime:
one-time device pairing, Keychain-backed device credentials, Cloud API,
PostgreSQL device state, real heartbeat, and Dashboard online/offline state.

The S1B repository slice is implemented and covered by contract, API, local
state, revocation and migration-replay gates. It remains deliberately
fail-closed until an HTTPS Cloud origin and the signed Swift Setup-to-Agent
handoff/Keychain ACL bootstrap are deployed. That means no live production
pairing is claimed by this task.

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

The full repository gate includes a process acceptance harness using a
loopback Setup callback double, in-memory Cloud API, file-backed test Keychain,
temporary SQLite, and the real Dashboard API client. It verifies revision-1
audit delivery and revoke cleanup while scanning stdout, JSON status, SQLite,
and repository fixtures for runtime credential and message-body canaries.

The authoritative operational procedure is
`docs/runbooks/S1B_PAIRING_REPAIR.md`; field, index, retention and secret
boundaries are in `docs/data/s1b-control-plane.md`.

## Railway preparation boundary

The local release gate includes the offline Railway deployment-verifier test.
It verifies preparation for `pca-cloud-api` and `pca-dashboard`, private
Railway PostgreSQL, same-origin Dashboard proxying, and immutable Cloud
migrations. It does not create Railway resources or prove generated domains,
Variables, migration execution, browser authentication, or Setup-to-Agent
pairing. Follow `docs/runbooks/S1B_RAILWAY_DEPLOYMENT.md` before claiming live
deployment or pairing; the exact deployment fields and secret boundary are in
`docs/data/S1B_RAILWAY_DEPLOYMENT_FIELDS.md`.
