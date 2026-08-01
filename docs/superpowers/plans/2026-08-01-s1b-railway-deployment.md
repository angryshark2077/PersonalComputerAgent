# S1B Railway Deployment Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the S1B Cloud API and Dashboard deployable as two Railway Singapore services backed by private Railway PostgreSQL, with safe migrations and an explicit live-acceptance procedure.

**Architecture:** The API binds Railway's `PORT`, performs immutable Cloud SQL migrations in a pre-deploy command, and exposes an unauthenticated non-secret health endpoint. The Dashboard is the browser origin and proxies `/api/auth/*` and `/v1/*` to the API over Railway private networking, keeping Better Auth cookies same-origin. The Agent calls only the API public HTTPS domain and remains fail-closed until that origin and its local handoff are configured.

**Tech Stack:** Node 22, pnpm 9, TypeScript, Hono with `@hono/node-server`, Next.js 16, PostgreSQL/`pg`, Railway Dockerfile builds and pre-deploy commands.

## Global Constraints

- Deploy PostgreSQL, `pca-cloud-api`, and `pca-dashboard` only in Railway Southeast Asia / Singapore.
- PostgreSQL is private to Railway; never configure a public database TCP endpoint.
- `DATABASE_URL`, `BETTER_AUTH_SECRET`, and any proxy signing secret exist only as Railway Variables; no `.env`, fixture, log, or repository value may contain them.
- Browser auth/API traffic is same-origin at the Dashboard. `CLOUD_API_INTERNAL_ORIGIN` is server-only and must never use a `NEXT_PUBLIC_` name.
- The API public domain is exclusively the installed Agent's HTTPS origin. Missing/malformed Agent origin or unavailable local handoff remains unpaired and leaves sensitive Collectors disabled.
- Cloud migrations remain immutable. Startup/pre-deploy must fail on checksum mismatch or migration error; it must not edit prior SQL files or silently skip a migration.
- No automatic Railway/GitHub actions, deploy, secret write, or production claim belongs to this plan. The operator runbook supplies those UI steps.

---

### Task 1: Add a production API entrypoint, health check, and immutable migration runner

**Files:**
- Modify: `apps/cloud-api/package.json`
- Modify: `apps/cloud-api/src/index.ts`
- Create: `apps/cloud-api/src/server.ts`
- Create: `apps/cloud-api/src/migrate.ts`
- Create: `apps/cloud-api/src/test/server.test.ts`
- Create: `apps/cloud-api/src/test/migrate.postgres.test.ts`
- Modify: `apps/cloud-api/src/test/session-storage.postgres.test.ts`

**Interfaces:**
- Produces `GET /healthz -> {"status":"ok"}` with no configuration, credential, database, or version detail.
- Produces `runCloudMigrations(connectionString: string, migrationDirectory: string): Promise<void>`.
- Produces package commands `build`, `start`, and `migrate`; `start` binds `0.0.0.0:${PORT}` and `migrate` exits non-zero on any unsafe schema condition.

- [ ] **Step 1: Write failing API production-entry tests**

```ts
test("healthz is public and contains no deployment configuration", async () => {
  const app = createProductionApp(validProductionEnvironment());
  const response = await app.request("http://pca.invalid/healthz");
  assert.equal(response.status, 200);
  assert.deepEqual(await response.json(), { status: "ok" });
});

test("server requires a numeric Railway port", () => {
  assert.throws(() => parseListenPort("invalid"), /PORT/);
});
```

- [ ] **Step 2: Run the API production-entry test to verify it fails**

Run: `pnpm --filter @pca/cloud-api test -- --test-name-pattern='healthz|Railway port'`

Expected: FAIL because the route and server entrypoint do not exist.

- [ ] **Step 3: Write failing real-PostgreSQL migration-runner tests**

Use the existing temporary PostgreSQL helper pattern from
`apps/cloud-api/src/test/session-storage.postgres.test.ts`.

```ts
await runCloudMigrations(connectionString, migrationDirectory);
await runCloudMigrations(connectionString, migrationDirectory);
assert.deepEqual(await migrationIds(pool), ["0000", "0001", "0002", "0003", "0004"]);
await assert.rejects(
  () => runCloudMigrations(connectionString, directoryWithChanged0001),
  /checksum mismatch: 0001/,
);
```

