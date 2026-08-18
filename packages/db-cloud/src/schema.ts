import { isNull, sql } from "drizzle-orm";
import {
  bigint,
  boolean,
  char,
  check,
  doublePrecision,
  foreignKey,
  index,
  inet,
  integer,
  jsonb,
  pgTable,
  primaryKey,
  text,
  timestamp,
  unique,
  uniqueIndex,
  uuid,
} from "drizzle-orm/pg-core";

export interface StoredCollectorConfig {
  networkEnabled: boolean;
  wechatEnabled: boolean;
  messagesEnabled: boolean;
  photosEnabled: boolean;
  screenCaptureEnabled: boolean;
  screenCaptureScheduledEnabled: boolean;
  screenCaptureIntervalSeconds: number;
  screenCaptureActivityEnabled: boolean;
  screenCaptureActivityMinIntervalSeconds: number;
  screenCaptureExcludedBundleIds: string[];
}

const timestampColumn = (name: string) =>
  timestamp(name, { withTimezone: true, mode: "date" });

export const authUsers = pgTable(
  "auth_users",
  {
    id: uuid("id").primaryKey(),
    name: text("name").notNull(),
    email: text("email").notNull(),
    emailVerified: boolean("email_verified").notNull().default(false),
    imageUrl: text("image_url"),
    createdAt: timestampColumn("created_at").notNull(),
    updatedAt: timestampColumn("updated_at").notNull(),
  },
  (table) => [uniqueIndex("auth_users_email_unique").on(table.email)],
);

export const authSessions = pgTable(
  "auth_sessions",
  {
    id: uuid("id").primaryKey(),
    userId: uuid("user_id")
      .notNull()
      .references(() => authUsers.id, { onDelete: "cascade" }),
    sessionTokenHash: char("session_token_hash", { length: 64 }).notNull(),
    expiresAt: timestampColumn("expires_at").notNull(),
    ipAddress: inet("ip_address"),
    userAgent: text("user_agent"),
    createdAt: timestampColumn("created_at").notNull(),
    updatedAt: timestampColumn("updated_at").notNull(),
  },
  (table) => [
    uniqueIndex("auth_sessions_token_hash_unique").on(table.sessionTokenHash),
    check(
      "auth_sessions_token_hash_hex",
      sql`${table.sessionTokenHash} ~ '^[0-9a-f]{64}$'`,
    ),
  ],
);

export const authAccounts = pgTable(
  "auth_accounts",
  {
    id: uuid("id").primaryKey(),
    userId: uuid("user_id")
      .notNull()
      .references(() => authUsers.id, { onDelete: "cascade" }),
    providerId: text("provider_id").notNull(),
    accountId: text("account_id").notNull(),
    passwordHash: text("password_hash"),
    createdAt: timestampColumn("created_at").notNull(),
    updatedAt: timestampColumn("updated_at").notNull(),
  },
  (table) => [unique("auth_accounts_provider_unique").on(table.providerId, table.accountId)],
);

export const workspaces = pgTable(
  "workspaces",
  {
    id: uuid("id").primaryKey(),
    name: text("name").notNull(),
    slug: text("slug").notNull(),
    createdAt: timestampColumn("created_at").notNull(),
    updatedAt: timestampColumn("updated_at").notNull(),
  },
  (table) => [uniqueIndex("workspaces_slug_unique").on(table.slug)],
);

export const workspaceMembers = pgTable(
  "workspace_members",
  {
    workspaceId: uuid("workspace_id")
      .notNull()
      .references(() => workspaces.id, { onDelete: "cascade" }),
    userId: uuid("user_id")
      .notNull()
      .references(() => authUsers.id, { onDelete: "cascade" }),
    role: text("role").notNull(),
    createdAt: timestampColumn("created_at").notNull(),
  },
  (table) => [
    primaryKey({ columns: [table.workspaceId, table.userId] }),
    unique("workspace_members_s1b_user_unique").on(table.userId),
    check("workspace_members_owner_only", sql`${table.role} = 'owner'`),
  ],
);

