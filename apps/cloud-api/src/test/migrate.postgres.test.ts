import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

import {
  ControlRepositoryError,
  DrizzleControlRepository,
  type CommunicationMessageEventRecord,
} from "@pca/db-cloud/src/repository.js";
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
    await assertScreenshotSchema(pool);
    await assertCredentialRotationReplay(pool);
    await assertCollectorHealthSchema(pool);
    await assertHeartbeatPrivacyCleanup(pool);
    await assertRepairPairingReusesExistingDevice(pool);
    await assertRepairPairingReplacesMissingDevice(pool);

    await runCloudMigrations(postgres.connectionString, committedMigrationDirectory);
    await assertHashedSessionSchema(pool);
    await assertSystemEventSchema(pool);
    await assertCommunicationEventSchema(pool);
    await assertCommunicationProjectionSchema(pool);
    await assertCommunicationObjectSchema(pool);
    await assertDeviceLocationSchema(pool);
    await assertScreenshotSchema(pool);
    await assertCollectorHealthSchema(pool);
    assert.deepEqual(await migrationIds(pool), ["0000", "0001", "0002", "0003", "0004", "0005", "0006", "0007", "0008", "0009", "0010", "0011", "0012", "0013", "0014", "0015", "0016", "0017", "0018", "0019", "0020", "0021", "0022", "0023", "0024", "0025", "0026", "0027", "0028", "0029"]);
  } finally {
    await pool.end();
    await postgres.stop();
  }
});

async function assertHeartbeatPrivacyCleanup(pool: Pool) {
  const workspaceId = "01982222-7222-8222-8222-222222222230";
  const ownerUserId = "01983333-7333-8333-8333-333333333340";
  const deviceId = "01981111-7111-8111-8111-111111111120";
  const sensitiveHeartbeatId = "01984444-7444-8444-8444-444444444440";
  const scrubbedHeartbeatId = "01984444-7444-8444-8444-444444444441";
  const now = new Date("2026-08-16T03:00:00.000Z");
  const old = new Date("2026-07-01T03:00:00.000Z");
  await pool.query(
    "INSERT INTO auth_users (id, name, email, created_at, updated_at) VALUES ($1, 'Heartbeat', 'heartbeat@example.invalid', $2, $2)",
    [ownerUserId, now],
  );
  await pool.query(
    "INSERT INTO workspaces (id, name, slug, created_at, updated_at) VALUES ($1, 'Heartbeat', 'heartbeat', $2, $2)",
    [workspaceId, now],
  );
  await pool.query(
    "INSERT INTO workspace_members (workspace_id, user_id, role, created_at) VALUES ($1, $2, 'owner', $3)",
    [workspaceId, ownerUserId, now],
  );
  await pool.query(
    "INSERT INTO devices (id, workspace_id, owner_user_id, device_public_key_hash, platform, created_at) VALUES ($1, $2, $3, $4, 'macos', $5)",
    [deviceId, workspaceId, ownerUserId, "7".repeat(64), now],
  );
  await pool.query(
    `INSERT INTO device_heartbeats (
       id, workspace_id, device_id, received_at, agent_version, presence, outbox_depth,
       network_interface_type, network_wifi_identity_available,
       network_ssid, network_bssid, network_local_ipv4, network_local_ipv6, network_public_ip,
       network_ip_country, network_ip_region, network_ip_city, network_ip_accuracy,
       network_location_latitude, network_location_longitude,
       network_location_horizontal_accuracy_meters, network_location_observed_at
     ) VALUES
       ($1, $3, $4, $5, '0.2.0', 'online', 0, 'wifi', true,
        'Private WiFi', '00:00:00:00:00:01', '192.168.1.20', '2001:db8::20', '203.0.113.20',
        'SG', 'Singapore', 'Singapore', 'ip_city', 1.3521, 103.8198, 20, $5),
       ($2, $3, $4, $5, '0.2.0', 'online', 0, 'wifi', false,
        NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL)`,
    [sensitiveHeartbeatId, scrubbedHeartbeatId, workspaceId, deviceId, old],
  );
  const before = await pool.query<{ xmin: string }>(
    "SELECT xmin::text AS xmin FROM device_heartbeats WHERE id = $1",
    [scrubbedHeartbeatId],
  );
  const repository = new DrizzleControlRepository(drizzle(pool, { schema: cloudSchema }));
  await repository.recordHeartbeat({
    heartbeatId: "01984444-7444-8444-8444-444444444442",
    workspaceId,
    deviceId,
    receivedAt: now,
    agentVersion: "0.2.0",
    presence: "online",
    outboxDepth: 0,
    localMedia: { completedFileCount: 0, completedBytes: 0, protectedFileCount: 0, protectedBytes: 0 },
    cleanupResult: null,
    network: null,
  });
  const sensitive = await pool.query<Record<string, string | null>>(
    `SELECT network_ssid, network_bssid, network_local_ipv4::text,
            network_local_ipv6::text, network_public_ip::text,
            network_ip_country, network_ip_region, network_ip_city, network_ip_accuracy,
            network_location_latitude::text, network_location_longitude::text,
            network_location_horizontal_accuracy_meters::text, network_location_observed_at::text
     FROM device_heartbeats WHERE id = $1`,
    [sensitiveHeartbeatId],
  );
  assert.ok(Object.values(sensitive.rows[0] ?? {}).every((value) => value === null));
  const after = await pool.query<{ xmin: string }>(
    "SELECT xmin::text AS xmin FROM device_heartbeats WHERE id = $1",
    [scrubbedHeartbeatId],
  );
  assert.equal(after.rows[0]?.xmin, before.rows[0]?.xmin);
  const index = await pool.query<{ indexdef: string }>(
    "SELECT indexdef FROM pg_indexes WHERE schemaname = 'public' AND indexname = 'idx_device_heartbeats_privacy_retention'",
  );
  assert.match(index.rows[0]?.indexdef ?? "", /\(received_at\)/);
}

