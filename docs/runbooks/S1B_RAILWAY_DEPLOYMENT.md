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

## Create the application services without starting Dashboard build

1. In the Railway project, select **New** → **GitHub Repo**, choose the private
   repository, and create `pca-cloud-api`. Set its region to Singapore, leave
   **Root Directory** as `/`, and set `RAILWAY_DOCKERFILE_PATH` to
   `/deploy/railway/Dockerfile.cloud-api`.
2. Create `pca-dashboard` as an empty service with no GitHub source connected.
   Set its region to Singapore and keep **Root Directory** as `/`. Reserve its
   public HTTPS domain for `BETTER_AUTH_URL`, but do not start a Dashboard build.
3. On the empty Dashboard service, set `RAILWAY_DOCKERFILE_PATH` to
   `/deploy/railway/Dockerfile.dashboard`. Leave
   `NEXT_PUBLIC_CLOUD_API_ORIGIN` unset; it is forbidden.

## Configure Cloud and the required first-build order

In `pca-cloud-api` → **Variables**, add
`DATABASE_URL=${{Postgres.DATABASE_URL}}` using Railway's PostgreSQL reference.
Enter `BETTER_AUTH_SECRET` directly in the UI and seal it; never copy it into
the repository, terminal history, logs, or this runbook. Set `BETTER_AUTH_URL`
to the reserved Dashboard HTTPS domain. In **Settings** → **Deploy**, set the
pre-deploy command to `pnpm --filter @pca/cloud-api migrate` and the health
check to `/healthz`.

1. Deploy `pca-cloud-api` and confirm `/healthz` succeeds.
2. Expose the API Railway private domain and record its private HTTP origin with port.
3. Set `CLOUD_API_INTERNAL_ORIGIN` on the undeployed `pca-dashboard` service.
4. Only now connect the Dashboard source and allow its first build to start.

For step 2, use the Cloud service's Railway private-domain reference and its
listening port, forming a root `http://<service>.railway.internal:<port>`
origin. For step 3, select that reference through Railway variable autocomplete;
do not type or copy a secret. Railway supplies the declared Docker build arg to
`next build`, where the existing private-origin validation rejects missing or
invalid values and rejects any configured `NEXT_PUBLIC_CLOUD_API_ORIGIN`.

After the Dashboard first build succeeds, set its health check to `/healthz`
and generate/confirm the API public HTTPS domain for the installed Agent. If the
private origin was absent at build time, the build must fail; setting it only as
a later runtime variable cannot repair that image, so correct the variable and
rebuild.

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
node --test scripts/tests/railway_dashboard_build_contract.test.mjs
docker build -f deploy/railway/Dockerfile.cloud-api -t pca-cloud-api:verify .
docker build \
  --build-arg CLOUD_API_INTERNAL_ORIGIN=http://pca-cloud-api.railway.internal:8080 \
  --build-arg NEXT_PUBLIC_CLOUD_API_ORIGIN= \
  -f deploy/railway/Dockerfile.dashboard -t pca-dashboard:verify .
pnpm --filter @pca/cloud-api build
CLOUD_API_INTERNAL_ORIGIN=http://pca-cloud-api.railway.internal:8080 \
  env -u NEXT_PUBLIC_CLOUD_API_ORIGIN pnpm --filter @pca/web-dashboard build
git diff --check
```

Record the command and exact error here if Docker is unavailable; Docker
unavailability is not permission to claim the image builds passed. The package
builds and verifier test still need to run locally.

### 2026-08-01 local result

Docker was unavailable in the preparation environment, so neither corrected
image build above executed and both image builds remain unverified. The local
availability check returned:

```text
docker: command not found
```

The commands in **Local verification record** are the authoritative commands
to run when Docker becomes available; do not convert this fixture-only contract
proof into an image-build claim.
