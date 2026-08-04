import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

import { DrizzleControlRepository, type CommunicationMessageEventRecord } from "@pca/db-cloud/src/repository.js";
import { cloudSchema } from "@pca/db-cloud/src/schema.js";
import { drizzle } from "drizzle-orm/node-postgres";
import { Pool } from "pg";

import { runCloudMigrations } from "../migrate.js";

const postgresUser = "pca_migration_test";
const testDirectory = dirname(fileURLToPath(import.meta.url));
const committedMigrationDirectory = resolve(testDirectory, "../../../../packages/db-cloud/migrations");

test("PostgreSQL migrations replay safely and create private communication projections", async () => {
  const postgres = await startTemporaryPostgres();
  const pool = new Pool({ connectionString: postgres.connectionString });
  try {
    await runCloudMigrations(postgres.connectionString, committedMigrationDirectory);
    await assertHashedSessionSchema(pool);
    await assertSystemEventSchema(pool);
    await assertCommunicationEventSchema(pool);
    await assertCommunicationProjectionSchema(pool);
    await assertCommunicationObjectSchema(pool);
    await assertCommunicationFileProjectionSchema(pool);
    await assertCommunicationProjectionBackfill(pool);
    await assertCommunicationMediaUpgrade(pool);
    await assertDeviceLocationSchema(pool);

    await runCloudMigrations(postgres.connectionString, committedMigrationDirectory);
    await assertHashedSessionSchema(pool);
    await assertSystemEventSchema(pool);
    await assertCommunicationEventSchema(pool);
    await assertCommunicationProjectionSchema(pool);
    await assertCommunicationObjectSchema(pool);
    await assertDeviceLocationSchema(pool);
    assert.deepEqual(await migrationIds(pool), ["0000", "0001", "0002", "0003", "0004", "0005", "0006", "0007", "0008", "0009", "0010", "0011", "0012", "0013", "0014", "0015", "0016", "0017", "0018", "0019"]);
  } finally {
    await pool.end();
    await postgres.stop();
  }
});

test("PostgreSQL migrations reject a changed completed migration", async () => {
  const postgres = await startTemporaryPostgres();
  const pool = new Pool({ connectionString: postgres.connectionString });
  const changedDirectory = await copyMigrationsWithChanged0001();
  try {
    await runCloudMigrations(postgres.connectionString, committedMigrationDirectory);
    await assert.rejects(
      () => runCloudMigrations(postgres.connectionString, changedDirectory),
      /checksum mismatch: 0001/,
    );
  } finally {
    await pool.end();
    await postgres.stop();
    await rm(changedDirectory, { recursive: true, force: true });
  }
});

async function assertHashedSessionSchema(pool: Pool) {
  const result = await pool.query<{ column_name: string }>(
    `SELECT column_name FROM information_schema.columns
     WHERE table_schema = 'public' AND table_name = 'auth_sessions'
     ORDER BY column_name`,
  );
  const columns = result.rows.map((row) => row.column_name);
  assert.ok(columns.includes("session_token_hash"));
  assert.ok(!columns.includes("session_token"));
}

async function assertSystemEventSchema(pool: Pool) {
  const result = await pool.query<{ exists: boolean }>(
    "SELECT to_regclass('public.system_events') IS NOT NULL AS exists",
  );
  assert.equal(result.rows[0]?.exists, true);
}

async function assertDeviceLocationSchema(pool: Pool) {
  const columns = await pool.query<{ column_name: string }>(
    `SELECT column_name FROM information_schema.columns
     WHERE table_schema = 'public'
       AND table_name = 'device_heartbeats'
       AND column_name LIKE 'network_location_%'
     ORDER BY column_name`,
  );
  assert.deepEqual(columns.rows.map((row) => row.column_name), [
    "network_location_horizontal_accuracy_meters",
    "network_location_latitude",
    "network_location_longitude",
    "network_location_observed_at",
  ]);
  const constraints = await pool.query<{ constraint_name: string }>(
    `SELECT constraint_name FROM information_schema.table_constraints
     WHERE table_schema = 'public'
       AND table_name = 'device_heartbeats'
       AND constraint_name = 'device_heartbeats_device_location_shape'`,
  );
  assert.equal(constraints.rows.length, 1);
}

async function assertCommunicationEventSchema(pool: Pool) {
  const result = await pool.query<{ exists: boolean }>(
    "SELECT to_regclass('public.communication_events') IS NOT NULL AS exists",
  );
  assert.equal(result.rows[0]?.exists, true);
}

async function assertCommunicationProjectionSchema(pool: Pool) {
  const result = await pool.query<{ exists: boolean }>(
    "SELECT to_regclass('public.communication_messages') IS NOT NULL AS exists",
  );
  assert.equal(result.rows[0]?.exists, true);
}

async function assertCommunicationObjectSchema(pool: Pool) {
  const result = await pool.query<{ exists: boolean }>(
    "SELECT to_regclass('public.communication_objects') IS NOT NULL AS exists",
  );
  assert.equal(result.rows[0]?.exists, true);
}