The test must assert `auth_sessions` has `session_token_hash` and not
`session_token` after both fresh and repeated execution.

- [ ] **Step 4: Implement the minimal API runtime and runner**

Add `@hono/node-server` and implement:

```ts
export function parseListenPort(value: string | undefined): number {
  const port = Number(value);
  if (!Number.isInteger(port) || port < 1 || port > 65_535) throw new Error("invalid PORT");
  return port;
}

const app = createProductionApp(process.env);
serve({ fetch: app.fetch, hostname: "0.0.0.0", port: parseListenPort(process.env.PORT) });
```

Add `GET /healthz` before authenticated routes. It always returns exactly
`{ status: "ok" }`.

`runCloudMigrations` must sort the five committed SQL files, acquire a
transaction-scoped PostgreSQL advisory lock, bootstrap `_pca_migrations` with
`0000`, then for each file compute SHA-256. It records a missing migration only
after its SQL completes, skips matching completed migrations, and throws on a
different recorded checksum or a non-completed record. It must release the
client in `finally`.

Add scripts:

```json
"build": "tsc --noEmit",
"start": "tsx src/server.ts",
"migrate": "tsx src/migrate.ts"
```

- [ ] **Step 5: Run focused API, migration, and type checks**

Run:

```bash
pnpm --filter @pca/cloud-api test
pnpm --filter @pca/cloud-api typecheck
```

Expected: all API tests including temporary PostgreSQL migration replay pass.

- [ ] **Step 6: Commit the API deployability unit**

```bash
git add apps/cloud-api/package.json apps/cloud-api/src/index.ts apps/cloud-api/src/server.ts \
  apps/cloud-api/src/migrate.ts apps/cloud-api/src/test/server.test.ts \
  apps/cloud-api/src/test/migrate.postgres.test.ts apps/cloud-api/src/test/session-storage.postgres.test.ts \
  pnpm-lock.yaml
git commit -m "feat: prepare Cloud API for Railway"
```

### Task 2: Make the Dashboard a same-origin Railway proxy

**Files:**
- Modify: `apps/web-dashboard/package.json`
- Create: `apps/web-dashboard/next.config.ts`
- Create: `apps/web-dashboard/src/app/healthz/route.ts`
- Modify: `apps/web-dashboard/src/lib/api.ts`
- Create: `apps/web-dashboard/test/railway-proxy.test.ts`

**Interfaces:**
- Consumes `CLOUD_API_INTERNAL_ORIGIN` only on the Dashboard server process.
- Produces Next rewrites for `/api/auth/:path*` and `/v1/:path*` to that private
  origin and `GET /healthz -> {"status":"ok"}`.
- Produces `cloudApiOrigin() === ""` in a production Dashboard browser bundle;
  a non-empty public origin is rejected at build/start validation.

- [ ] **Step 1: Write failing rewrite and public-origin tests**

```ts
test("Railway proxy rewrites auth and control paths to the private API", async () => {
  const config = await createNextConfig("http://pca-cloud-api.railway.internal:8080");
  assert.deepEqual(await config.rewrites(), [
    { source: "/api/auth/:path*", destination: "http://pca-cloud-api.railway.internal:8080/api/auth/:path*" },
    { source: "/v1/:path*", destination: "http://pca-cloud-api.railway.internal:8080/v1/:path*" },
  ]);
});

test("Dashboard rejects a public browser Cloud API origin", () => {
  assert.throws(() => validateDashboardEnvironment({ NEXT_PUBLIC_CLOUD_API_ORIGIN: "https://api.invalid" }), /same-origin/);
});
```

- [ ] **Step 2: Run the Dashboard test to verify it fails**

Run: `pnpm --filter @pca/web-dashboard test -- --test-name-pattern='Railway proxy|public browser'`

Expected: FAIL because no Next configuration or environment validation exists.

- [ ] **Step 3: Implement proxy and production scripts**

Implement `createNextConfig(internalOrigin)` with URL validation requiring
`http`, a non-empty host ending in `.railway.internal` (or an explicitly
provided private host for tests), and no path/query/fragment. `next.config.ts`
reads only `CLOUD_API_INTERNAL_ORIGIN`.

