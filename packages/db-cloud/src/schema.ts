import { isNull, sql } from "drizzle-orm";
import {
  bigint,
  boolean,
  char,
  check,
  foreignKey,
  index,
  inet,
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
    sessionTokenHash: char("session_token_hash", { length: 64 }),
    sessionToken: text("session_token"),
    expiresAt: timestampColumn("expires_at").notNull(),
    ipAddress: inet("ip_address"),
    userAgent: text("user_agent"),
    createdAt: timestampColumn("created_at").notNull(),
    updatedAt: timestampColumn("updated_at").notNull(),
  },
  (table) => [
    uniqueIndex("auth_sessions_token_hash_unique").on(table.sessionTokenHash),
    uniqueIndex("auth_sessions_session_token_unique")
      .on(table.sessionToken)
      .where(sql`${table.sessionToken} IS NOT NULL`),
    check(
      "auth_sessions_token_hash_hex",
      sql`${table.sessionTokenHash} IS NULL OR ${table.sessionTokenHash} ~ '^[0-9a-f]{64}$'`,
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
  ],
);

export const pairingSessions = pgTable(
  "pairing_sessions",
  {
    sessionIdHash: char("session_id_hash", { length: 64 }).primaryKey(),
    devicePublicKeyHash: char("device_public_key_hash", { length: 64 }).notNull(),
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
    updatedAt: timestampColumn("updated_at").notNull(),
  },
  (table) => [
    primaryKey({ columns: [table.workspaceId, table.deviceId] }),
    foreignKey({
      columns: [table.workspaceId, table.deviceId],
      foreignColumns: [devices.workspaceId, devices.id],
    }).onDelete("cascade"),
    check("collector_configs_revision_nonnegative", sql`${table.configurationRevision} >= 0`),
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
    check(
      "device_heartbeats_presence",
      sql`${table.presence} IN ('online', 'stale', 'offline', 'sleeping')`,
    ),
    check("device_heartbeats_outbox_nonnegative", sql`${table.outboxDepth} >= 0`),
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
  deviceRevocationAudit,
};