export const devices = pgTable(
  "devices",
  {
    id: uuid("id").primaryKey(),
    workspaceId: uuid("workspace_id")
      .notNull()
      .references(() => workspaces.id, { onDelete: "restrict" }),
    ownerUserId: uuid("owner_user_id").notNull(),
    devicePublicKeyHash: char("device_public_key_hash", { length: 64 }).notNull(),
    platform: text("platform").notNull(),
    createdAt: timestampColumn("created_at").notNull(),
    revokedAt: timestampColumn("revoked_at"),
    mergedIntoDeviceId: uuid("merged_into_device_id"),
    mergedAt: timestampColumn("merged_at"),
    mergedByUserId: uuid("merged_by_user_id"),
  },
  (table) => [
    unique("devices_workspace_id_unique").on(table.workspaceId, table.id),
    foreignKey({
      name: "devices_owner_membership_fk",
      columns: [table.workspaceId, table.ownerUserId],
      foreignColumns: [workspaceMembers.workspaceId, workspaceMembers.userId],
    }).onDelete("restrict"),
    uniqueIndex("devices_public_key_hash_unique").on(table.devicePublicKeyHash),
    index("idx_devices_workspace").on(table.workspaceId, table.id),
    index("idx_devices_merge_target").on(table.workspaceId, table.mergedIntoDeviceId).where(sql`${table.mergedIntoDeviceId} IS NOT NULL`),
    foreignKey({
      name: "devices_merge_target_fk",
      columns: [table.workspaceId, table.mergedIntoDeviceId],
      foreignColumns: [table.workspaceId, table.id],
    }).onDelete("restrict"),
    foreignKey({
      name: "devices_merge_actor_fk",
      columns: [table.mergedByUserId],
      foreignColumns: [authUsers.id],
    }).onDelete("restrict"),
    check("devices_not_merged_into_self", sql`${table.mergedIntoDeviceId} IS NULL OR ${table.mergedIntoDeviceId} <> ${table.id}`),
    check("devices_merge_fields_complete", sql`(
      (${table.mergedIntoDeviceId} IS NULL AND ${table.mergedAt} IS NULL AND ${table.mergedByUserId} IS NULL)
      OR
      (${table.mergedIntoDeviceId} IS NOT NULL AND ${table.mergedAt} IS NOT NULL AND ${table.mergedByUserId} IS NOT NULL)
    )`),
    check("devices_macos_only", sql`${table.platform} = 'macos'`),
    check(
      "devices_public_key_hash_hex",
      sql`${table.devicePublicKeyHash} ~ '^[0-9a-f]{64}$'`,
    ),
  ],
);

export const deviceCredentialGenerations = pgTable(
  "device_credential_generations",
  {
    workspaceId: uuid("workspace_id").notNull(),
    deviceId: uuid("device_id").notNull(),
    generation: bigint("generation", { mode: "number" }).notNull(),
    accessTokenHash: char("access_token_hash", { length: 64 }).notNull(),
    refreshTokenHash: char("refresh_token_hash", { length: 64 }).notNull(),
    accessExpiresAt: timestampColumn("access_expires_at").notNull(),
    refreshExpiresAt: timestampColumn("refresh_expires_at").notNull(),
    createdAt: timestampColumn("created_at").notNull(),
    revokedAt: timestampColumn("revoked_at"),
    rotationReplayPayload: text("rotation_replay_payload"),
    rotationReplayExpiresAt: timestampColumn("rotation_replay_expires_at"),
  },
  (table) => [
    primaryKey({ columns: [table.deviceId, table.generation] }),
    foreignKey({
      columns: [table.workspaceId, table.deviceId],
      foreignColumns: [devices.workspaceId, devices.id],
    }).onDelete("cascade"),
    uniqueIndex("device_credentials_access_hash_unique").on(table.accessTokenHash),
    uniqueIndex("device_credentials_refresh_hash_unique").on(table.refreshTokenHash),
    check("device_credentials_generation_positive", sql`${table.generation} > 0`),
    check(
      "device_credentials_access_hash_hex",
      sql`${table.accessTokenHash} ~ '^[0-9a-f]{64}$'`,
    ),
    check(
      "device_credentials_refresh_hash_hex",
      sql`${table.refreshTokenHash} ~ '^[0-9a-f]{64}$'`,
    ),
    check(
      "device_credentials_rotation_replay_pair",
      sql`(${table.rotationReplayPayload} IS NULL) = (${table.rotationReplayExpiresAt} IS NULL)`,
    ),
    check(
      "device_credentials_rotation_replay_nonempty",
      sql`${table.rotationReplayPayload} IS NULL OR length(${table.rotationReplayPayload}) > 0`,
    ),
  ],
);

export const pairingSessions = pgTable(
  "pairing_sessions",
  {
    sessionIdHash: char("session_id_hash", { length: 64 }).primaryKey(),
    devicePublicKeyHash: char("device_public_key_hash", { length: 64 }).notNull(),
    requestedDeviceId: uuid("requested_device_id"),
    codeChallenge: text("code_challenge").notNull(),
    callbackUri: text("callback_uri").notNull(),
    callbackStateHash: char("callback_state_hash", { length: 64 }),
    expiresAt: timestampColumn("expires_at").notNull(),
    createdAt: timestampColumn("created_at").notNull(),
    authorizedAt: timestampColumn("authorized_at"),
  },
  (table) => [
    index("idx_pairing_sessions_active_expiry")
      .on(table.expiresAt)
      .where(isNull(table.authorizedAt)),
    index("idx_pairing_sessions_requested_device")
      .on(table.requestedDeviceId)
      .where(sql`${table.requestedDeviceId} IS NOT NULL`),
    check("pairing_sessions_id_hash_hex", sql`${table.sessionIdHash} ~ '^[0-9a-f]{64}$'`),
    check(
      "pairing_sessions_public_key_hash_hex",
      sql`${table.devicePublicKeyHash} ~ '^[0-9a-f]{64}$'`,
    ),
    check(
      "pairing_sessions_callback_state_hash_hex",
      sql`${table.callbackStateHash} IS NULL OR ${table.callbackStateHash} ~ '^[0-9a-f]{64}$'`,
    ),
  ],
);

