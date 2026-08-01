# S1B Railway Deployment Fields

**Status:** repository deployment preparation is locally verified. No Railway
project, service, domain, Variable, PostgreSQL instance, browser session, or
live Setup-to-Agent pairing has been created or verified from this repository.

## Service and origin boundary

| Component | Railway name / visibility | Required origin boundary |
|---|---|---|
| PostgreSQL | private Railway service | No public database TCP endpoint |
| Cloud API | `pca-cloud-api`; public health/API service | Its public HTTPS origin is for the installed Agent only |
| Dashboard | `pca-dashboard`; public browser service | The browser's same-origin `/api/auth/*` and `/v1/*` paths |
| Dashboard-to-API | private Railway network | Dashboard server rewrites to API private HTTP origin; browser code never receives it |

The operator must configure all three services in Railway Singapore (Southeast
Asia) when following the deployment runbook. The operator keeps root directory
`/` and selects the Dockerfile path through the service Variable
`RAILWAY_DOCKERFILE_PATH`: `/deploy/railway/Dockerfile.cloud-api` or
`/deploy/railway/Dockerfile.dashboard`.

## Deployment variables

| Variable | Service | Value class / rule |
|---|---|---|
| `DATABASE_URL` | `pca-cloud-api` | Railway PostgreSQL reference only; secret and never public |
| `BETTER_AUTH_SECRET` | `pca-cloud-api` | generated and sealed in Railway Variables only; never repository or log data |
| `BETTER_AUTH_URL` | `pca-cloud-api` | public Dashboard HTTPS origin |
| `PORT` | `pca-cloud-api` runtime | Railway-assigned numeric listen port; server binds `0.0.0.0:${PORT}` |
| `CLOUD_API_INTERNAL_ORIGIN` | `pca-dashboard` | server-only API private HTTP origin; no path, query, or fragment |
| `RAILWAY_DOCKERFILE_PATH` | each application service | service-specific root-context Dockerfile path above |

No `NEXT_PUBLIC_CLOUD_API_ORIGIN` is allowed in a production Dashboard build.
Do not commit variable values, connection strings, domains, tokens, or
Keychain material.

## Health and migration behavior

- Both public services expose `/healthz` with exactly `{ "status": "ok" }`
  when ready. The Dashboard returns `503` with `{ "status": "not_ready" }`
  when `CLOUD_API_INTERNAL_ORIGIN` is missing or invalid, so Railway cannot
  admit a runtime that has no private API route. `scripts/verify-railway-deployment.sh`
  requires public HTTPS origins and rejects health bodies containing
  `DATABASE_URL`, token, or Keychain wording.
- The API pre-deploy command is `pnpm --filter @pca/cloud-api migrate`.
  The migration runner uses a transaction-scoped PostgreSQL advisory lock and
  records SHA-256 checksums in `_pca_migrations` only after each SQL migration
  succeeds.
- Cloud migrations `0000` through `0004` are ordered and immutable. A missing,
  incomplete, changed-checksum, or failed migration exits non-zero; it never
  rewrites a committed SQL migration or silently skips it.

## Live acceptance remains unverified

The local full gate runs the offline deployment-verifier test, but does not
contact Railway. The operator must complete
`docs/runbooks/S1B_RAILWAY_DEPLOYMENT.md`, then the pairing/revoke checks in
`docs/runbooks/S1B_PAIRING_REPAIR.md`. Until a real API HTTPS origin, signed
Setup-to-Agent local transport, and restricted Keychain ACL are installed and
tested, the Setup bridge remains unavailable, the Agent is unpaired, and
sensitive Collectors remain disabled.
