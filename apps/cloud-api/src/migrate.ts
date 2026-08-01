import { createHash } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { Client } from "pg";

const migrationFileName = /^(\d{4})_.+\.sql$/;

export async function runCloudMigrations(
  connectionString: string,
  migrationDirectory: string,
): Promise<void> {
  const migrations = await loadMigrations(migrationDirectory);
  const client = new Client({ connectionString });
  await client.connect();
  try {
    await client.query("BEGIN");
    try {
      await client.query("SELECT pg_advisory_xact_lock(hashtext('pca-cloud-migrations'))");
      for (const migration of migrations) {
        const existing = await migrationRecord(client, migration.id);
        if (existing !== undefined) {
          if (existing.checksum !== migration.checksum) {
            throw new Error(`checksum mismatch: ${migration.id}`);
          }
          if (existing.status !== "completed") {
            throw new Error(`migration not completed: ${migration.id}`);
          }
          continue;
        }

        await client.query(migration.sql);
        await client.query(
          `INSERT INTO _pca_migrations (id, checksum, app_version, started_at, completed_at, status)
           VALUES ($1, $2, $3, now(), now(), 'completed')`,
          [migration.id, migration.checksum, "cloud-api"],
        );
      }
      await client.query("COMMIT");
    } catch (error) {
      await client.query("ROLLBACK").catch(() => undefined);
      throw error;
    }
  } finally {
    await client.end();
  }
}

async function loadMigrations(migrationDirectory: string) {
  const files = (await readdir(migrationDirectory))
    .map((file) => ({ file, match: migrationFileName.exec(file) }))
    .filter((entry): entry is { file: string; match: RegExpExecArray } => entry.match !== null)
    .sort((left, right) => left.file.localeCompare(right.file));
  if (
    files.length === 0 ||
    files.some((entry, index) => entry.match[1] !== String(index).padStart(4, "0"))
  ) {
    throw new Error("expected contiguous committed cloud migrations beginning at 0000");
  }
  return Promise.all(
    files.map(async ({ file, match }) => {
      const sql = await readFile(resolve(migrationDirectory, file), "utf8");
      const id = match[1];
      if (id === undefined) throw new Error(`invalid migration filename: ${file}`);
      return { id: id!, sql, checksum: createHash("sha256").update(sql).digest("hex") };
    }),
  );
}

async function migrationRecord(client: Client, id: string) {
  if (id === "0000") {
    const exists = await client.query<{ exists: boolean }>(
      "SELECT to_regclass('public._pca_migrations') IS NOT NULL AS exists",
    );
    if (!exists.rows[0]?.exists) return undefined;
  }
  const result = await client.query<{ checksum: string; status: string }>(
    "SELECT checksum, status FROM _pca_migrations WHERE id = $1",
    [id],
  );
  return result.rows[0];
}

const moduleDirectory = dirname(fileURLToPath(import.meta.url));
const defaultMigrationDirectory = resolve(moduleDirectory, "../../../packages/db-cloud/migrations");

if (process.env.NODE_TEST_CONTEXT === undefined) {
  const connectionString = process.env.DATABASE_URL;
  if (connectionString === undefined || connectionString.length === 0) {
    throw new Error("missing required configuration: DATABASE_URL");
  }
  await runCloudMigrations(connectionString, defaultMigrationDirectory);
}