async function assertCollectorHealthSchema(pool: Pool) {
  const result = await pool.query<{ column_name: string }>(
    "SELECT column_name FROM information_schema.columns WHERE table_schema = 'public' AND table_name = 'device_collector_health' ORDER BY ordinal_position",
  );
  assert.deepEqual(result.rows.map((row) => row.column_name), [
    "workspace_id",
    "device_id",
    "collector_key",
    "collector_version",
    "status",
    "desired_config_revision",
    "applied_config_revision",
    "last_event_at",
    "last_health_at",
    "error_code",
    "reported_at",
    "agent_version",
  ]);
}

async function assertCredentialRotationReplay(pool: Pool) {
  const workspaceId = "01982222-7222-8222-8222-222222222229";
  const userId = "01983333-7333-8333-8333-333333333339";
  const deviceId = "01981111-7111-8111-8111-111111111119";
  const now = new Date("2026-08-10T08:00:00.000Z");
  const replayExpiresAt = new Date(now.getTime() + 5 * 60 * 1000);
  await pool.query(
    "INSERT INTO auth_users (id, name, email, created_at, updated_at) VALUES ($1, 'Rotation', 'rotation@example.invalid', $2, $2)",
    [userId, now],
  );
  await pool.query(
    "INSERT INTO workspaces (id, name, slug, created_at, updated_at) VALUES ($1, 'Rotation', 'rotation', $2, $2)",
    [workspaceId, now],
  );
  await pool.query(
    "INSERT INTO workspace_members (workspace_id, user_id, role, created_at) VALUES ($1, $2, 'owner', $3)",
    [workspaceId, userId, now],
  );
  await pool.query(
    "INSERT INTO devices (id, workspace_id, owner_user_id, device_public_key_hash, platform, created_at) VALUES ($1, $2, $3, $4, 'macos', $5)",
    [deviceId, workspaceId, userId, "f".repeat(64), now],
  );
  await pool.query(
    `INSERT INTO device_credential_generations (
       workspace_id, device_id, generation, access_token_hash, refresh_token_hash,
       access_expires_at, refresh_expires_at, created_at
     ) VALUES ($1, $2, 1, $3, $4, $5, $5, $6)`,
    [workspaceId, deviceId, "1".repeat(64), "2".repeat(64), new Date("2026-09-10T08:00:00.000Z"), now],
  );
  const repository = new DrizzleControlRepository(drizzle(pool, { schema: cloudSchema }));
  const rotation = {
    workspaceId,
    deviceId,
    currentRefreshTokenHash: "2".repeat(64),
    newAccessTokenHash: "3".repeat(64),
    newRefreshTokenHash: "4".repeat(64),
    accessExpiresAt: new Date("2026-08-10T09:00:00.000Z"),
    refreshExpiresAt: new Date("2026-09-10T08:00:00.000Z"),
    replayPayload: "sealed-postgres-rotation",
    replayExpiresAt,
    now,
  };
  const grant = await repository.rotateDeviceCredentials(rotation);
  assert.equal(grant.replayPayload, rotation.replayPayload);
  assert.deepEqual(await repository.rotateDeviceCredentials({
    ...rotation,
    newAccessTokenHash: "5".repeat(64),
    newRefreshTokenHash: "6".repeat(64),
    replayPayload: "discarded-retry-payload",
  }), grant);
  assert.deepEqual(await repository.authenticateDeviceRefresh("2".repeat(64), now), {
    workspaceId,
    deviceId,
  });
  await assert.rejects(
    repository.authenticateDeviceRefresh("2".repeat(64), replayExpiresAt),
    (error) => error instanceof ControlRepositoryError && error.code === "CREDENTIAL_INVALID",
  );
}

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

