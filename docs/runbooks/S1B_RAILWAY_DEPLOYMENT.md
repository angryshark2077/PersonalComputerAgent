# S1B Railway deployment

This is an operator procedure, not a deployment record. It neither creates
Railway resources nor proves a live service, pairing, or local credential
handoff. Keep PostgreSQL private to Railway and enter every secret only in the
Railway Variables UI.

## Preconditions

- Review the branch and push it to a private GitHub repository. Do not include
  `.env` files, connection strings, domains, or secret values in Git.
- Open the existing Railway project that contains its private PostgreSQL
  service. Use Singapore (Southeast Asia) for every service in this procedure.
- Keep the repository root directory as `/` for both application services.

## Create the application services

1. In the Railway project, select **New** → **GitHub Repo**, choose the private
   repository, and create the first service. Rename it to `pca-cloud-api`, set
   its region to Singapore, and leave **Root Directory** as `/`.
2. In that service's **Variables** tab, set
   `RAILWAY_DOCKERFILE_PATH` to `/deploy/railway/Dockerfile.cloud-api`.
3. Repeat **New** → **GitHub Repo** for a second service. Rename it to
   `pca-dashboard`, set its region to Singapore, and leave **Root Directory**
   as `/`.
4. Set its `RAILWAY_DOCKERFILE_PATH` to
   `/deploy/railway/Dockerfile.dashboard`.
5. Wait for each initial build to succeed before selecting **Settings** →
   **Networking** → **Generate Domain**. Generate one public domain per
   service only after that service has a successful build.

## Configure variables and deploy order

1. In `pca-cloud-api` → **Variables**, add
   `DATABASE_URL=${{Postgres.DATABASE_URL}}` using Railway's PostgreSQL
   reference. Enter `BETTER_AUTH_SECRET` directly in the UI and seal it; never
   copy it into the repository, terminal history, logs, or this runbook. Set
   `BETTER_AUTH_URL` to the generated Dashboard HTTPS domain.
2. In `pca-dashboard` → **Variables**, add `CLOUD_API_INTERNAL_ORIGIN` with
   Railway's variable autocomplete: select the API service's private-domain
   reference and port, forming its private HTTP origin. Do not create any
   browser-facing API-origin variable, especially no `NEXT_PUBLIC_` API origin.
3. In `pca-cloud-api` → **Settings** → **Deploy**, set the pre-deploy command
   to `pnpm --filter @pca/cloud-api migrate`. In each service's health-check
   setting, use `/healthz`.
4. Deploy `pca-cloud-api` first. Confirm its migration/pre-deploy phase and
   `/healthz` complete successfully. Then deploy `pca-dashboard`. Its
   `/healthz` remains `503 not_ready` until the private API origin is present
   and valid; do not bypass that readiness failure.

## Public verification and acceptance

From a machine with the repository checkout, run the verifier with the two
generated public HTTPS domains (Dashboard first, API second):

```bash
scripts/verify-railway-deployment.sh https://<dashboard-domain> https://<api-domain>
```

It requests only each public `/healthz`, requires JSON with `status: "ok"`,
and fails without printing a response if it sees `DATABASE_URL`, `token`, or
`Keychain` wording.

Then manually register an account, sign in, and verify that browser auth and
Dashboard API requests remain same-origin. Complete the Setup pairing and
revoke checklist in [`S1B_PAIRING_REPAIR.md`](S1B_PAIRING_REPAIR.md).

Deployment health is not live-pairing acceptance. A missing signed local
handoff or missing Keychain ACL is a fail-closed live-pairing blocker: leave
the Agent unpaired and sensitive Collectors disabled. Do not report the
deployment as a pairing success until that checklist has completed.

## Local verification record

Before an operator deploys, run:

```bash
bash scripts/tests/test_verify_railway_deployment.sh
docker build -f deploy/railway/Dockerfile.cloud-api -t pca-cloud-api:verify .
docker build -f deploy/railway/Dockerfile.dashboard -t pca-dashboard:verify .
pnpm --filter @pca/cloud-api build
pnpm --filter @pca/web-dashboard build
git diff --check
```

Record the command and exact error here if Docker is unavailable; Docker
unavailability is not permission to claim the image builds passed. The package
builds and verifier test still need to run locally.

### 2026-08-01 local result

Docker was unavailable in the preparation environment, so image builds remain
unverified. The exact attempted commands and errors were:

```text
docker build -f deploy/railway/Dockerfile.cloud-api -t pca-cloud-api:verify .
zsh: command not found: docker

docker build -f deploy/railway/Dockerfile.dashboard -t pca-dashboard:verify .
zsh: command not found: docker
```
