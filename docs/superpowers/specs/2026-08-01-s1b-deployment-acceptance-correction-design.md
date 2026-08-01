# S1B Deployment Acceptance Correction Design

**Status:** approved design

## Problem

The S1B/Railway final review found two release-blocking false proofs:

1. The process test exercised callback, Agent state, Dashboard audit, and
   revocation as independent fakes instead of one shared Cloud state flow.
2. Next.js rewrites are produced at build time, while Dashboard health was
   checking an environment value at runtime. A build without the private API
   origin could later report healthy even though no proxy route exists.

## Scope

- Make `CLOUD_API_INTERNAL_ORIGIN` mandatory while building the production
  Dashboard. Its validated value is the sole source for the generated
  `/api/auth/*` and `/v1/*` rewrites.
- Make Dashboard health a statement about the built proxy configuration, not a
  mutable runtime environment value.
- Replace the test-only fragmented S1B process flow with one local Cloud test
  service that owns pairing session/code/device/config/revocation state and is
  called by Setup callback handoff, Agent HTTP clients, and Dashboard clients.
- Keep canary scans over stdout, stderr, JSON status, temporary SQLite/WAL,
  and fixtures.

## Exclusions

- No Railway deployment, account/variable/secret action, custom domain, or
  database modification.
- No production local IPC transport redesign, Collector source, Event sync, or
  remote command.
- No weakening of migration, secret, or full-gate verification.

## Dashboard build configuration

`next.config.ts` reads `CLOUD_API_INTERNAL_ORIGIN` during `next build`. In a
production build it must be a root `http://` Railway private origin with no
path/query/fragment and a `.railway.internal` host. Missing or invalid input
terminates the build before a deployment can be created. Local/test builds may
inject a valid test private origin explicitly.

The generated rewrites are the readiness proof. `GET /healthz` returns
`200 {"status":"ok"}` only from a build that successfully generated those
rewrites. There is no runtime fallback and no runtime re-read of
`CLOUD_API_INTERNAL_ORIGIN`. The Dashboard browser continues using relative
same-origin API paths; no CORS or public browser API origin is introduced.

## Shared process acceptance flow

The test harness starts one in-process Hono Cloud service backed by a single
test state object. It provides only the S1B endpoints needed for the test:

```text
pairing start -> authorization callback -> one-time exchange
-> heartbeat/control snapshot -> Dashboard device/audit reads
-> Dashboard revoke -> next heartbeat reports revoked
```

The spawned Agent helper receives only the callback result and test API base
URL. It generates/holds the PKCE proof, performs the actual exchange against
that test service, stores the returned synthetic credential in a test-only
file-backed Keychain double, and writes non-secret pairing state to a temporary
SQLite database. It then uses the same service for its control request.

The Dashboard test client authorizes and reads/revokes through the same service
state. The revoke handler marks that device revoked; the Agent's next real
test-client request must receive the revoked response and execute the existing
fail-closed cleanup path. Static snapshots, hard-coded authorization codes,
direct credential stdin injection, and always-revoked control clients are not
allowed in this acceptance path.

## Assertions

The one process test must prove all of the following:

1. The callback's one-time code and state create exactly one device credential
   record through the shared service.
2. The first Agent control response applies a newer revision from the shared
   Dashboard-controlled configuration.
3. Dashboard device and audit reads show the same device/revision and owner
   configuration change.
4. Dashboard revoke makes the next Agent control call fail as revoked and
   atomically removes pairing state while disabling Network and WeChat
   configuration.
5. Runtime-generated credential and message-body canaries do not occur in
   stdout, stderr, JSON status, SQLite or WAL/SHM bytes, or fixture source.

## Verification and release boundary

The full local gate includes the process acceptance script and Dashboard
production build with an explicit test private origin. A missing private origin
has its own negative build test. Docker images and live Railway remain marked
unverified until the operator runbook is executed; this correction itself does
not make a live-deployment claim.