export const pairingAuthorizationCodes = pgTable(
  "pairing_authorization_codes",
  {
    authorizationCodeHash: char("authorization_code_hash", { length: 64 }).primaryKey(),
    sessionIdHash: char("session_id_hash", { length: 64 })
      .notNull()
      .references(() => pairingSessions.sessionIdHash, { onDelete: "cascade" }),
    workspaceId: uuid("workspace_id")
      .notNull()
      .references(() => workspaces.id, { onDelete: "restrict" }),
    ownerUserId: uuid("owner_user_id").notNull(),
    callbackStateHash: char("callback_state_hash", { length: 64 }).notNull(),
    expiresAt: timestampColumn("expires_at").notNull(),
    createdAt: timestampColumn("created_at").notNull(),
    consumedAt: timestampColumn("consumed_at"),
  },
  (table) => [
    unique("pairing_authorization_codes_session_unique").on(table.sessionIdHash),
    foreignKey({
      name: "pairing_codes_owner_membership_fk",
      columns: [table.workspaceId, table.ownerUserId],
      foreignColumns: [workspaceMembers.workspaceId, workspaceMembers.userId],
    }).onDelete("restrict"),
    check(
      "pairing_authorization_code_hash_hex",
      sql`${table.authorizationCodeHash} ~ '^[0-9a-f]{64}$'`,
    ),
    check(
      "pairing_callback_state_hash_hex",
      sql`${table.callbackStateHash} ~ '^[0-9a-f]{64}$'`,
    ),
  ],
);

export const collectorConfigs = pgTable(
  "collector_configs",
  {
    workspaceId: uuid("workspace_id").notNull(),
    deviceId: uuid("device_id").notNull(),
    configurationRevision: bigint("configuration_revision", { mode: "number" })
      .notNull()
      .default(0),
    networkEnabled: boolean("network_enabled").notNull().default(false),
    wechatEnabled: boolean("wechat_enabled").notNull().default(false),
    messagesEnabled: boolean("messages_enabled").notNull().default(false),
    photosEnabled: boolean("photos_enabled").notNull().default(false),
    screenCaptureEnabled: boolean("screen_capture_enabled").notNull().default(false),
    screenCaptureScheduledEnabled: boolean("screen_capture_scheduled_enabled").notNull().default(true),
    screenCaptureIntervalSeconds: integer("screen_capture_interval_seconds").notNull().default(300),
    screenCaptureActivityEnabled: boolean("screen_capture_activity_enabled").notNull().default(true),
    screenCaptureActivityMinIntervalSeconds: integer("screen_capture_activity_min_interval_seconds").notNull().default(30),
    screenCaptureExcludedBundleIds: jsonb("screen_capture_excluded_bundle_ids").$type<string[]>().notNull().default([]),
    updatedAt: timestampColumn("updated_at").notNull(),
  },
  (table) => [
    primaryKey({ columns: [table.workspaceId, table.deviceId] }),
    foreignKey({
      columns: [table.workspaceId, table.deviceId],
      foreignColumns: [devices.workspaceId, devices.id],
    }).onDelete("cascade"),
    check("collector_configs_revision_nonnegative", sql`${table.configurationRevision} >= 0`),
    check("collector_configs_screen_interval", sql`${table.screenCaptureIntervalSeconds} BETWEEN 60 AND 86400`),
    check("collector_configs_screen_activity_interval", sql`${table.screenCaptureActivityMinIntervalSeconds} BETWEEN 10 AND 3600`),
  ],
);

export const collectorConfigAudit = pgTable(
  "collector_config_audit",
  {
    id: uuid("id").primaryKey(),
    workspaceId: uuid("workspace_id").notNull(),
    deviceId: uuid("device_id").notNull(),
    actorUserId: uuid("actor_user_id").notNull(),
    configurationRevision: bigint("configuration_revision", { mode: "number" }).notNull(),
    oldConfig: jsonb("old_config").$type<StoredCollectorConfig>().notNull(),
    newConfig: jsonb("new_config").$type<StoredCollectorConfig>().notNull(),
    createdAt: timestampColumn("created_at").notNull(),
  },
  (table) => [
    foreignKey({
      columns: [table.workspaceId, table.deviceId],
      foreignColumns: [devices.workspaceId, devices.id],
    }).onDelete("cascade"),
    foreignKey({
      name: "config_audit_actor_membership_fk",
      columns: [table.workspaceId, table.actorUserId],
      foreignColumns: [workspaceMembers.workspaceId, workspaceMembers.userId],
    }).onDelete("restrict"),
    unique("collector_config_audit_device_revision_unique").on(
      table.deviceId,
      table.configurationRevision,
    ),
    index("idx_collector_config_audit_chronology").on(
      table.workspaceId,
      table.deviceId,
      table.createdAt.desc(),
    ),
    check("collector_config_audit_revision_positive", sql`${table.configurationRevision} > 0`),
  ],
);