Add `app/healthz/route.ts`:

```ts
export function GET(): Response {
  return Response.json({ status: "ok" });
}
```

Add Dashboard package scripts:

```json
"build": "next build",
"start": "next start"
```

Keep `NEXT_PUBLIC_CLOUD_API_ORIGIN` absent in production so existing Dashboard
calls use relative URLs. Do not add CORS headers or cross-origin credentials.

- [ ] **Step 4: Run Dashboard tests, typecheck, and production build**

Run:

```bash
pnpm --filter @pca/web-dashboard test
pnpm --filter @pca/web-dashboard typecheck
pnpm --filter @pca/web-dashboard build
```

Expected: proxy configuration tests pass and Next production build succeeds.

- [ ] **Step 5: Commit the Dashboard deployability unit**

```bash
git add apps/web-dashboard/package.json apps/web-dashboard/next.config.ts \
  apps/web-dashboard/src/app/healthz/route.ts apps/web-dashboard/src/lib/api.ts \
  apps/web-dashboard/test/railway-proxy.test.ts
git commit -m "feat: proxy Dashboard API through Railway private network"
```

### Task 3: Add Railway build definitions and an operator-safe deployment runbook

**Files:**
- Create: `deploy/railway/Dockerfile.cloud-api`
- Create: `deploy/railway/Dockerfile.dashboard`
- Create: `docs/runbooks/S1B_RAILWAY_DEPLOYMENT.md`
- Create: `scripts/verify-railway-deployment.sh`
- Create: `scripts/tests/test_verify_railway_deployment.sh`
- Modify: `README.md`

**Interfaces:**
- Consumes Railway's root Git context and `RAILWAY_DOCKERFILE_PATH` service
  variable with `/deploy/railway/Dockerfile.cloud-api` or
  `/deploy/railway/Dockerfile.dashboard`.
- Consumes `DATABASE_URL`, `BETTER_AUTH_SECRET`, `BETTER_AUTH_URL`, and
  `CLOUD_API_INTERNAL_ORIGIN` only from Railway Variables.
- Produces `scripts/verify-railway-deployment.sh <dashboard-origin> <api-origin>`;
  it checks public health JSON and rejects exposed `DATABASE_URL`, token, or
  Keychain wording in those responses.

- [ ] **Step 1: Write failing deployment verifier tests**

```bash
PCA_RAILWAY_CURL='fixture-curl' scripts/verify-railway-deployment.sh \
  https://dashboard.example https://api.example
# fixture responses {"status":"ok"} -> exits 0
# fixture response containing "DATABASE_URL" -> exits non-zero
```

The shell test must not make a network request and must assert a missing origin
or non-HTTPS public origin exits non-zero.

- [ ] **Step 2: Run the verifier test to verify it fails**

Run: `bash scripts/tests/test_verify_railway_deployment.sh`

Expected: FAIL because the deployment verifier does not exist.

- [ ] **Step 3: Implement Dockerfiles, verifier, and runbook**

Both Dockerfiles use Node 22, enable Corepack, copy the root monorepo,
run `pnpm install --frozen-lockfile`, and run the service-specific build.
The API image exposes the `start` command; Railway's pre-deploy command is
`pnpm --filter @pca/cloud-api migrate`. The Dashboard image exposes its
`start` command.

The runbook must give exact Railway UI actions:

1. Push the reviewed branch to a private GitHub repository.
2. Add two GitHub services in the existing project, rename them
   `pca-cloud-api` and `pca-dashboard`, set both Singapore, retain root
   directory `/`, and set their respective `RAILWAY_DOCKERFILE_PATH`.
3. Generate a public domain for each service only after a successful build.
4. Add `DATABASE_URL=${{Postgres.DATABASE_URL}}` to the API; enter and seal
   `BETTER_AUTH_SECRET`; set `BETTER_AUTH_URL` to the Dashboard HTTPS domain.
5. Add `CLOUD_API_INTERNAL_ORIGIN` to Dashboard with the API private-domain
   reference and port using Railway's variable autocomplete; do not create a
   public browser API-origin variable.