async function assertCommunicationFileProjectionSchema(pool: Pool) {
  const result = await pool.query<{ constraint_name: string }>(
    `SELECT constraint_name
     FROM information_schema.table_constraints
     WHERE table_schema = 'public'
       AND table_name IN ('communication_messages', 'communication_message_attachments')
       AND constraint_type = 'CHECK'
     ORDER BY constraint_name`,
  );
  const names = result.rows.map((row) => row.constraint_name);
  assert.ok(names.includes("communication_messages_kind"));
  assert.ok(names.includes("communication_message_attachments_kind"));
  assert.ok(!names.includes("communication_messages_kind_check"));
  assert.ok(!names.includes("communication_message_attachments_kind_check"));
}

async function assertCommunicationProjectionBackfill(pool: Pool) {
  const workspaceId = "01982222-7222-8222-8222-222222222224";
  const userId = "01983333-7333-8333-8333-333333333335";
  const deviceId = "01981111-7111-8111-8111-111111111112";
  await pool.query(
    "INSERT INTO auth_users (id, name, email, created_at, updated_at) VALUES ($1, 'Migration', 'migration@example.invalid', now(), now())",
    [userId],
  );
  await pool.query(
    "INSERT INTO workspaces (id, name, slug, created_at, updated_at) VALUES ($1, 'Migration', 'migration', now(), now())",
    [workspaceId],
  );
  await pool.query(
    "INSERT INTO workspace_members (workspace_id, user_id, role, created_at) VALUES ($1, $2, 'owner', now())",
    [workspaceId, userId],
  );
  await pool.query(
    "INSERT INTO devices (id, workspace_id, owner_user_id, device_public_key_hash, platform, created_at) VALUES ($1, $2, $3, $4, 'macos', now())",
    [deviceId, workspaceId, userId, "a".repeat(64)],
  );
  await pool.query(
    `INSERT INTO communication_events (
       event_id, workspace_id, device_id, event_type, source, schema_version,
       occurred_at, created_at, sensitivity, payload, attachment_refs, idempotency_key
     ) VALUES (
       '01986666-7666-8666-8666-666666666669', $1, $2,
       'communication.message_recorded', 'communication.wechat', 1,
       '2026-08-02T00:00:00Z', '2026-08-02T00:00:00Z', 'high', $3::jsonb, '[]'::jsonb,
       'source-key-migration'
     )`,
    [
      workspaceId,
      deviceId,
      JSON.stringify({
        message_id: "message-migration",
        conversation_id: "conversation-migration",
        source_key: "source-key-migration",
        occurred_at: "2026-08-02T00:00:00Z",
        direction: "incoming",
        kind: "text",
        conversation: { scope: "direct" },
        text: "private body",
      }),
    ],
  );
  await pool.query(await readFile(join(committedMigrationDirectory, "0007_communication_projections.sql"), "utf8"));
  const result = await pool.query<{ message_id: string; text_body: string }>(
    "SELECT message_id, text_body FROM communication_messages WHERE event_id = '01986666-7666-8666-8666-666666666669'",
  );
  assert.deepEqual(result.rows, [{ message_id: "message-migration", text_body: "private body" }]);

  const fileEventId = "01986666-7666-8666-8666-666666666670";
  await pool.query(
    `INSERT INTO communication_events (
       event_id, workspace_id, device_id, event_type, source, schema_version,
       occurred_at, created_at, sensitivity, payload, attachment_refs, idempotency_key
     ) VALUES (
       $1, $2, $3, 'communication.message_recorded', 'communication.wechat', 1,
       '2026-08-02T00:01:00Z', '2026-08-02T00:01:00Z', 'high', '{}'::jsonb, '[]'::jsonb,
       'source-key-file-migration'
     )`,
    [fileEventId, workspaceId, deviceId],
  );
  await pool.query(
    `INSERT INTO communication_messages (
       event_id, workspace_id, device_id, conversation_id, message_id, source_key,
       occurred_at, direction, kind, text_body
     ) VALUES ($1, $2, $3, 'conversation-migration', 'message-file-migration',
       'source-key-file-migration', '2026-08-02T00:01:00Z', 'incoming', 'file', NULL)`,
    [fileEventId, workspaceId, deviceId],
  );
  await pool.query(
    `INSERT INTO communication_message_attachments (
       event_id, attachment_id, kind, sha256, size_bytes, mime_type, file_name
     ) VALUES ($1, 'attachment-file-migration', 'file', $2, 12,
       'application/octet-stream', 'example.bin')`,
    [fileEventId, "b".repeat(64)],
  );
  const fileResult = await pool.query<{ kind: string; file_name: string }>(
    `SELECT message.kind, attachment.file_name
     FROM communication_messages AS message
     INNER JOIN communication_message_attachments AS attachment USING (event_id)
     WHERE message.event_id = $1`,
    [fileEventId],
  );
  assert.deepEqual(fileResult.rows, [{ kind: "file", file_name: "example.bin" }]);
}