export const deviceHeartbeats = pgTable(
  "device_heartbeats",
  {
    id: uuid("id").primaryKey(),
    workspaceId: uuid("workspace_id").notNull(),
    deviceId: uuid("device_id").notNull(),
    receivedAt: timestampColumn("received_at").notNull(),
    agentVersion: text("agent_version").notNull(),
    presence: text("presence").notNull(),
    outboxDepth: bigint("outbox_depth", { mode: "number" }).notNull(),
    completedMediaFileCount: bigint("completed_media_file_count", { mode: "number" }).notNull().default(0),
    completedMediaBytes: bigint("completed_media_bytes", { mode: "number" }).notNull().default(0),
    protectedMediaFileCount: bigint("protected_media_file_count", { mode: "number" }).notNull().default(0),
    protectedMediaBytes: bigint("protected_media_bytes", { mode: "number" }).notNull().default(0),
    networkInterfaceType: text("network_interface_type"),
    networkWifiIdentityAvailable: boolean("network_wifi_identity_available"),
    networkSsid: text("network_ssid"),
    networkBssid: text("network_bssid"),
    networkLocalIpv4: inet("network_local_ipv4"),
    networkLocalIpv6: inet("network_local_ipv6"),
    networkPublicIp: inet("network_public_ip"),
    networkIpCountry: text("network_ip_country"),
    networkIpRegion: text("network_ip_region"),
    networkIpCity: text("network_ip_city"),
    networkIpAccuracy: text("network_ip_accuracy"),
    networkLocationLatitude: doublePrecision("network_location_latitude"),
    networkLocationLongitude: doublePrecision("network_location_longitude"),
    networkLocationHorizontalAccuracyMeters: doublePrecision("network_location_horizontal_accuracy_meters"),
    networkLocationObservedAt: timestampColumn("network_location_observed_at"),
  },
  (table) => [
    foreignKey({
      columns: [table.workspaceId, table.deviceId],
      foreignColumns: [devices.workspaceId, devices.id],
    }).onDelete("cascade"),
    index("idx_device_heartbeats_last").on(
      table.workspaceId,
      table.deviceId,
      table.receivedAt.desc(),
    ),
    index("idx_device_heartbeats_privacy_retention").on(table.receivedAt).where(sql`
      ${table.networkSsid} IS NOT NULL
      OR ${table.networkBssid} IS NOT NULL
      OR ${table.networkLocalIpv4} IS NOT NULL
      OR ${table.networkLocalIpv6} IS NOT NULL
      OR ${table.networkPublicIp} IS NOT NULL
      OR ${table.networkIpCountry} IS NOT NULL
      OR ${table.networkIpRegion} IS NOT NULL
      OR ${table.networkIpCity} IS NOT NULL
      OR ${table.networkIpAccuracy} IS NOT NULL
      OR ${table.networkLocationLatitude} IS NOT NULL
      OR ${table.networkLocationLongitude} IS NOT NULL
      OR ${table.networkLocationHorizontalAccuracyMeters} IS NOT NULL
      OR ${table.networkLocationObservedAt} IS NOT NULL
    `),
    check(
      "device_heartbeats_presence",
      sql`${table.presence} IN ('online', 'stale', 'offline', 'sleeping')`,
    ),
    check("device_heartbeats_outbox_nonnegative", sql`${table.outboxDepth} >= 0`),
    check(
      "device_heartbeats_media_counts_nonnegative",
      sql`${table.completedMediaFileCount} >= 0 AND ${table.completedMediaBytes} >= 0 AND ${table.protectedMediaFileCount} >= 0 AND ${table.protectedMediaBytes} >= 0`,
    ),
  ],
);

export const deviceCollectorHealth = pgTable(
  "device_collector_health",
  {
    workspaceId: uuid("workspace_id").notNull(),
    deviceId: uuid("device_id").notNull(),
    collectorKey: text("collector_key").notNull(),
    collectorVersion: text("collector_version").notNull(),
    status: text("status").notNull(),
    desiredConfigRevision: bigint("desired_config_revision", { mode: "number" }).notNull(),
    appliedConfigRevision: bigint("applied_config_revision", { mode: "number" }).notNull(),
    lastEventAt: timestampColumn("last_event_at"),
    lastHealthAt: timestampColumn("last_health_at"),
    errorCode: text("error_code"),
    reportedAt: timestampColumn("reported_at").notNull(),
    agentVersion: text("agent_version").notNull(),
  },
  (table) => [
    primaryKey({ columns: [table.workspaceId, table.deviceId, table.collectorKey] }),
    foreignKey({
      columns: [table.workspaceId, table.deviceId],
      foreignColumns: [devices.workspaceId, devices.id],
    }).onDelete("cascade"),
    index("idx_device_collector_health_reported").on(
      table.workspaceId,
      table.deviceId,
      table.reportedAt.desc(),
    ),
    check("device_collector_health_key_valid", sql`${table.collectorKey} ~ '^[a-z][a-z0-9.-]{0,63}$'`),
    check("device_collector_health_status_valid", sql`${table.status} IN ('disabled', 'permission_required', 'initializing', 'running', 'paused', 'degraded', 'unsupported', 'error')`),
    check("device_collector_health_revisions_nonnegative", sql`${table.desiredConfigRevision} >= 0 AND ${table.appliedConfigRevision} >= 0`),
    check("device_collector_health_error_code_valid", sql`${table.errorCode} IS NULL OR ${table.errorCode} ~ '^[A-Z][A-Z0-9_]{0,127}$'`),
  ],
);