6. Configure API pre-deploy command, health paths `/healthz`, and deploy in
   dependency order: API then Dashboard.
7. Run the verifier with the two generated HTTPS domains; manually exercise
   registration/sign-in and finally the Setup pairing/revoke checklist.

The runbook must mark any missing signed local handoff/Keychain ACL as a
fail-closed live-pairing blocker, not a deployment success.

- [ ] **Step 4: Run verifier tests and static deployment checks**

Run:

```bash
bash scripts/tests/test_verify_railway_deployment.sh
docker build -f deploy/railway/Dockerfile.cloud-api -t pca-cloud-api:verify .
docker build -f deploy/railway/Dockerfile.dashboard -t pca-dashboard:verify .
git diff --check
```

Expected: verifier tests and both Docker builds pass. If Docker is unavailable,
record the exact command/error in the runbook verification section and still
run the package build commands from Tasks 1 and 2.

- [ ] **Step 5: Commit the deployment/operator unit**

```bash
git add deploy/railway docs/runbooks/S1B_RAILWAY_DEPLOYMENT.md \
  scripts/verify-railway-deployment.sh scripts/tests/test_verify_railway_deployment.sh README.md
git commit -m "feat: add Railway deployment runbook"
```

### Task 4: Execute the deployment-preparation gate and record exact live handoff status

**Files:**
- Modify: `scripts/verify-full.sh`
- Modify: `ARCHITECTURE.md`
- Modify: `SECURITY.md`
- Modify: `PERFORMANCE.md`
- Modify: `tasks/S1B_CLOUD_CONTROL_PLANE.md`
- Modify: `tasks/BACKLOG_V0.md`
- Create: `docs/data/S1B_RAILWAY_DEPLOYMENT_FIELDS.md`

**Interfaces:**
- Produces an expanded repository verification command which exercises both
  deployment-manifest tests and all prior S1B gates.
- Produces a precise boundary: deployability is verified locally; live Railway
  and live Setup-to-Agent handoff remain unverified until the operator runbook
  completes.

- [ ] **Step 1: Add a failing full-gate expectation for deployment verifier tests**

Add a test that invokes `./scripts/verify-full.sh` with a fixture verifier and
expects the new deployment test script to execute before final success.

- [ ] **Step 2: Run the full gate to verify the missing deployment check**

Run: `PATH="/Users/jacob/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" PCA_DISABLE_TOOLCHAIN_FALLBACK=1 ./scripts/verify-full.sh`

Expected: FAIL because the deployment verifier is not yet part of the gate.

- [ ] **Step 3: Wire the verified deployment checks and update authoritative docs**

Add `bash scripts/tests/test_verify_railway_deployment.sh` to
`verify-full.sh`. Update architecture/security/performance/task/data documents
with the exact service names, private/public origin boundary, variable names,
migration behavior, and the fact that live Railway pairing remains unverified.
Do not add secret values, domains, or copied Railway connection strings.

- [ ] **Step 4: Run the complete local release gate**

Run:

```bash
PATH="/Users/jacob/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" \
  PCA_DISABLE_TOOLCHAIN_FALLBACK=1 ./scripts/verify-full.sh
git diff --check
git status --short
```

Expected: `FULL VERIFICATION PASSED`, a clean diff check, and no untracked
secret/config files.

- [ ] **Step 5: Commit the final Railway-preparation evidence**

```bash
git add scripts/verify-full.sh ARCHITECTURE.md SECURITY.md PERFORMANCE.md \
  tasks/S1B_CLOUD_CONTROL_PLANE.md tasks/BACKLOG_V0.md docs/data/S1B_RAILWAY_DEPLOYMENT_FIELDS.md
git commit -m "docs: verify Railway deployment preparation"
```

## Plan self-review

- Scope coverage: Tasks 1-3 implement API runtime, private Dashboard proxy,
  Railway build/variables/runbook, migration safety, and live verification
  procedure. Task 4 adds the local release proof and accurate documentation.
- No application secret or Railway action is automated; the operator performs
  all external account/domain/variable writes through the runbook.
- The plan deliberately treats actual Setup-to-Agent local-handoff/ACL proof
  as a post-deploy acceptance condition instead of claiming it is implemented
  by Docker configuration.
