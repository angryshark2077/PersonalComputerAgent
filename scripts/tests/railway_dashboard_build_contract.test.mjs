import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { fileURLToPath } from "node:url";

const repositoryRoot = fileURLToPath(new URL("../..", import.meta.url));
const dockerfile = await readFile(
  `${repositoryRoot}/deploy/railway/Dockerfile.dashboard`,
  "utf8",
);
const runbook = await readFile(
  `${repositoryRoot}/docs/runbooks/S1B_RAILWAY_DEPLOYMENT.md`,
  "utf8",
);

function lineNumber(text, exactLine) {
  const index = text.split("\n").findIndex((line) => line.trim() === exactLine);
  assert.notEqual(index, -1, `missing contract line: ${exactLine}`);
  return index + 1;
}

test("Dashboard Docker build exposes both proxy-origin args before next build", () => {
  const privateArg = lineNumber(dockerfile, "ARG CLOUD_API_INTERNAL_ORIGIN");
  const publicArg = lineNumber(dockerfile, "ARG NEXT_PUBLIC_CLOUD_API_ORIGIN");
  const privateEnvironment = lineNumber(
    dockerfile,
    "ENV CLOUD_API_INTERNAL_ORIGIN=${CLOUD_API_INTERNAL_ORIGIN}",
  );
  const publicEnvironment = lineNumber(
    dockerfile,
    "ENV NEXT_PUBLIC_CLOUD_API_ORIGIN=${NEXT_PUBLIC_CLOUD_API_ORIGIN}",
  );
  const build = lineNumber(dockerfile, "RUN pnpm --filter @pca/web-dashboard build");

  assert.ok(privateArg < privateEnvironment && privateEnvironment < build);
  assert.ok(publicArg < publicEnvironment && publicEnvironment < build);
});

test("Railway runbook configures the private origin before the first Dashboard build", () => {
  const cloudDeploy = lineNumber(
    runbook,
    "1. Deploy `pca-cloud-api` and confirm `/healthz` succeeds.",
  );
  const privateDomain = lineNumber(
    runbook,
    "2. Expose the API Railway private domain and record its private HTTP origin with port.",
  );
  const dashboardOrigin = lineNumber(
    runbook,
    "3. Set `CLOUD_API_INTERNAL_ORIGIN` on the undeployed `pca-dashboard` service.",
  );
  const dashboardBuild = lineNumber(
    runbook,
    "4. Only now connect the Dashboard source and allow its first build to start.",
  );

  assert.ok(cloudDeploy < privateDomain);
  assert.ok(privateDomain < dashboardOrigin);
  assert.ok(dashboardOrigin < dashboardBuild);
});

test("local Dashboard image verification passes both required build args", () => {
  assert.match(
    runbook,
    /docker build \\\n+  --build-arg CLOUD_API_INTERNAL_ORIGIN=http:\/\/pca-cloud-api\.railway\.internal:8080 \\\n+  --build-arg NEXT_PUBLIC_CLOUD_API_ORIGIN= \\\n+  -f deploy\/railway\/Dockerfile\.dashboard -t pca-dashboard:verify \./,
  );
});