export const deviceNetworkHistory = pgTable(
  "device_network_history",
  {
    id: uuid("id").primaryKey(),
    workspaceId: uuid("workspace_id").notNull(),
    deviceId: uuid("device_id").notNull(),
    observedAt: timestampColumn("observed_at").notNull(),
    interfaceType: text("interface_type").notNull(),
    wifiIdentityAvailable: boolean("wifi_identity_available").notNull(),
    ssid: text("ssid"),
    bssid: text("bssid"),
    localIpv4: inet("local_ipv4"),
    localIpv6: inet("local_ipv6"),
    publicIp: inet("public_ip"),
    ipCountry: text("ip_country"),
    ipRegion: text("ip_region"),
    ipCity: text("ip_city"),
    ipAccuracy: text("ip_accuracy"),
    locationLatitude: doublePrecision("location_latitude"),
    locationLongitude: doublePrecision("location_longitude"),
    locationHorizontalAccuracyMeters: doublePrecision("location_horizontal_accuracy_meters"),
    locationObservedAt: timestampColumn("location_observed_at"),
  },
  (table) => [
    foreignKey({
      columns: [table.workspaceId, table.deviceId],
      foreignColumns: [devices.workspaceId, devices.id],
    }).onDelete("cascade"),
    index("idx_device_network_history_recent").on(table.workspaceId, table.deviceId, table.observedAt.desc()),
  ],
);

export const networkLocationLibrary = pgTable(
  "network_location_library",
  {
    id: uuid("id").primaryKey(),
    workspaceId: uuid("workspace_id").notNull().references(() => workspaces.id, { onDelete: "cascade" }),
    actorUserId: uuid("actor_user_id").notNull(),
    name: text("name").notNull(),
    matchSsid: text("match_ssid"),
    matchBssid: text("match_bssid"),
    country: text("country"),
    region: text("region"),
    city: text("city"),
    createdAt: timestampColumn("created_at").notNull(),
    updatedAt: timestampColumn("updated_at").notNull(),
  },
  (table) => [
    foreignKey({
      columns: [table.workspaceId, table.actorUserId],
      foreignColumns: [workspaceMembers.workspaceId, workspaceMembers.userId],
    }).onDelete("cascade"),
    uniqueIndex("network_location_library_workspace_bssid_unique")
      .on(table.workspaceId, table.matchBssid)
      .where(sql`${table.matchBssid} IS NOT NULL`),
    index("idx_network_location_library_workspace_ssid")
      .on(table.workspaceId, table.matchSsid)
      .where(sql`${table.matchSsid} IS NOT NULL`),
  ],
);

export const deviceMediaCleanupRequests = pgTable(
  "device_media_cleanup_requests",
  {
    id: uuid("id").primaryKey(),
    workspaceId: uuid("workspace_id").notNull(),
    deviceId: uuid("device_id").notNull(),
    actorUserId: uuid("actor_user_id").notNull().references(() => authUsers.id, { onDelete: "restrict" }),
    status: text("status").notNull().default("queued"),
    requestedAt: timestampColumn("requested_at").notNull(),
    completedAt: timestampColumn("completed_at"),
    deletedFileCount: bigint("deleted_file_count", { mode: "number" }),
    freedBytes: bigint("freed_bytes", { mode: "number" }),
    errorCode: text("error_code"),
  },
  (table) => [
    foreignKey({
      columns: [table.workspaceId, table.deviceId],
      foreignColumns: [devices.workspaceId, devices.id],
    }).onDelete("cascade"),
    uniqueIndex("device_media_cleanup_one_queued")
      .on(table.workspaceId, table.deviceId)
      .where(sql`${table.status} = 'queued'`),
    index("device_media_cleanup_latest").on(
      table.workspaceId,
      table.deviceId,
      table.requestedAt.desc(),
    ),
    check(
      "device_media_cleanup_status",
      sql`${table.status} IN ('queued', 'succeeded', 'failed')`,
    ),
  ],
);

export const deviceScreenshotRequests = pgTable(
  "device_screenshot_requests",
  {
    id: uuid("id").primaryKey(),
    workspaceId: uuid("workspace_id").notNull(),
    deviceId: uuid("device_id").notNull(),
    actorUserId: uuid("actor_user_id").notNull().references(() => authUsers.id, { onDelete: "restrict" }),
    status: text("status").notNull().default("queued"),
    requestedAt: timestampColumn("requested_at").notNull(),
    completedAt: timestampColumn("completed_at"),
    screenshotId: uuid("screenshot_id"),
    errorCode: text("error_code"),
  },
  (table) => [
    foreignKey({
      columns: [table.workspaceId, table.deviceId],
      foreignColumns: [devices.workspaceId, devices.id],
    }).onDelete("cascade"),
    uniqueIndex("device_screenshot_requests_one_queued")
      .on(table.workspaceId, table.deviceId)
      .where(sql`${table.status} = 'queued'`),
    index("device_screenshot_requests_latest").on(table.workspaceId, table.deviceId, table.requestedAt.desc()),
    check("device_screenshot_requests_status", sql`${table.status} IN ('queued', 'succeeded', 'failed')`),
  ],
);