async function assertCommunicationMediaUpgrade(pool: Pool) {
  const workspaceId = "01982222-7222-8222-8222-222222222224";
  const deviceId = "01981111-7111-8111-8111-111111111112";
  const repository = new DrizzleControlRepository(drizzle(pool, { schema: cloudSchema }));
  const event = (
    eventId: string,
    sourceKey: string,
    attachmentId: string,
    sizeBytes: number,
    createdAt: Date,
  ): CommunicationMessageEventRecord => ({
    eventId,
    workspaceId,
    deviceId,
    eventType: "communication.message_recorded",
    source: "communication.wechat",
    schemaVersion: 1,
    occurredAt: new Date("2026-08-02T00:02:00Z"),
    createdAt,
    sensitivity: "high",
    payload: {},
    attachmentRefs: [attachmentId],
    idempotencyKey: sourceKey,
    message: {
      messageId: "message-media-upgrade",
      conversationId: "conversation-migration",
      senderId: "wxid-self",
      senderDisplayName: "You",
      senderAvatarUrl: null,
      sourceKey,
      occurredAt: new Date("2026-08-02T00:02:00Z"),
      direction: "outgoing",
      kind: "image",
      conversation: { scope: "direct", memberCount: null },
      text: null,
      attachments: [{
        attachmentId,
        kind: "image",
        sha256: "c".repeat(64),
        sizeBytes,
        mimeType: "image/jpeg",
        fileName: null,
      }],
    },
  });

  await repository.appendCommunicationEvents(workspaceId, deviceId, [
    event(
      "01986666-7666-8666-8666-666666666671",
      "source-key-media-thumbnail",
      "attachment-media-thumbnail",
      1024,
      new Date("2026-08-02T00:02:01Z"),
    ),
  ]);
  await repository.appendCommunicationEvents(workspaceId, deviceId, [
    event(
      "01986666-7666-8666-8666-666666666672",
      "source-key-media-full",
      "attachment-media-full",
      4096,
      new Date("2026-08-02T00:02:02Z"),
    ),
  ]);

  const result = await pool.query<{ event_id: string; attachment_id: string; size_bytes: string }>(
    `SELECT message.event_id, attachment.attachment_id, attachment.size_bytes
     FROM communication_messages AS message
     INNER JOIN communication_message_attachments AS attachment USING (event_id)
     WHERE message.message_id = 'message-media-upgrade'`,
  );
  assert.deepEqual(result.rows, [{
    event_id: "01986666-7666-8666-8666-666666666672",
    attachment_id: "attachment-media-full",
    size_bytes: "4096",
  }]);
}

async function migrationIds(pool: Pool): Promise<string[]> {
  const result = await pool.query<{ id: string }>(
    "SELECT id FROM _pca_migrations WHERE status = 'completed' ORDER BY id",
  );
  return result.rows.map((row) => row.id);
}

async function copyMigrationsWithChanged0001(): Promise<string> {
  const directory = await mkdtemp(join(tmpdir(), "pca-changed-migrations-"));
  for (const file of await readdir(committedMigrationDirectory)) {
    if (!file.endsWith(".sql")) continue;
    const source = join(committedMigrationDirectory, file);
    const sql = await readFile(source, "utf8");
    await writeFile(join(directory, file), file.startsWith("0001_") ? `${sql}\n-- changed` : sql);
  }
  return directory;
}

async function startTemporaryPostgres() {
  const root = await mkdtemp(join(tmpdir(), "pca-migration-test-"));
  const dataDirectory = join(root, "data");
  const port = await availablePort();
  let started = false;
  const run = (binary: string, arguments_: string[]) =>
    execFileSync(binary, arguments_, { encoding: "utf8", stdio: "pipe" });

  try {
    run("initdb", ["-D", dataDirectory, "-A", "trust", "-U", postgresUser, "--encoding=UTF8", "--no-locale", "--no-sync"]);
    run("pg_ctl", ["-D", dataDirectory, "-o", `-F -h 127.0.0.1 -p ${port}`, "-l", join(root, "postgres.log"), "-w", "start"]);
    started = true;
    return {
      connectionString: `postgresql://${postgresUser}@127.0.0.1:${port}/postgres`,
      stop: async () => {
        if (started) run("pg_ctl", ["-D", dataDirectory, "-m", "fast", "-w", "stop"]);
        await rm(root, { recursive: true, force: true });
      },
    };
  } catch (error) {
    if (started) run("pg_ctl", ["-D", dataDirectory, "-m", "fast", "-w", "stop"]);
    await rm(root, { recursive: true, force: true });
    throw error;
  }
}

async function availablePort(): Promise<number> {
  return new Promise((resolvePort, reject) => {
    const server = createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      if (typeof address === "object" && address !== null) {
        server.close((error) => (error === undefined ? resolvePort(address.port) : reject(error)));
      } else {
        server.close();
        reject(new Error("unable to allocate a temporary PostgreSQL port"));
      }
    });
  });
}