test("migration 0020 repairs a thumbnail projection when an immutable full image event exists", async () => {
  const postgres = await startTemporaryPostgres();
  const pool = new Pool({ connectionString: postgres.connectionString });
  const previousMigrations = await copyMigrationsBefore0020();
  try {
    await runCloudMigrations(postgres.connectionString, previousMigrations);
    await seedIncorrectMediaProjection(pool);
    await runCloudMigrations(postgres.connectionString, committedMigrationDirectory);
    const result = await pool.query<{ event_id: string; attachment_id: string; size_bytes: string }>(
      `SELECT message.event_id, attachment.attachment_id, attachment.size_bytes
       FROM communication_messages AS message
       INNER JOIN communication_message_attachments AS attachment USING (event_id)
       WHERE message.message_id = 'message-migration-upgrade'`,
    );
    assert.deepEqual(result.rows, [{
      event_id: "01986666-7666-8666-8666-666666666675",
      attachment_id: "attachment-migration-full",
      size_bytes: "4096",
    }]);
  } finally {
    await pool.end();
    await postgres.stop();
    await rm(previousMigrations, { recursive: true, force: true });
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
  const pairingResult = await pool.query<{ column_name: string }>(
    `SELECT column_name FROM information_schema.columns
     WHERE table_schema = 'public' AND table_name = 'pairing_sessions'
       AND column_name = 'requested_device_id'`,
  );
  assert.equal(pairingResult.rows[0]?.column_name, "requested_device_id");
}

async function assertRepairPairingReusesExistingDevice(pool: Pool) {
  const workspaceId = "01982222-7222-8222-8222-222222222228";
  const ownerUserId = "01983333-7333-8333-8333-333333333338";
  const deviceId = "01981111-7111-8111-8111-111111111118";
  const now = new Date("2026-08-16T01:00:00.000Z");
  const later = new Date("2026-08-16T02:00:00.000Z");
  await pool.query(
    "INSERT INTO auth_users (id, name, email, created_at, updated_at) VALUES ($1, 'Repair', 'repair@example.invalid', $2, $2)",
    [ownerUserId, now],
  );
  await pool.query(
    "INSERT INTO workspaces (id, name, slug, created_at, updated_at) VALUES ($1, 'Repair', 'repair', $2, $2)",
    [workspaceId, now],
  );
  await pool.query(
    "INSERT INTO workspace_members (workspace_id, user_id, role, created_at) VALUES ($1, $2, 'owner', $3)",
    [workspaceId, ownerUserId, now],
  );
  await pool.query(
    "INSERT INTO devices (id, workspace_id, owner_user_id, device_public_key_hash, platform, created_at) VALUES ($1, $2, $3, $4, 'macos', $5)",
    [deviceId, workspaceId, ownerUserId, "8".repeat(64), now],
  );
  await pool.query(
    `INSERT INTO device_credential_generations (
       workspace_id, device_id, generation, access_token_hash, refresh_token_hash,
       access_expires_at, refresh_expires_at, created_at
     ) VALUES ($1, $2, 1, $3, $4, $5, $5, $6)`,
    [workspaceId, deviceId, "9".repeat(64), "a".repeat(64), later, now],
  );
  await pool.query(
    `INSERT INTO collector_configs (
       workspace_id, device_id, configuration_revision, network_enabled, wechat_enabled, updated_at
     ) VALUES ($1, $2, 7, true, true, $3)`,
    [workspaceId, deviceId, now],
  );
  const repository = new DrizzleControlRepository(drizzle(pool, { schema: cloudSchema }));
  await repository.createPairingSession({
    sessionIdHash: "b".repeat(64),
    devicePublicKeyHash: "c".repeat(64),
    requestedDeviceId: deviceId,
    codeChallenge: "repair-challenge",
    callbackUri: "http://127.0.0.1:43123/pca/pair/callback",
    callbackStateHash: "d".repeat(64),
    expiresAt: later,
    createdAt: now,
  });
  await repository.authorizePairingSession({
    sessionIdHash: "b".repeat(64),
    authorizationCodeHash: "e".repeat(64),
    callbackStateHash: "d".repeat(64),
    workspaceId,
    ownerUserId,
    expiresAt: later,
    now,
  });
  const grant = await repository.consumeAuthorizationCode({
    sessionIdHash: "b".repeat(64),
    authorizationCodeHash: "e".repeat(64),
    codeChallenge: "repair-challenge",
    deviceId: "01984444-7444-8444-8444-444444444448",
    accessTokenHash: "f".repeat(64),
    refreshTokenHash: "0".repeat(64),
    accessExpiresAt: later,
    refreshExpiresAt: later,
    now,
  });
  assert.equal(grant.deviceId, deviceId);
  assert.equal(grant.credentialGeneration, 2);
  const result = await pool.query<{
    device_count: string;
    configuration_revision: string;
    wechat_enabled: boolean;
    active_credentials: string;
  }>(
    `SELECT
       (SELECT count(*)::text FROM devices WHERE workspace_id = $1) AS device_count,
       configuration_revision::text,
       wechat_enabled,
       (SELECT count(*)::text FROM device_credential_generations
        WHERE workspace_id = $1 AND device_id = $2 AND revoked_at IS NULL) AS active_credentials
     FROM collector_configs WHERE workspace_id = $1 AND device_id = $2`,
    [workspaceId, deviceId],
  );
  assert.deepEqual(result.rows[0], {
    device_count: "1",
    configuration_revision: "7",
    wechat_enabled: true,
    active_credentials: "1",
  });
}

async function assertRepairPairingReplacesMissingDevice(pool: Pool) {
  const workspaceId = "01982222-7222-8222-8222-222222222238";
  const ownerUserId = "01983333-7333-8333-8333-333333333348";
  const missingDeviceId = "01981111-7111-8111-8111-111111111128";
  const replacementDeviceId = "01984444-7444-8444-8444-444444444458";
  const now = new Date("2026-08-16T01:30:00.000Z");
  const later = new Date("2026-08-16T02:30:00.000Z");
  await pool.query(
    "INSERT INTO auth_users (id, name, email, created_at, updated_at) VALUES ($1, 'Replacement', 'replacement@example.invalid', $2, $2)",
    [ownerUserId, now],
  );
  await pool.query(
    "INSERT INTO workspaces (id, name, slug, created_at, updated_at) VALUES ($1, 'Replacement', 'replacement', $2, $2)",
    [workspaceId, now],
  );
  await pool.query(
    "INSERT INTO workspace_members (workspace_id, user_id, role, created_at) VALUES ($1, $2, 'owner', $3)",
    [workspaceId, ownerUserId, now],
  );
  const repository = new DrizzleControlRepository(drizzle(pool, { schema: cloudSchema }));
  await repository.createPairingSession({
    sessionIdHash: "1".repeat(64),
    devicePublicKeyHash: "2".repeat(64),
    requestedDeviceId: missingDeviceId,
    codeChallenge: "replacement-challenge",
    callbackUri: "http://127.0.0.1:43123/pca/pair/callback",
    callbackStateHash: "3".repeat(64),
    expiresAt: later,
    createdAt: now,
  });
  await repository.authorizePairingSession({
    sessionIdHash: "1".repeat(64),
    authorizationCodeHash: "4".repeat(64),
    callbackStateHash: "3".repeat(64),
    workspaceId,
    ownerUserId,
    expiresAt: later,
    now,
  });
  const grant = await repository.consumeAuthorizationCode({
    sessionIdHash: "1".repeat(64),
    authorizationCodeHash: "4".repeat(64),
    codeChallenge: "replacement-challenge",
    deviceId: replacementDeviceId,
    accessTokenHash: "5".repeat(64),
    refreshTokenHash: "6".repeat(64),
    accessExpiresAt: later,
    refreshExpiresAt: later,
    now,
  });

  assert.equal(grant.deviceId, replacementDeviceId);
  assert.equal(grant.credentialGeneration, 1);
  const result = await pool.query<{ device_id: string; configuration_revision: string }>(
    `SELECT devices.id AS device_id, collector_configs.configuration_revision::text
     FROM devices
     INNER JOIN collector_configs ON collector_configs.device_id = devices.id
     WHERE devices.workspace_id = $1`,
    [workspaceId],
  );
  assert.deepEqual(result.rows, [{
    device_id: replacementDeviceId,
    configuration_revision: "0",
  }]);
}

async function assertSystemEventSchema(pool: Pool) {
  const result = await pool.query<{ exists: boolean }>(
    "SELECT to_regclass('public.system_events') IS NOT NULL AS exists",
  );
  assert.equal(result.rows[0]?.exists, true);
  const workspaceId = "01982222-7222-8222-8222-222222222222";
  const deviceId = "01981111-7111-8111-8111-111111111111";
  const userId = "01983333-7333-8333-8333-333333333333";
  await pool.query(
    `INSERT INTO auth_users (id, name, email, created_at, updated_at)
     VALUES ($1, 'Photo Migration', 'photo-migration@example.invalid', now(), now())
     ON CONFLICT (id) DO NOTHING`,
    [userId],
  );
  await pool.query(
    `INSERT INTO workspaces (id, name, slug, created_at, updated_at)
     VALUES ($1, 'Photo Migration', 'photo-migration', now(), now())
     ON CONFLICT (id) DO NOTHING`,
    [workspaceId],
  );
  await pool.query(
    `INSERT INTO workspace_members (workspace_id, user_id, role, created_at)
     VALUES ($1, $2, 'owner', now())
     ON CONFLICT (workspace_id, user_id) DO NOTHING`,
    [workspaceId, userId],
  );
  await pool.query(
    `INSERT INTO devices (id, workspace_id, owner_user_id, device_public_key_hash, platform, created_at)
     VALUES ($1, $2, $3, $4, 'macos', now())
     ON CONFLICT (id) DO NOTHING`,
    [deviceId, workspaceId, userId, "e".repeat(64)],
  );
  await pool.query(
    `INSERT INTO system_events (
       event_id, workspace_id, device_id, event_type, source, schema_version,
       occurred_at, created_at, sensitivity, payload, idempotency_key
     ) VALUES (
       '01989999-7999-8999-8999-999999999996', $1, $2,
       'photos.asset_recorded', 'photos.library', 1, now(), now(), 'high', '{}'::jsonb,
       'photos:migration-check'
     ) ON CONFLICT (event_id) DO NOTHING`,
    [workspaceId, deviceId],
  );
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

async function assertScreenshotSchema(pool: Pool) {
  const tables = await pool.query<{ name: string | null }>(
    `SELECT to_regclass('public.device_screenshot_requests')::text AS name
     UNION ALL
     SELECT to_regclass('public.device_screenshots')::text AS name`,
  );
  assert.deepEqual(tables.rows.map((row) => row.name), [
    "device_screenshot_requests",
    "device_screenshots",
  ]);
  const columns = await pool.query<{ column_name: string }>(
    `SELECT column_name FROM information_schema.columns
     WHERE table_schema = 'public' AND table_name = 'collector_configs'
       AND column_name LIKE 'screen_capture_%'
     ORDER BY column_name`,
  );
  assert.deepEqual(columns.rows.map((row) => row.column_name), [
    "screen_capture_activity_enabled",
    "screen_capture_activity_min_interval_seconds",
    "screen_capture_enabled",
    "screen_capture_excluded_bundle_ids",
    "screen_capture_interval_seconds",
    "screen_capture_scheduled_enabled",
  ]);
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

  const repository = new DrizzleControlRepository(drizzle(pool, { schema: cloudSchema }));
  const conversations = await repository.listOwnerCommunicationConversations(
    deviceId,
    workspaceId,
    userId,
    "communication.wechat",
    100,
    0,
  );
  assert.equal(conversations.conversations.length, 1);
  assert.ok(conversations.conversations[0]?.lastMessageAt instanceof Date);
  assert.equal(conversations.conversations[0]?.lastMessageAt.toISOString(), "2026-08-02T00:01:00.000Z");

  const missing = await repository.listUnlinkedCommunicationAttachments(100);
  const missingFile = missing.find((attachment) => attachment.eventId === fileEventId);
  assert.ok(missingFile !== undefined);
  assert.equal(await repository.recoverCompletedCommunicationObject({
    ...missingFile,
    objectId: "01986666-7666-8666-8666-666666666676",
    objectKey: "communication/01986666-7666-8666-8666-666666666677",
    now: new Date("2026-08-05T08:00:00Z"),
  }), true);
  assert.equal(await repository.recoverCompletedCommunicationObject({
    ...missingFile,
    objectId: "01986666-7666-8666-8666-666666666678",
    objectKey: "communication/01986666-7666-8666-8666-666666666679",
    now: new Date("2026-08-05T08:00:01Z"),
  }), false);
  assert.ok((await repository.listCommunicationObjectKeys()).includes(
    "communication/01986666-7666-8666-8666-666666666677",
  ));
  assert.equal(
    (await repository.listUnlinkedCommunicationAttachments(100))
      .some((attachment) => attachment.eventId === fileEventId),
    false,
  );
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
      "source-key-media:image",
      "attachment-media-thumbnail",
      1024,
      new Date("2026-08-02T00:02:00Z"),
    ),
  ]);
  await repository.appendCommunicationEvents(workspaceId, deviceId, [
    event(
      "01986666-7666-8666-8666-666666666672",
      "source-key-media:image:full",
      "attachment-media-full",
      4096,
      new Date("2026-08-02T00:02:00Z"),
    ),
  ]);
  await repository.appendCommunicationEvents(workspaceId, deviceId, [
    event(
      "01986666-7666-8666-8666-666666666673",
      "source-key-media:image:thumbnail-retry",
      "attachment-media-thumbnail-retry",
      2048,
      new Date("2026-08-02T00:02:00Z"),
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

async function seedIncorrectMediaProjection(pool: Pool) {
  const workspaceId = "01982222-7222-8222-8222-222222222225";
  const userId = "01983333-7333-8333-8333-333333333336";
  const deviceId = "01981111-7111-8111-8111-111111111113";
  const thumbnailEventId = "01986666-7666-8666-8666-666666666674";
  const fullEventId = "01986666-7666-8666-8666-666666666675";
  await pool.query(
    "INSERT INTO auth_users (id, name, email, created_at, updated_at) VALUES ($1, 'Upgrade', 'upgrade@example.invalid', now(), now())",
    [userId],
  );
  await pool.query(
    "INSERT INTO workspaces (id, name, slug, created_at, updated_at) VALUES ($1, 'Upgrade', 'upgrade', now(), now())",
    [workspaceId],
  );
  await pool.query(
    "INSERT INTO workspace_members (workspace_id, user_id, role, created_at) VALUES ($1, $2, 'owner', now())",
    [workspaceId, userId],
  );
  await pool.query(
    "INSERT INTO devices (id, workspace_id, owner_user_id, device_public_key_hash, platform, created_at) VALUES ($1, $2, $3, $4, 'macos', now())",
    [deviceId, workspaceId, userId, "d".repeat(64)],
  );
  await pool.query(
    `INSERT INTO communication_conversations (
       workspace_id, device_id, conversation_id, display_name, scope, member_count, last_message_at
     ) VALUES ($1, $2, 'conversation-migration-upgrade', 'Upgrade', 'direct', NULL, '2026-08-02T00:03:00Z')`,
    [workspaceId, deviceId],
  );
  const eventPayload = (sourceKey: string, attachmentId: string, sizeBytes: number) => JSON.stringify({
    message_id: "message-migration-upgrade",
    conversation_id: "conversation-migration-upgrade",
    sender_id: "wxid-self",
    sender_display_name: "You",
    source_key: sourceKey,
    occurred_at: "2026-08-02T00:03:00Z",
    direction: "outgoing",
    kind: "image",
    conversation: { scope: "direct" },
    attachments: [{
      attachment_id: attachmentId,
      kind: "image",
      sha256: "e".repeat(64),
      size_bytes: sizeBytes,
      mime_type: "image/jpeg",
    }],
  });
  for (const [eventId, sourceKey, attachmentId, sizeBytes] of [
    [thumbnailEventId, "source-key-migration:image", "attachment-migration-thumbnail", 1024],
    [fullEventId, "source-key-migration:image:full", "attachment-migration-full", 4096],
  ] as const) {
    await pool.query(
      `INSERT INTO communication_events (
         event_id, workspace_id, device_id, event_type, source, schema_version,
         occurred_at, created_at, sensitivity, payload, attachment_refs, idempotency_key
       ) VALUES ($1, $2, $3, 'communication.message_recorded', 'communication.wechat', 1,
         '2026-08-02T00:03:00Z', '2026-08-02T00:03:00Z', 'high', $4::jsonb, $5::jsonb, $6)`,
      [eventId, workspaceId, deviceId, eventPayload(sourceKey, attachmentId, sizeBytes), JSON.stringify([attachmentId]), sourceKey],
    );
  }
  await pool.query(
    `INSERT INTO communication_messages (
       event_id, workspace_id, device_id, conversation_id, message_id, sender_id,
       sender_display_name, source_key, occurred_at, direction, kind, text_body
     ) VALUES ($1, $2, $3, 'conversation-migration-upgrade', 'message-migration-upgrade',
       'wxid-self', 'You', 'source-key-migration:image', '2026-08-02T00:03:00Z', 'outgoing', 'image', NULL)`,
    [thumbnailEventId, workspaceId, deviceId],
  );
  await pool.query(
    `INSERT INTO communication_message_attachments (
       event_id, attachment_id, kind, sha256, size_bytes, mime_type
     ) VALUES ($1, 'attachment-migration-thumbnail', 'image', $2, 1024, 'image/jpeg')`,
    [thumbnailEventId, "e".repeat(64)],
  );
}

async function copyMigrationsBefore0020(): Promise<string> {
  const directory = await mkdtemp(join(tmpdir(), "pca-before-0020-migrations-"));
  for (const file of await readdir(committedMigrationDirectory)) {
    if (!file.endsWith(".sql") || file.slice(0, 4) >= "0020") continue;
    await writeFile(join(directory, file), await readFile(join(committedMigrationDirectory, file), "utf8"));
  }
  return directory;
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