export const deviceScreenshots = pgTable(
  "device_screenshots",
  {
    id: uuid("id").primaryKey(),
    workspaceId: uuid("workspace_id").notNull(),
    deviceId: uuid("device_id").notNull(),
    requestId: uuid("request_id"),
    trigger: text("trigger").notNull(),
    capturedAt: timestampColumn("captured_at").notNull(),
    appBundleId: text("app_bundle_id"),
    pixelWidth: integer("pixel_width").notNull(),
    pixelHeight: integer("pixel_height").notNull(),
    objectKey: text("object_key").notNull(),
    expectedSha256: char("expected_sha256", { length: 64 }).notNull(),
    expectedSizeBytes: bigint("expected_size_bytes", { mode: "number" }).notNull(),
    expectedMimeType: text("expected_mime_type").notNull(),
    state: text("state").notNull().default("prepared"),
    preparedAt: timestampColumn("prepared_at").notNull(),
    completedAt: timestampColumn("completed_at"),
  },
  (table) => [
    foreignKey({
      columns: [table.workspaceId, table.deviceId],
      foreignColumns: [devices.workspaceId, devices.id],
    }).onDelete("cascade"),
    foreignKey({
      columns: [table.requestId],
      foreignColumns: [deviceScreenshotRequests.id],
    }).onDelete("set null"),
    uniqueIndex("device_screenshots_request_unique").on(table.requestId),
    index("device_screenshots_chronology").on(table.workspaceId, table.deviceId, table.capturedAt.desc()),
    check("device_screenshots_trigger", sql`${table.trigger} IN ('manual', 'scheduled', 'activity')`),
    check("device_screenshots_dimensions", sql`${table.pixelWidth} > 0 AND ${table.pixelHeight} > 0`),
    check("device_screenshots_size", sql`${table.expectedSizeBytes} > 0`),
    check("device_screenshots_sha256", sql`${table.expectedSha256} ~ '^[0-9a-f]{64}$'`),
    check("device_screenshots_mime", sql`${table.expectedMimeType} = 'image/jpeg'`),
    check("device_screenshots_state", sql`${table.state} IN ('prepared', 'completed')`),
  ],
);

export const deviceRevocationAudit = pgTable(
  "device_revocation_audit",
  {
    id: uuid("id").primaryKey(),
    workspaceId: uuid("workspace_id").notNull(),
    deviceId: uuid("device_id").notNull(),
    actorUserId: uuid("actor_user_id").notNull(),
    revokedAt: timestampColumn("revoked_at").notNull(),
  },
  (table) => [
    foreignKey({
      columns: [table.workspaceId, table.deviceId],
      foreignColumns: [devices.workspaceId, devices.id],
    }).onDelete("cascade"),
    foreignKey({
      name: "device_revocation_audit_actor_membership_fk",
      columns: [table.workspaceId, table.actorUserId],
      foreignColumns: [workspaceMembers.workspaceId, workspaceMembers.userId],
    }).onDelete("restrict"),
    index("idx_device_revocation_audit_chronology").on(
      table.workspaceId,
      table.deviceId,
      table.revokedAt.desc(),
    ),
  ],
);

export const systemEvents = pgTable(
  "system_events",
  {
    eventId: uuid("event_id").primaryKey(),
    workspaceId: uuid("workspace_id").notNull(),
    deviceId: uuid("device_id").notNull(),
    eventType: text("event_type").notNull(),
    source: text("source").notNull(),
    schemaVersion: integer("schema_version").notNull(),
    occurredAt: timestampColumn("occurred_at").notNull(),
    createdAt: timestampColumn("created_at").notNull(),
    sensitivity: text("sensitivity").notNull(),
    payload: jsonb("payload").$type<Record<string, unknown>>().notNull(),
    idempotencyKey: text("idempotency_key"),
  },
  (table) => [
    foreignKey({
      columns: [table.workspaceId, table.deviceId],
      foreignColumns: [devices.workspaceId, devices.id],
    }).onDelete("cascade"),
    index("idx_system_events_device_chronology").on(
      table.workspaceId,
      table.deviceId,
      table.occurredAt.desc(),
    ),
  ],
);

export const communicationEvents = pgTable(
  "communication_events",
  {
    eventId: uuid("event_id").primaryKey(),
    workspaceId: uuid("workspace_id").notNull(),
    deviceId: uuid("device_id").notNull(),
    eventType: text("event_type").notNull(),
    source: text("source").notNull(),
    schemaVersion: integer("schema_version").notNull(),
    occurredAt: timestampColumn("occurred_at").notNull(),
    createdAt: timestampColumn("created_at").notNull(),
    sensitivity: text("sensitivity").notNull(),
    payload: jsonb("payload").$type<Record<string, unknown>>().notNull(),
    attachmentRefs: jsonb("attachment_refs").$type<string[]>().notNull(),
    idempotencyKey: text("idempotency_key"),
  },
  (table) => [
    foreignKey({
      columns: [table.workspaceId, table.deviceId],
      foreignColumns: [devices.workspaceId, devices.id],
    }).onDelete("cascade"),
    index("idx_communication_events_device_chronology").on(
      table.workspaceId,
      table.deviceId,
      table.occurredAt.desc(),
    ),
    uniqueIndex("communication_events_idempotency_unique")
      .on(table.workspaceId, table.deviceId, table.idempotencyKey)
      .where(sql`${table.idempotencyKey} IS NOT NULL`),
  ],
);

