# S1B Cloud Control Plane Data Dictionary

**Status:** implemented local/control contracts; production deployment and the
Swift-to-Agent handoff transport are intentionally not configured.

S1B contains device pairing, identity, desired Collector configuration and
presence only. It does not contain business Events, Network observations,
WeChat message bodies, attachments, public-IP data, or geo projections.

## Local SQLite: `pairing_state`

Migration: `crates/db-local/migrations/0003_s1b_pairing_state.sql`.
There is at most one row (`singleton_id = 1`). Agent Core is the sole writer
through `DbActorHandle`.

| Field | Type / constraint | Meaning | Secret / retention |
|---|---|---|---|
| `singleton_id` | integer, fixed `1` | Singleton key | normal; cleared on unpair/revocation |
| `device_id` | canonical UUID text | Cloud device identity | normal; cleared on unpair/revocation |
| `workspace_id` | canonical UUID text | Owning Cloud Workspace | normal; cleared on unpair/revocation |
| `credential_ref` | `keychain://%` text | Keychain pointer only | non-secret reference; cleared on unpair/revocation |
| `credential_generation` | non-negative integer | Current Cloud credential generation | normal; replaced on validated refresh |
| `applied_control_revision` | non-negative integer | Highest complete desired configuration applied | normal; cleared on unpair/revocation |
| `paired_at_ms` | non-negative Unix milliseconds | Last validated pairing write | normal; cleared on unpair/revocation |

The table deliberately has no access credential, refresh credential,
authorization code, device key material, message body, or Event payload.

## Cloud PostgreSQL

Migrations are ordered and immutable:

1. `0000_baseline.sql` creates `_pca_migrations`.
2. `0001_s1b_control_plane.sql` creates S1B identity, pairing, device,
   configuration and heartbeat state.
3. `0002_s1b_device_revocation_audit.sql` creates the immutable revocation
   audit.
4. `0003_s1b_pairing_state_and_better_auth_session.sql` adds Better Auth's
   session token compatibility field and pairing callback-state hash.

### Account and Workspace scope

| Table | Fields | Scope and retention |
|---|---|---|
| `auth_users` | `id`, `name`, `email`, verification/image and timestamps | Better Auth account data; private self-use Owner only |
| `auth_sessions` | user ID, hashed legacy session token, optional Better Auth session token, expiry, IP, user agent and timestamps | Better Auth session lifecycle; session token is an auth secret and must never be logged |
| `auth_accounts` | user ID, provider/account identifiers, optional password hash and timestamps | Better Auth account lifecycle; password hash is secret-store data |
| `workspaces` | ID, name, unique slug and timestamps | one Owner Workspace per S1B user |
| `workspace_members` | workspace/user IDs, fixed `owner` role, timestamp | composite membership FK is the tenant boundary; no public membership UI in S1B |

### Pairing and device credentials

| Table | Fields | Scope and retention |
|---|---|---|
| `pairing_sessions` | SHA-256 session/device-public-key/callback-state hashes, PKCE challenge, callback URI, expiry/created/authorized timestamps | five-minute one-time pairing session; no plaintext code or credential |
| `pairing_authorization_codes` | SHA-256 authorization-code and callback-state hashes, session, workspace/Owner, expiry/created/consumed timestamps | one-time code; server consumes it atomically; no plaintext authorization code |
| `devices` | device/workspace/Owner IDs, public-key hash, fixed `macos` platform, created/revoked timestamps | device belongs to its composite Workspace/Owner membership |
| `device_credential_generations` | device/workspace, generation, SHA-256 access/refresh hashes, expiries, created/revoked timestamps | credentials rotate by generation; only hashes persist; revocation invalidates active generations |

### Desired configuration, audit, and presence

| Table | Fields | Scope and retention |
|---|---|---|
| `collector_configs` | workspace/device, non-negative revision, `network_enabled`, `wechat_enabled`, update time | one complete desired configuration per device; S1B transports it but starts neither source |
| `collector_config_audit` | ID, workspace/device, actor, positive revision, old/new JSON configuration, time | append-only Owner audit; records only the two approved boolean scope settings |
| `device_heartbeats` | ID, workspace/device, received time, Agent version, presence, non-negative Outbox depth | authenticated presence history; S1B has no Event upload |
| `device_revocation_audit` | ID, workspace/device, Owner actor, revoked time | append-only device revocation audit |

### Indexes and integrity boundaries

- `idx_pairing_sessions_active_expiry` expires unauthorised pairing sessions.
- `idx_devices_workspace` scopes device lookup to a Workspace.
- `idx_collector_config_audit_chronology` and
  `idx_device_revocation_audit_chronology` support Owner audit timelines.
- `idx_device_heartbeats_last` supports recent presence lookup.
- Composite `(workspace_id, user_id)` membership foreign keys protect pairing
  authorization, device ownership and configuration/revocation audit actors.
- Device credentials and pairing values are global unique SHA-256 values;
  plaintext access tokens, refresh credentials and authorization codes do not
  appear in database columns.

## Secrets and retention boundary

- Device access/refresh credentials and device key material belong only in the
  macOS Keychain and Cloud request/secret handling. They are never SQLite,
  Event, fixture, diagnostic, JSON status, or ordinary log data.
- Better Auth password/session secrets remain inside Better Auth's own
  authentication boundary and are not exposed through Dashboard device APIs.
- S1B does not create raw Network or WeChat data. The future retention rules
  are 30 days for raw Network identifiers and 90 days for WeChat body/display
  names; those rules require their separate S2B/S3 implementation before any
  such data is collected.
