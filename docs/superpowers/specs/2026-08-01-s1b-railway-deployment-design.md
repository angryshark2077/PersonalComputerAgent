# S1B Railway Deployment Design

**Status:** approved design

## Goal

Deploy the self-use S1B Cloud control plane on Railway Singapore without
committing secrets, exposing PostgreSQL, or turning an absent deployment
configuration into a paired Agent.

## Scope

- One Railway project: `pca-control-plane`.
- Three same-region resources: managed PostgreSQL, `pca-cloud-api`, and
  `pca-dashboard`.
- A reproducible Node build/start path for the Hono API and Next Dashboard.
- An idempotent, checksum-recorded Cloud SQL migration runner.
- A production-like Railway acceptance checklist. It is not a claim that a
  live device has paired before the checklist is run.

## Exclusions

- No automated Railway login, project/service creation, or secret write.
- No public PostgreSQL access.
- No new Collector, business Event sync, or remote-command capability.
- No automatic Agent pairing until the installed Setup-to-Agent handoff and
  Keychain ACL path are exercised against the deployed API.

## Chosen topology

Railway runs all resources in **Southeast Asia / Singapore**. PostgreSQL keeps
its Railway-private connection string. The Cloud API receives that string only
through a Railway variable reference.

`pca-dashboard` is the browser origin. It proxies `/api/auth/*` and `/v1/*`
to the API's Railway-private hostname, so browser cookies stay same-origin. The
Cloud API has its own public Railway domain only for the installed Agent's
HTTPS control client. This avoids browser CORS/cross-site-cookie assumptions.

```text
Browser ── HTTPS ──> pca-dashboard
                         │ private Railway network
                         ▼
                      pca-cloud-api ── private ──> PostgreSQL

Installed Agent ── HTTPS ──> pca-cloud-api public domain
```

## Runtime configuration

### `pca-cloud-api`

- `DATABASE_URL`: Railway PostgreSQL private variable reference.
- `BETTER_AUTH_SECRET`: randomly generated production secret, entered only in
  Railway Variables.
- `BETTER_AUTH_URL`: the public Dashboard origin. Auth responses reach this
  origin through the Dashboard proxy.
- `PORT`: supplied by Railway; the server must bind `0.0.0.0:$PORT`.
- `TRUSTED_PROXY_CLIENT_IP_HMAC_SECRET`: omitted for the initial deployment.
  The pairing-start limiter then uses its safe unattributed bucket rather than
  trusting arbitrary forwarding headers.

### `pca-dashboard`

- `CLOUD_API_INTERNAL_ORIGIN`: Railway-private API URL, used only by Next.js
  rewrites.
- `NEXT_PUBLIC_CLOUD_API_ORIGIN`: empty/unset, forcing Dashboard browser calls
  onto same-origin relative paths.

### Agent

The installed Agent receives the public API origin only through its intended
installation/runtime configuration. If it is absent, malformed, or the local
handoff is unavailable, the Agent stays unpaired and sensitive Collectors stay
disabled.

## Build and migration behavior

Both Railway services build from the monorepo root with frozen `pnpm` lockfile
installs. The API gains explicit build, migration, and start commands; the
Dashboard gains explicit build and start commands.

Before an API process serves traffic, its migration command takes a PostgreSQL
advisory lock, checks each immutable SQL file against `_pca_migrations`, then
applies missing files transactionally and records their SHA-256 checksum. A
checksum mismatch or failed migration terminates startup. Re-runs perform no
schema mutation. PostgreSQL is never exposed through a public application
endpoint.

## Acceptance

1. Railway marks PostgreSQL, API, and Dashboard healthy in Singapore.
2. The API migration ledger contains `0000` through `0004` with expected
   checksums; no raw Better Auth session-token column exists.
3. Dashboard registration/sign-in, Owner Workspace bootstrap, device read, and
   fixed configuration update work through same-origin proxy paths.
4. A deployed API health request does not reveal secrets or database URLs.
5. The installed Agent remains unpaired until the explicitly configured public
   API origin and the signed local handoff are both available.
6. Only after those checks does a manual Setup pairing, first control response,
   Dashboard audit read, and revoke/disable path count as live S1B acceptance.

## Failure handling

- Missing variable, failed migration, unavailable PostgreSQL, or invalid API
  origin causes startup/configuration failure or unpaired state; never a
  permissive fallback.
- A failed deployment is rolled back in Railway. Schema migration failures stop
  service start; immutable migration history is not edited in place.
- Credentials remain in Railway Variables/Keychain only and are never added to
  repository files, fixtures, SQLite, ordinary logs, or Dashboard views.