export const communicationConversations = pgTable(
  "communication_conversations",
  {
    workspaceId: uuid("workspace_id").notNull(),
    deviceId: uuid("device_id").notNull(),
    conversationId: text("conversation_id").notNull(),
    displayName: text("display_name").notNull().default(""),
    avatarUrl: text("avatar_url"),
    scope: text("scope").notNull(),
    memberCount: integer("member_count"),
    lastMessageAt: timestampColumn("last_message_at").notNull(),
  },
  (table) => [
    primaryKey({ columns: [table.workspaceId, table.deviceId, table.conversationId] }),
    foreignKey({
      columns: [table.workspaceId, table.deviceId],
      foreignColumns: [devices.workspaceId, devices.id],
    }).onDelete("cascade"),
    check(
      "communication_conversations_scope_members",
      sql`(${table.scope} = 'direct' AND ${table.memberCount} IS NULL) OR (${table.scope} = 'group' AND ${table.memberCount} BETWEEN 1 AND 15)`,
    ),
  ],
);

export const communicationMessages = pgTable(
  "communication_messages",
  {
    eventId: uuid("event_id")
      .primaryKey()
      .references(() => communicationEvents.eventId, { onDelete: "cascade" }),
    workspaceId: uuid("workspace_id").notNull(),
    deviceId: uuid("device_id").notNull(),
    conversationId: text("conversation_id").notNull(),
    messageId: text("message_id").notNull(),
    senderId: text("sender_id").notNull().default(""),
    senderDisplayName: text("sender_display_name").notNull().default(""),
    senderAvatarUrl: text("sender_avatar_url"),
    sourceKey: text("source_key").notNull(),
    occurredAt: timestampColumn("occurred_at").notNull(),
    direction: text("direction").notNull(),
    kind: text("kind").notNull(),
    textBody: text("text_body"),
  },
  (table) => [
    foreignKey({
      columns: [table.workspaceId, table.deviceId, table.conversationId],
      foreignColumns: [
        communicationConversations.workspaceId,
        communicationConversations.deviceId,
        communicationConversations.conversationId,
      ],
    }).onDelete("cascade"),
    unique("communication_messages_source_key_unique").on(
      table.workspaceId,
      table.deviceId,
      table.sourceKey,
    ),
    unique("communication_messages_message_id_unique").on(
      table.workspaceId,
      table.deviceId,
      table.messageId,
    ),
    uniqueIndex("communication_messages_workspace_device_event_unique").on(
      table.workspaceId,
      table.deviceId,
      table.eventId,
    ),
    index("idx_communication_messages_device_conversation_chronology").on(
      table.workspaceId,
      table.deviceId,
      table.conversationId,
      table.occurredAt.desc(),
    ),
    check(
      "communication_messages_direction",
      sql`${table.direction} IN ('incoming', 'outgoing')`,
    ),
    check(
      "communication_messages_kind",
      sql`${table.kind} IN ('text', 'audio', 'image', 'video', 'file')`,
    ),
    check(
      "communication_messages_text_body",
      sql`(${table.kind} = 'text' AND ${table.textBody} IS NOT NULL) OR (${table.kind} <> 'text' AND ${table.textBody} IS NULL)`,
    ),
  ],
);

export const communicationMessageAttachments = pgTable(
  "communication_message_attachments",
  {
    eventId: uuid("event_id").notNull(),
    attachmentId: text("attachment_id").notNull(),
    kind: text("kind").notNull(),
    sha256: char("sha256", { length: 64 }).notNull(),
    sizeBytes: bigint("size_bytes", { mode: "number" }).notNull(),
    mimeType: text("mime_type").notNull(),
    fileName: text("file_name"),
  },
  (table) => [
    primaryKey({ columns: [table.eventId, table.attachmentId] }),
    check("communication_message_attachments_kind", sql`${table.kind} IN ('audio', 'image', 'video', 'file')`),
    check("communication_message_attachments_hash", sql`${table.sha256} ~ '^[a-f0-9]{64}$'`),
    check("communication_message_attachments_size", sql`${table.sizeBytes} > 0`),
  ],
);

export const communicationObjects = pgTable(
  "communication_objects",
  {
    objectId: uuid("object_id").primaryKey(),
    workspaceId: uuid("workspace_id").notNull(),
    deviceId: uuid("device_id").notNull(),
    eventId: uuid("event_id")
      .notNull()
      .references(() => communicationMessages.eventId, { onDelete: "cascade" }),
    attachmentId: text("attachment_id").notNull(),
    objectKey: text("object_key").notNull(),
    expectedSha256: char("expected_sha256", { length: 64 }).notNull(),
    expectedSizeBytes: bigint("expected_size_bytes", { mode: "number" }).notNull(),
    expectedMimeType: text("expected_mime_type").notNull(),
    state: text("state").notNull(),
    preparedAt: timestampColumn("prepared_at").notNull(),
    completedAt: timestampColumn("completed_at"),
  },
  (table) => [
    foreignKey({
      columns: [table.workspaceId, table.deviceId],
      foreignColumns: [devices.workspaceId, devices.id],
    }).onDelete("cascade"),
    foreignKey({
      columns: [table.workspaceId, table.deviceId, table.eventId],
      foreignColumns: [
        communicationMessages.workspaceId,
        communicationMessages.deviceId,
        communicationMessages.eventId,
      ],
    }).onDelete("cascade"),
    foreignKey({
      columns: [table.eventId, table.attachmentId],
      foreignColumns: [communicationMessageAttachments.eventId, communicationMessageAttachments.attachmentId],
    }).onDelete("cascade"),
    unique("communication_objects_event_attachment_unique").on(table.eventId, table.attachmentId),
    uniqueIndex("communication_objects_key_unique").on(table.objectKey),
    index("idx_communication_objects_owner")
      .on(table.workspaceId, table.deviceId, table.objectId)
      .where(sql`${table.state} = 'completed'`),
    check("communication_objects_key", sql`${table.objectKey} ~ '^communication/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'`),
    check("communication_objects_hash", sql`${table.expectedSha256} ~ '^[a-f0-9]{64}$'`),
    check("communication_objects_size", sql`${table.expectedSizeBytes} > 0`),
    check("communication_objects_state", sql`${table.state} IN ('prepared', 'completed')`),
    check(
      "communication_objects_completed_at",
      sql`(${table.state} = 'prepared' AND ${table.completedAt} IS NULL) OR (${table.state} = 'completed' AND ${table.completedAt} IS NOT NULL)`,
    ),
  ],
);

export const photoLibraryAssets = pgTable(
  "photo_library_assets",
  {
    id: uuid("id").primaryKey(),
    workspaceId: uuid("workspace_id").notNull(),
    deviceId: uuid("device_id").notNull(),
    eventId: uuid("event_id").notNull().references(() => systemEvents.eventId, { onDelete: "cascade" }),
    assetId: text("asset_id").notNull(),
    capturedAt: timestampColumn("captured_at").notNull(),
    mediaType: text("media_type").notNull(),
    originalFilename: text("original_filename").notNull(),
    mimeType: text("mime_type").notNull(),
    pixelWidth: integer("pixel_width").notNull(),
    pixelHeight: integer("pixel_height").notNull(),
    durationSeconds: doublePrecision("duration_seconds").notNull(),
    albumNames: jsonb("album_names").$type<string[]>().notNull(),
    objectKey: text("object_key").notNull(),
    expectedSha256: char("expected_sha256", { length: 64 }).notNull(),
    expectedSizeBytes: bigint("expected_size_bytes", { mode: "number" }).notNull(),
    state: text("state").notNull(),
    preparedAt: timestampColumn("prepared_at").notNull(),
    completedAt: timestampColumn("completed_at"),
  },
  (table) => [
    foreignKey({ columns: [table.workspaceId, table.deviceId], foreignColumns: [devices.workspaceId, devices.id] }).onDelete("cascade"),
    unique("photo_library_assets_event_unique").on(table.eventId),
    unique("photo_library_assets_source_unique").on(table.workspaceId, table.deviceId, table.assetId),
    uniqueIndex("photo_library_assets_key_unique").on(table.objectKey),
    index("idx_photo_library_assets_owner_chronology").on(table.workspaceId, table.deviceId, table.capturedAt.desc()).where(sql`${table.state} = 'completed'`),
    check("photo_library_assets_media_type", sql`${table.mediaType} IN ('image', 'video')`),
    check("photo_library_assets_dimensions", sql`${table.pixelWidth} > 0 AND ${table.pixelHeight} > 0`),
    check("photo_library_assets_duration", sql`${table.durationSeconds} >= 0`),
    check("photo_library_assets_hash", sql`${table.expectedSha256} ~ '^[a-f0-9]{64}$'`),
    check("photo_library_assets_size", sql`${table.expectedSizeBytes} > 0`),
    check("photo_library_assets_state", sql`${table.state} IN ('prepared', 'completed')`),
    check(
      "photo_library_assets_completed_at",
      sql`(${table.state} = 'prepared' AND ${table.completedAt} IS NULL) OR (${table.state} = 'completed' AND ${table.completedAt} IS NOT NULL)`,
    ),
  ],
);

export const cloudSchema = {
  authUsers,
  authSessions,
  authAccounts,
  workspaces,
  workspaceMembers,
  devices,
  deviceCredentialGenerations,
  pairingSessions,
  pairingAuthorizationCodes,
  collectorConfigs,
  collectorConfigAudit,
  deviceHeartbeats,
  deviceCollectorHealth,
  deviceNetworkHistory,
  deviceMediaCleanupRequests,
  deviceScreenshotRequests,
  deviceScreenshots,
  networkLocationLibrary,
  deviceRevocationAudit,
  communicationEvents,
  communicationConversations,
  communicationMessages,
  communicationMessageAttachments,
  communicationObjects,
  photoLibraryAssets,
  systemEvents,
};
