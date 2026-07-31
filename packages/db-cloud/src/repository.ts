import type { AgentControlSnapshot } from "@pca/contracts/src/types.js";
import { and, desc, eq, gt, isNull, sql } from "drizzle-orm";
import { randomUUID, timingSafeEqual } from "node:crypto";
import type { NodePgDatabase } from "drizzle-orm/node-postgres";

import {
  authUsers,
  cloudSchema,
  collectorConfigAudit,
  collectorConfigs,
  deviceCredentialGenerations,
  deviceHeartbeats,
  devices,
  deviceRevocationAudit,
  pairingAuthorizationCodes,
  pairingSessions,
  workspaceMembers,
  workspaces,
  type StoredCollectorConfig,
} from "./schema.js";

export type ControlRepositoryErrorCode =
  | "CONFLICT"
  | "CREDENTIAL_INVALID"
  | "DEVICE_NOT_FOUND"
  | "DEVICE_REVOKED"
  | "PAIRING_EXPIRED"
  | "PAIRING_REPLAYED"
  | "PKCE_INVALID"
  | "WORKSPACE_FORBIDDEN";

export class ControlRepositoryError extends Error {
  constructor(readonly code: ControlRepositoryErrorCode) {
    super(code);
    this.name = "ControlRepositoryError";
  }
}

export interface PairingSessionInput {
  sessionIdHash: string;
  devicePublicKeyHash: string;
  codeChallenge: string;
  callbackUri: string;
  callbackStateHash: string;
  expiresAt: Date;
  createdAt: Date;
}

export interface PairingSession extends PairingSessionInput {
  authorizedAt: Date | null;
}

export interface AuthorizePairingSessionInput {
  sessionIdHash: string;
  authorizationCodeHash: string;
  callbackStateHash: string;
  workspaceId: string;
  ownerUserId: string;
  expiresAt: Date;
  now: Date;
}

export interface CodeExchangeInput {
  sessionIdHash: string;
  authorizationCodeHash: string;
  codeChallenge: string;
  deviceId: string;
  accessTokenHash: string;
  refreshTokenHash: string;
  accessExpiresAt: Date;
  refreshExpiresAt: Date;
  now: Date;
}

export interface CredentialRotationInput {
  workspaceId: string;
  deviceId: string;
  currentRefreshTokenHash: string;
  newAccessTokenHash: string;
  newRefreshTokenHash: string;
  accessExpiresAt: Date;
  refreshExpiresAt: Date;
  now: Date;
}

export interface DeviceCredentialGrant {
  workspaceId: string;
  deviceId: string;
  credentialGeneration: number;
  accessExpiresAt: Date;
  refreshExpiresAt: Date;
}

export interface HeartbeatInput {
  heartbeatId: string;
  workspaceId: string;
  deviceId: string;
  receivedAt: Date;
  agentVersion: string;
  presence: "online" | "stale" | "offline" | "sleeping";
  outboxDepth: number;
}

export interface ConfigAuditInput {
  auditId: string;
  actorUserId: string;
  workspaceId: string;
  deviceId: string;
  config: StoredCollectorConfig;
  now: Date;
}

export interface DeviceCredentialAuthentication {
  workspaceId: string;
  deviceId: string;
}

export interface DeviceRevocationInput {
  auditId: string;
  actorUserId: string;
  workspaceId: string;
  deviceId: string;
  now: Date;
}

export interface OwnerWorkspace {
  workspaceId: string;
  name: string;
}

export interface DeviceStatus {
  presence: HeartbeatInput["presence"];
  agentVersion: string;
  outboxDepth: number;
  observedAt: Date;
}

export interface OwnerDeviceSummary {
  deviceId: string;
  workspaceId: string;
  platform: "macos";
  pairedAt: Date;
  revoked: boolean;
  configurationRevision: number;
  status: DeviceStatus | null;
}

export interface OwnerDeviceDetail extends OwnerDeviceSummary {
  snapshot: AgentControlSnapshot;
}

export interface CollectorConfigAuditRecord {
  actorUserId: string;
  configurationRevision: number;
  oldConfig: StoredCollectorConfig;
  newConfig: StoredCollectorConfig;
  createdAt: Date;
}

export interface ControlRepository {
  createPairingSession(input: PairingSessionInput): Promise<PairingSession>;
  authorizePairingSession(input: AuthorizePairingSessionInput): Promise<string>;
  consumeAuthorizationCode(input: CodeExchangeInput): Promise<DeviceCredentialGrant>;
  rotateDeviceCredentials(input: CredentialRotationInput): Promise<DeviceCredentialGrant>;
  authenticateDeviceAccess(
    accessTokenHash: string,
    now: Date,
  ): Promise<DeviceCredentialAuthentication>;
  authenticateDeviceRefresh(
    refreshTokenHash: string,
    now: Date,
  ): Promise<DeviceCredentialAuthentication>;
  loadControlSnapshot(deviceId: string, workspaceId: string): Promise<AgentControlSnapshot>;
  recordHeartbeat(input: HeartbeatInput): Promise<void>;
  appendConfigAudit(input: ConfigAuditInput): Promise<number>;
  revokeDevice(input: DeviceRevocationInput): Promise<void>;
  resolveOwnerWorkspace(userId: string): Promise<string | null>;
  bootstrapOwnerWorkspace(userId: string): Promise<OwnerWorkspace>;
  listOwnerWorkspaces(userId: string): Promise<OwnerWorkspace[]>;
  listOwnerDevices(workspaceId: string, userId: string): Promise<OwnerDeviceSummary[]>;
  loadOwnerDevice(
    deviceId: string,
    workspaceId: string,
    userId: string,
  ): Promise<OwnerDeviceDetail>;
  listCollectorConfigAudit(
    deviceId: string,
    workspaceId: string,
    userId: string,
  ): Promise<CollectorConfigAuditRecord[]>;
}

export interface OwnerMembership {
  workspaceId: string;
  userId: string;
}

interface AuthorizationCodeRecord extends AuthorizePairingSessionInput {
  consumedAt: Date | null;
}

interface DeviceRecord {
  id: string;
  workspaceId: string;
  ownerUserId: string;
  devicePublicKeyHash: string;
  platform: "macos";
  createdAt: Date;
  revokedAt: Date | null;
}

interface CredentialRecord extends DeviceCredentialGrant {
  accessTokenHash: string;
  refreshTokenHash: string;
  revokedAt: Date | null;
}

interface ConfigRecord extends StoredCollectorConfig {
  configurationRevision: number;
}

export class MemoryControlRepository implements ControlRepository {
  readonly #sessions = new Map<string, PairingSession>();
  readonly #authorizationCodes = new Map<string, AuthorizationCodeRecord>();
  readonly #devices = new Map<string, DeviceRecord>();
  readonly #credentials = new Map<string, CredentialRecord[]>();
  readonly #configs = new Map<string, ConfigRecord>();
  readonly #heartbeatIds = new Set<string>();
  readonly #auditIds = new Set<string>();
  readonly #ownerMemberships = new Set<string>();
  readonly #devicePublicKeyHashes = new Set<string>();
  readonly #accessTokenHashes = new Set<string>();
  readonly #refreshTokenHashes = new Set<string>();
  readonly #revocationAuditIds = new Set<string>();
  readonly #workspaceNames = new Map<string, string>();
  readonly #latestHeartbeats = new Map<string, DeviceStatus>();
  readonly #configAudit: Array<CollectorConfigAuditRecord & { workspaceId: string; deviceId: string }> = [];

  constructor(memberships: readonly OwnerMembership[] = []) {
    for (const membership of memberships) {
      this.#ownerMemberships.add(membershipKey(membership.workspaceId, membership.userId));
      this.#workspaceNames.set(membership.workspaceId, "Personal Computer Agent");
    }
  }

  async createPairingSession(input: PairingSessionInput): Promise<PairingSession> {
    if (this.#sessions.has(input.sessionIdHash)) {
      throw new ControlRepositoryError("CONFLICT");
    }
    const session = { ...input, authorizedAt: null };
    this.#sessions.set(input.sessionIdHash, session);
    return { ...session };
  }

  async authorizePairingSession(input: AuthorizePairingSessionInput): Promise<string> {
    this.#requireOwnerMembership(input.workspaceId, input.ownerUserId);
    const session = this.#sessions.get(input.sessionIdHash);
    if (session === undefined || session.expiresAt <= input.now) {
      throw new ControlRepositoryError("PAIRING_EXPIRED");
    }
    if (session.authorizedAt !== null || this.#authorizationCodes.has(input.authorizationCodeHash)) {
      throw new ControlRepositoryError("CONFLICT");
    }
    if (!secureEqual(session.callbackStateHash, input.callbackStateHash)) {
      throw new ControlRepositoryError("PAIRING_EXPIRED");
    }
    session.authorizedAt = input.now;
    this.#authorizationCodes.set(input.authorizationCodeHash, {
      ...input,
      consumedAt: null,
    });
    return session.callbackUri;
  }

  async consumeAuthorizationCode(input: CodeExchangeInput): Promise<DeviceCredentialGrant> {
    const code = this.#authorizationCodes.get(input.authorizationCodeHash);
    const session = this.#sessions.get(input.sessionIdHash);
    if (
      code === undefined ||
      session === undefined ||
      code.sessionIdHash !== input.sessionIdHash ||
      code.expiresAt <= input.now ||
      session.expiresAt <= input.now
    ) {
      throw new ControlRepositoryError("PAIRING_EXPIRED");
    }
    if (code.consumedAt !== null) {
      throw new ControlRepositoryError("PAIRING_REPLAYED");
    }
    if (!secureEqual(session.codeChallenge, input.codeChallenge)) {
      throw new ControlRepositoryError("PKCE_INVALID");
    }
    if (this.#devices.has(input.deviceId)) {
      throw new ControlRepositoryError("CONFLICT");
    }
    if (
      this.#devicePublicKeyHashes.has(session.devicePublicKeyHash) ||
      this.#accessTokenHashes.has(input.accessTokenHash) ||
      this.#refreshTokenHashes.has(input.refreshTokenHash)
    ) {
      throw new ControlRepositoryError("CONFLICT");
    }

    const grant: DeviceCredentialGrant = {
      workspaceId: code.workspaceId,
      deviceId: input.deviceId,
      credentialGeneration: 1,
      accessExpiresAt: input.accessExpiresAt,
      refreshExpiresAt: input.refreshExpiresAt,
    };
    code.consumedAt = input.now;
    this.#devices.set(input.deviceId, {
      id: input.deviceId,
      workspaceId: code.workspaceId,
      ownerUserId: code.ownerUserId,
      devicePublicKeyHash: session.devicePublicKeyHash,
      platform: "macos",
      createdAt: input.now,
      revokedAt: null,
    });
    this.#devicePublicKeyHashes.add(session.devicePublicKeyHash);
    this.#accessTokenHashes.add(input.accessTokenHash);
    this.#refreshTokenHashes.add(input.refreshTokenHash);
    this.#credentials.set(input.deviceId, [
      {
        ...grant,
        accessTokenHash: input.accessTokenHash,
        refreshTokenHash: input.refreshTokenHash,
        revokedAt: null,
      },
    ]);
    this.#configs.set(configKey(code.workspaceId, input.deviceId), {
      configurationRevision: 0,
      networkEnabled: false,
      wechatEnabled: false,
    });
    return grant;
  }

  async rotateDeviceCredentials(input: CredentialRotationInput): Promise<DeviceCredentialGrant> {
    const device = this.#requireDevice(input.deviceId, input.workspaceId, false);
    if (device.revokedAt !== null) {
      throw new ControlRepositoryError("DEVICE_REVOKED");
    }
    const records = this.#credentials.get(input.deviceId) ?? [];
    const current = records.find(
      (record) =>
        record.refreshTokenHash === input.currentRefreshTokenHash &&
        record.revokedAt === null &&
        record.refreshExpiresAt > input.now,
    );
    if (current === undefined) {
      throw new ControlRepositoryError("CREDENTIAL_INVALID");
    }
    if (
      this.#accessTokenHashes.has(input.newAccessTokenHash) ||
      this.#refreshTokenHashes.has(input.newRefreshTokenHash)
    ) {
      throw new ControlRepositoryError("CONFLICT");
    }
    current.revokedAt = input.now;
    const grant: DeviceCredentialGrant = {
      workspaceId: input.workspaceId,
      deviceId: input.deviceId,
      credentialGeneration: current.credentialGeneration + 1,
      accessExpiresAt: input.accessExpiresAt,
      refreshExpiresAt: input.refreshExpiresAt,
    };
    records.push({
      ...grant,
      accessTokenHash: input.newAccessTokenHash,
      refreshTokenHash: input.newRefreshTokenHash,
      revokedAt: null,
    });
    this.#accessTokenHashes.add(input.newAccessTokenHash);
    this.#refreshTokenHashes.add(input.newRefreshTokenHash);
    return grant;
  }

  async authenticateDeviceAccess(
    accessTokenHash: string,
    now: Date,
  ): Promise<DeviceCredentialAuthentication> {
    return this.#authenticateCredential(accessTokenHash, now, "accessTokenHash", "accessExpiresAt");
  }

  async authenticateDeviceRefresh(
    refreshTokenHash: string,
    now: Date,
  ): Promise<DeviceCredentialAuthentication> {
    return this.#authenticateCredential(refreshTokenHash, now, "refreshTokenHash", "refreshExpiresAt");
  }

  async loadControlSnapshot(
    deviceId: string,
    workspaceId: string,
  ): Promise<AgentControlSnapshot> {
    const device = this.#requireDevice(deviceId, workspaceId, true);
    const config = this.#configs.get(configKey(workspaceId, deviceId)) ?? {
      configurationRevision: 0,
      networkEnabled: false,
      wechatEnabled: false,
    };
    return snapshot(device, config);
  }

  async recordHeartbeat(input: HeartbeatInput): Promise<void> {
    this.#requireDevice(input.deviceId, input.workspaceId, false);
    if (this.#heartbeatIds.has(input.heartbeatId)) {
      throw new ControlRepositoryError("CONFLICT");
    }
    this.#heartbeatIds.add(input.heartbeatId);
    this.#latestHeartbeats.set(input.deviceId, {
      presence: input.presence,
      agentVersion: input.agentVersion,
      outboxDepth: input.outboxDepth,
      observedAt: input.receivedAt,
    });
  }

  async appendConfigAudit(input: ConfigAuditInput): Promise<number> {
    this.#requireOwnerMembership(input.workspaceId, input.actorUserId);
    this.#requireDevice(input.deviceId, input.workspaceId, false);
    if (this.#auditIds.has(input.auditId)) {
      throw new ControlRepositoryError("CONFLICT");
    }
    const key = configKey(input.workspaceId, input.deviceId);
    const current = this.#configs.get(key) ?? {
      configurationRevision: 0,
      networkEnabled: false,
      wechatEnabled: false,
    };
    const revision = current.configurationRevision + 1;
    this.#configs.set(key, { ...input.config, configurationRevision: revision });
    this.#auditIds.add(input.auditId);
    this.#configAudit.push({
      workspaceId: input.workspaceId,
      deviceId: input.deviceId,
      actorUserId: input.actorUserId,
      configurationRevision: revision,
      oldConfig: {
        networkEnabled: current.networkEnabled,
        wechatEnabled: current.wechatEnabled,
      },
      newConfig: { ...input.config },
      createdAt: input.now,
    });
    return revision;
  }

  async revokeDevice(input: DeviceRevocationInput): Promise<void> {
    this.#requireOwnerMembership(input.workspaceId, input.actorUserId);
    const device = this.#requireDevice(input.deviceId, input.workspaceId, true);
    if (device.revokedAt !== null) {
      throw new ControlRepositoryError("DEVICE_REVOKED");
    }
    if (this.#revocationAuditIds.has(input.auditId)) {
      throw new ControlRepositoryError("CONFLICT");
    }
    device.revokedAt = input.now;
    for (const credential of this.#credentials.get(input.deviceId) ?? []) {
      if (credential.revokedAt === null) {
        credential.revokedAt = input.now;
      }
    }
    this.#revocationAuditIds.add(input.auditId);
  }

  async resolveOwnerWorkspace(userId: string): Promise<string | null> {
    for (const membership of this.#ownerMemberships) {
      const [workspaceId, memberUserId] = membership.split(":");
      if (memberUserId === userId && workspaceId !== undefined) {
        return workspaceId;
      }
    }
    return null;
  }

  async bootstrapOwnerWorkspace(userId: string): Promise<OwnerWorkspace> {
    const existingWorkspaceId = await this.resolveOwnerWorkspace(userId);
    if (existingWorkspaceId !== null) {
      return {
        workspaceId: existingWorkspaceId,
        name: this.#workspaceNames.get(existingWorkspaceId) ?? "Personal Computer Agent",
      };
    }
    const workspace: OwnerWorkspace = {
      workspaceId: randomUUID(),
      name: "Personal Computer Agent",
    };
    this.#ownerMemberships.add(membershipKey(workspace.workspaceId, userId));
    this.#workspaceNames.set(workspace.workspaceId, workspace.name);
    return workspace;
  }

  async listOwnerWorkspaces(userId: string): Promise<OwnerWorkspace[]> {
    const results: OwnerWorkspace[] = [];
    for (const membership of this.#ownerMemberships) {
      const [workspaceId, memberUserId] = membership.split(":");
      if (memberUserId === userId && workspaceId !== undefined) {
        results.push({
          workspaceId,
          name: this.#workspaceNames.get(workspaceId) ?? "Personal Computer Agent",
        });
      }
    }
    return results;
  }

  async listOwnerDevices(workspaceId: string, userId: string): Promise<OwnerDeviceSummary[]> {
    this.#requireOwnerMembership(workspaceId, userId);
    return [...this.#devices.values()]
      .filter((device) => device.workspaceId === workspaceId)
      .map((device) => this.#ownerDeviceSummary(device));
  }

  async loadOwnerDevice(
    deviceId: string,
    workspaceId: string,
    userId: string,
  ): Promise<OwnerDeviceDetail> {
    this.#requireOwnerMembership(workspaceId, userId);
    const device = this.#requireDevice(deviceId, workspaceId, true);
    const summary = this.#ownerDeviceSummary(device);
    return { ...summary, snapshot: await this.loadControlSnapshot(deviceId, workspaceId) };
  }

  async listCollectorConfigAudit(
    deviceId: string,
    workspaceId: string,
    userId: string,
  ): Promise<CollectorConfigAuditRecord[]> {
    this.#requireOwnerMembership(workspaceId, userId);
    this.#requireDevice(deviceId, workspaceId, true);
    return this.#configAudit
      .filter((record) => record.workspaceId === workspaceId && record.deviceId === deviceId)
      .map(({ workspaceId: _workspaceId, deviceId: _deviceId, ...record }) => ({ ...record }))
      .reverse();
  }

  #ownerDeviceSummary(device: DeviceRecord): OwnerDeviceSummary {
    const config = this.#configs.get(configKey(device.workspaceId, device.id)) ?? {
      configurationRevision: 0,
      networkEnabled: false,
      wechatEnabled: false,
    };
    return {
      deviceId: device.id,
      workspaceId: device.workspaceId,
      platform: device.platform,
      pairedAt: device.createdAt,
      revoked: device.revokedAt !== null,
      configurationRevision: config.configurationRevision,
      status: this.#latestHeartbeats.get(device.id) ?? null,
    };
  }

  #authenticateCredential(
    credentialHash: string,
    now: Date,
    hashField: "accessTokenHash" | "refreshTokenHash",
    expiresField: "accessExpiresAt" | "refreshExpiresAt",
  ): DeviceCredentialAuthentication {
    for (const [deviceId, credentials] of this.#credentials) {
      const credential = credentials.find((record) => record[hashField] === credentialHash);
      if (credential === undefined || credential[expiresField] <= now) {
        continue;
      }
      const device = this.#devices.get(deviceId);
      if (device === undefined) {
        throw new ControlRepositoryError("DEVICE_NOT_FOUND");
      }
      if (device.revokedAt !== null) {
        throw new ControlRepositoryError("DEVICE_REVOKED");
      }
      if (credential.revokedAt !== null) {
        throw new ControlRepositoryError("CREDENTIAL_INVALID");
      }
      return { workspaceId: device.workspaceId, deviceId: device.id };
    }
    throw new ControlRepositoryError("CREDENTIAL_INVALID");
  }

  #requireDevice(deviceId: string, workspaceId: string, allowRevoked: boolean): DeviceRecord {
    const device = this.#devices.get(deviceId);
    if (device === undefined) {
      throw new ControlRepositoryError("DEVICE_NOT_FOUND");
    }
    if (device.workspaceId !== workspaceId) {
      throw new ControlRepositoryError("WORKSPACE_FORBIDDEN");
    }
    if (!allowRevoked && device.revokedAt !== null) {
      throw new ControlRepositoryError("DEVICE_REVOKED");
    }
    return device;
  }

  #requireOwnerMembership(workspaceId: string, userId: string): void {
    if (!this.#ownerMemberships.has(membershipKey(workspaceId, userId))) {
      throw new ControlRepositoryError("WORKSPACE_FORBIDDEN");
    }
  }

}

export class DrizzleControlRepository implements ControlRepository {
  constructor(private readonly database: NodePgDatabase<typeof cloudSchema>) {}

  async createPairingSession(input: PairingSessionInput): Promise<PairingSession> {
    try {
      const [created] = await this.database
        .insert(pairingSessions)
        .values({ ...input, authorizedAt: null })
        .returning();
      const callbackStateHash = created?.callbackStateHash;
      if (created === undefined || callbackStateHash === undefined || callbackStateHash === null) {
        throw new ControlRepositoryError("CONFLICT");
      }
      return { ...created, callbackStateHash };
    } catch (error) {
      throw repositoryError(error);
    }
  }

  async authorizePairingSession(input: AuthorizePairingSessionInput): Promise<string> {
    try {
      return await this.database.transaction(async (transaction) => {
        await requireDatabaseOwnerMembership(
          transaction,
          input.workspaceId,
          input.ownerUserId,
        );
        const [authorized] = await transaction
          .update(pairingSessions)
          .set({ authorizedAt: input.now })
          .where(
            and(
              eq(pairingSessions.sessionIdHash, input.sessionIdHash),
              eq(pairingSessions.callbackStateHash, input.callbackStateHash),
              isNull(pairingSessions.authorizedAt),
              gt(pairingSessions.expiresAt, input.now),
            ),
          )
          .returning({ callbackUri: pairingSessions.callbackUri });
        if (authorized === undefined) {
          throw new ControlRepositoryError("PAIRING_EXPIRED");
        }
        await transaction.insert(pairingAuthorizationCodes).values({
          authorizationCodeHash: input.authorizationCodeHash,
          sessionIdHash: input.sessionIdHash,
          workspaceId: input.workspaceId,
          ownerUserId: input.ownerUserId,
          callbackStateHash: input.callbackStateHash,
          expiresAt: input.expiresAt,
          createdAt: input.now,
          consumedAt: null,
        });
        return authorized.callbackUri;
      });
    } catch (error) {
      throw repositoryError(error);
    }
  }

  async consumeAuthorizationCode(input: CodeExchangeInput): Promise<DeviceCredentialGrant> {
    try {
      return await this.database.transaction(async (transaction) => {
        const [binding] = await transaction
          .select({
            workspaceId: pairingAuthorizationCodes.workspaceId,
            ownerUserId: pairingAuthorizationCodes.ownerUserId,
            consumedAt: pairingAuthorizationCodes.consumedAt,
            codeExpiresAt: pairingAuthorizationCodes.expiresAt,
            sessionExpiresAt: pairingSessions.expiresAt,
            codeChallenge: pairingSessions.codeChallenge,
            devicePublicKeyHash: pairingSessions.devicePublicKeyHash,
          })
          .from(pairingAuthorizationCodes)
          .innerJoin(
            pairingSessions,
            eq(pairingSessions.sessionIdHash, pairingAuthorizationCodes.sessionIdHash),
          )
          .where(
            and(
              eq(pairingAuthorizationCodes.authorizationCodeHash, input.authorizationCodeHash),
              eq(pairingAuthorizationCodes.sessionIdHash, input.sessionIdHash),
            ),
          )
          .limit(1);
        if (
          binding === undefined ||
          binding.codeExpiresAt <= input.now ||
          binding.sessionExpiresAt <= input.now
        ) {
          throw new ControlRepositoryError("PAIRING_EXPIRED");
        }
        if (binding.consumedAt !== null) {
          throw new ControlRepositoryError("PAIRING_REPLAYED");
        }
        if (!secureEqual(binding.codeChallenge, input.codeChallenge)) {
          throw new ControlRepositoryError("PKCE_INVALID");
        }
        const [consumed] = await transaction
          .update(pairingAuthorizationCodes)
          .set({ consumedAt: input.now })
          .where(
            and(
              eq(pairingAuthorizationCodes.authorizationCodeHash, input.authorizationCodeHash),
              isNull(pairingAuthorizationCodes.consumedAt),
              gt(pairingAuthorizationCodes.expiresAt, input.now),
            ),
          )
          .returning({ authorizationCodeHash: pairingAuthorizationCodes.authorizationCodeHash });
        if (consumed === undefined) {
          throw new ControlRepositoryError("PAIRING_REPLAYED");
        }

        await transaction.insert(devices).values({
          id: input.deviceId,
          workspaceId: binding.workspaceId,
          ownerUserId: binding.ownerUserId,
          devicePublicKeyHash: binding.devicePublicKeyHash,
          platform: "macos",
          createdAt: input.now,
          revokedAt: null,
        });
        await transaction.insert(deviceCredentialGenerations).values({
          workspaceId: binding.workspaceId,
          deviceId: input.deviceId,
          generation: 1,
          accessTokenHash: input.accessTokenHash,
          refreshTokenHash: input.refreshTokenHash,
          accessExpiresAt: input.accessExpiresAt,
          refreshExpiresAt: input.refreshExpiresAt,
          createdAt: input.now,
          revokedAt: null,
        });
        await transaction.insert(collectorConfigs).values({
          workspaceId: binding.workspaceId,
          deviceId: input.deviceId,
          configurationRevision: 0,
          networkEnabled: false,
          wechatEnabled: false,
          updatedAt: input.now,
        });
        return {
          workspaceId: binding.workspaceId,
          deviceId: input.deviceId,
          credentialGeneration: 1,
          accessExpiresAt: input.accessExpiresAt,
          refreshExpiresAt: input.refreshExpiresAt,
        };
      });
    } catch (error) {
      throw repositoryError(error);
    }
  }

  async rotateDeviceCredentials(input: CredentialRotationInput): Promise<DeviceCredentialGrant> {
    try {
      return await this.database.transaction(async (transaction) => {
        await requireDatabaseDevice(transaction, input.deviceId, input.workspaceId, false);
        const [current] = await transaction
          .select({ generation: deviceCredentialGenerations.generation })
          .from(deviceCredentialGenerations)
          .where(
            and(
              eq(deviceCredentialGenerations.workspaceId, input.workspaceId),
              eq(deviceCredentialGenerations.deviceId, input.deviceId),
              eq(deviceCredentialGenerations.refreshTokenHash, input.currentRefreshTokenHash),
              isNull(deviceCredentialGenerations.revokedAt),
              gt(deviceCredentialGenerations.refreshExpiresAt, input.now),
            ),
          )
          .limit(1);
        if (current === undefined) {
          throw new ControlRepositoryError("CREDENTIAL_INVALID");
        }
        const [revoked] = await transaction
          .update(deviceCredentialGenerations)
          .set({ revokedAt: input.now })
          .where(
            and(
              eq(deviceCredentialGenerations.deviceId, input.deviceId),
              eq(deviceCredentialGenerations.generation, current.generation),
              isNull(deviceCredentialGenerations.revokedAt),
            ),
          )
          .returning({ generation: deviceCredentialGenerations.generation });
        if (revoked === undefined) {
          throw new ControlRepositoryError("CREDENTIAL_INVALID");
        }
        const generation = current.generation + 1;
        await transaction.insert(deviceCredentialGenerations).values({
          workspaceId: input.workspaceId,
          deviceId: input.deviceId,
          generation,
          accessTokenHash: input.newAccessTokenHash,
          refreshTokenHash: input.newRefreshTokenHash,
          accessExpiresAt: input.accessExpiresAt,
          refreshExpiresAt: input.refreshExpiresAt,
          createdAt: input.now,
          revokedAt: null,
        });
        return {
          workspaceId: input.workspaceId,
          deviceId: input.deviceId,
          credentialGeneration: generation,
          accessExpiresAt: input.accessExpiresAt,
          refreshExpiresAt: input.refreshExpiresAt,
        };
      });
    } catch (error) {
      throw repositoryError(error);
    }
  }

  async authenticateDeviceAccess(
    accessTokenHash: string,
    now: Date,
  ): Promise<DeviceCredentialAuthentication> {
    return this.#authenticateDatabaseCredential(accessTokenHash, now, "access");
  }

  async authenticateDeviceRefresh(
    refreshTokenHash: string,
    now: Date,
  ): Promise<DeviceCredentialAuthentication> {
    return this.#authenticateDatabaseCredential(refreshTokenHash, now, "refresh");
  }

  async loadControlSnapshot(
    deviceId: string,
    workspaceId: string,
  ): Promise<AgentControlSnapshot> {
    try {
      const device = await requireDatabaseDevice(this.database, deviceId, workspaceId, true);
      const [config] = await this.database
        .select()
        .from(collectorConfigs)
        .where(
          and(
            eq(collectorConfigs.workspaceId, workspaceId),
            eq(collectorConfigs.deviceId, deviceId),
          ),
        )
        .limit(1);
      return snapshot(device, {
        configurationRevision: config?.configurationRevision ?? 0,
        networkEnabled: config?.networkEnabled ?? false,
        wechatEnabled: config?.wechatEnabled ?? false,
      });
    } catch (error) {
      throw repositoryError(error);
    }
  }

  async recordHeartbeat(input: HeartbeatInput): Promise<void> {
    try {
      await requireDatabaseDevice(this.database, input.deviceId, input.workspaceId, false);
      await this.database.insert(deviceHeartbeats).values({
        id: input.heartbeatId,
        workspaceId: input.workspaceId,
        deviceId: input.deviceId,
        receivedAt: input.receivedAt,
        agentVersion: input.agentVersion,
        presence: input.presence,
        outboxDepth: input.outboxDepth,
      });
    } catch (error) {
      throw repositoryError(error);
    }
  }

  async appendConfigAudit(input: ConfigAuditInput): Promise<number> {
    try {
      return await this.database.transaction(async (transaction) => {
        await requireDatabaseOwnerMembership(
          transaction,
          input.workspaceId,
          input.actorUserId,
        );
        await requireDatabaseDevice(transaction, input.deviceId, input.workspaceId, false);
        const [current] = await transaction
          .select()
          .from(collectorConfigs)
          .where(
            and(
              eq(collectorConfigs.workspaceId, input.workspaceId),
              eq(collectorConfigs.deviceId, input.deviceId),
            ),
          )
          .limit(1);
        const oldConfig: StoredCollectorConfig = {
          networkEnabled: current?.networkEnabled ?? false,
          wechatEnabled: current?.wechatEnabled ?? false,
        };
        const revision = (current?.configurationRevision ?? 0) + 1;
        if (current === undefined) {
          await transaction.insert(collectorConfigs).values({
            workspaceId: input.workspaceId,
            deviceId: input.deviceId,
            configurationRevision: revision,
            ...input.config,
            updatedAt: input.now,
          });
        } else {
          const [updated] = await transaction
            .update(collectorConfigs)
            .set({
              configurationRevision: revision,
              ...input.config,
              updatedAt: input.now,
            })
            .where(
              and(
                eq(collectorConfigs.workspaceId, input.workspaceId),
                eq(collectorConfigs.deviceId, input.deviceId),
                eq(collectorConfigs.configurationRevision, current.configurationRevision),
              ),
            )
            .returning({ revision: collectorConfigs.configurationRevision });
          if (updated === undefined) {
            throw new ControlRepositoryError("CONFLICT");
          }
        }
        await transaction.insert(collectorConfigAudit).values({
          id: input.auditId,
          workspaceId: input.workspaceId,
          deviceId: input.deviceId,
          actorUserId: input.actorUserId,
          configurationRevision: revision,
          oldConfig,
          newConfig: input.config,
          createdAt: input.now,
        });
        return revision;
      });
    } catch (error) {
      throw repositoryError(error);
    }
  }

  async revokeDevice(input: DeviceRevocationInput): Promise<void> {
    try {
      await this.database.transaction(async (transaction) => {
        await requireDatabaseOwnerMembership(transaction, input.workspaceId, input.actorUserId);
        const device = await requireDatabaseDevice(transaction, input.deviceId, input.workspaceId, true);
        if (device.revokedAt !== null) {
          throw new ControlRepositoryError("DEVICE_REVOKED");
        }
        const [revoked] = await transaction
          .update(devices)
          .set({ revokedAt: input.now })
          .where(and(eq(devices.id, input.deviceId), isNull(devices.revokedAt)))
          .returning({ id: devices.id });
        if (revoked === undefined) {
          throw new ControlRepositoryError("DEVICE_REVOKED");
        }
        await transaction
          .update(deviceCredentialGenerations)
          .set({ revokedAt: input.now })
          .where(
            and(
              eq(deviceCredentialGenerations.deviceId, input.deviceId),
              isNull(deviceCredentialGenerations.revokedAt),
            ),
          );
        await transaction.insert(deviceRevocationAudit).values({
          id: input.auditId,
          workspaceId: input.workspaceId,
          deviceId: input.deviceId,
          actorUserId: input.actorUserId,
          revokedAt: input.now,
        });
      });
    } catch (error) {
      throw repositoryError(error);
    }
  }

  async resolveOwnerWorkspace(userId: string): Promise<string | null> {
    try {
      const [membership] = await this.database
        .select({ workspaceId: workspaceMembers.workspaceId })
        .from(workspaceMembers)
        .where(and(eq(workspaceMembers.userId, userId), eq(workspaceMembers.role, "owner")))
        .limit(1);
      return membership?.workspaceId ?? null;
    } catch (error) {
      throw repositoryError(error);
    }
  }

  async bootstrapOwnerWorkspace(userId: string): Promise<OwnerWorkspace> {
    try {
      return await this.database.transaction(async (transaction) => {
        await transaction.execute(
          sql`SELECT id FROM ${authUsers} WHERE ${authUsers.id} = ${userId} FOR UPDATE`,
        );
        const [existing] = await transaction
          .select({ workspaceId: workspaces.id, name: workspaces.name })
          .from(workspaceMembers)
          .innerJoin(workspaces, eq(workspaces.id, workspaceMembers.workspaceId))
          .where(and(eq(workspaceMembers.userId, userId), eq(workspaceMembers.role, "owner")))
          .limit(1);
        if (existing !== undefined) return existing;

        const workspace: OwnerWorkspace = {
          workspaceId: randomUUID(),
          name: "Personal Computer Agent",
        };
        const now = new Date();
        await transaction.insert(workspaces).values({
          id: workspace.workspaceId,
          name: workspace.name,
          slug: `pca-${workspace.workspaceId}`,
          createdAt: now,
          updatedAt: now,
        });
        await transaction.insert(workspaceMembers).values({
          workspaceId: workspace.workspaceId,
          userId,
          role: "owner",
          createdAt: now,
        });
        return workspace;
      });
    } catch (error) {
      throw repositoryError(error);
    }
  }

  async listOwnerWorkspaces(userId: string): Promise<OwnerWorkspace[]> {
    try {
      return await this.database
        .select({ workspaceId: workspaces.id, name: workspaces.name })
        .from(workspaceMembers)
        .innerJoin(workspaces, eq(workspaces.id, workspaceMembers.workspaceId))
        .where(and(eq(workspaceMembers.userId, userId), eq(workspaceMembers.role, "owner")));
    } catch (error) {
      throw repositoryError(error);
    }
  }

  async listOwnerDevices(workspaceId: string, userId: string): Promise<OwnerDeviceSummary[]> {
    try {
      await requireDatabaseOwnerMembership(this.database, workspaceId, userId);
      const rows = await this.database
        .select({ deviceId: devices.id })
        .from(devices)
        .where(eq(devices.workspaceId, workspaceId));
      return Promise.all(
        rows.map(async ({ deviceId }) => {
          const detail = await this.loadOwnerDevice(deviceId, workspaceId, userId);
          const { snapshot: _snapshot, ...summary } = detail;
          return summary;
        }),
      );
    } catch (error) {
      throw repositoryError(error);
    }
  }

  async loadOwnerDevice(
    deviceId: string,
    workspaceId: string,
    userId: string,
  ): Promise<OwnerDeviceDetail> {
    try {
      await requireDatabaseOwnerMembership(this.database, workspaceId, userId);
      const device = await requireDatabaseDevice(this.database, deviceId, workspaceId, true);
      const [config] = await this.database
        .select()
        .from(collectorConfigs)
        .where(
          and(
            eq(collectorConfigs.workspaceId, workspaceId),
            eq(collectorConfigs.deviceId, deviceId),
          ),
        )
        .limit(1);
      const [heartbeat] = await this.database
        .select({
          presence: deviceHeartbeats.presence,
          agentVersion: deviceHeartbeats.agentVersion,
          outboxDepth: deviceHeartbeats.outboxDepth,
          observedAt: deviceHeartbeats.receivedAt,
        })
        .from(deviceHeartbeats)
        .where(
          and(
            eq(deviceHeartbeats.workspaceId, workspaceId),
            eq(deviceHeartbeats.deviceId, deviceId),
          ),
        )
        .orderBy(desc(deviceHeartbeats.receivedAt))
        .limit(1);
      const configRecord: ConfigRecord = {
        configurationRevision: config?.configurationRevision ?? 0,
        networkEnabled: config?.networkEnabled ?? false,
        wechatEnabled: config?.wechatEnabled ?? false,
      };
      return {
        deviceId: device.id,
        workspaceId: device.workspaceId,
        platform: device.platform,
        pairedAt: device.createdAt,
        revoked: device.revokedAt !== null,
        configurationRevision: configRecord.configurationRevision,
        status:
          heartbeat === undefined
            ? null
            : {
                presence: heartbeat.presence as HeartbeatInput["presence"],
                agentVersion: heartbeat.agentVersion,
                outboxDepth: heartbeat.outboxDepth,
                observedAt: heartbeat.observedAt,
              },
        snapshot: snapshot(device, configRecord),
      };
    } catch (error) {
      throw repositoryError(error);
    }
  }

  async listCollectorConfigAudit(
    deviceId: string,
    workspaceId: string,
    userId: string,
  ): Promise<CollectorConfigAuditRecord[]> {
    try {
      await requireDatabaseOwnerMembership(this.database, workspaceId, userId);
      await requireDatabaseDevice(this.database, deviceId, workspaceId, true);
      return await this.database
        .select({
          actorUserId: collectorConfigAudit.actorUserId,
          configurationRevision: collectorConfigAudit.configurationRevision,
          oldConfig: collectorConfigAudit.oldConfig,
          newConfig: collectorConfigAudit.newConfig,
          createdAt: collectorConfigAudit.createdAt,
        })
        .from(collectorConfigAudit)
        .where(
          and(
            eq(collectorConfigAudit.workspaceId, workspaceId),
            eq(collectorConfigAudit.deviceId, deviceId),
          ),
        )
        .orderBy(desc(collectorConfigAudit.createdAt));
    } catch (error) {
      throw repositoryError(error);
    }
  }

  async #authenticateDatabaseCredential(
    credentialHash: string,
    now: Date,
    kind: "access" | "refresh",
  ): Promise<DeviceCredentialAuthentication> {
    const hashColumn =
      kind === "access"
        ? deviceCredentialGenerations.accessTokenHash
        : deviceCredentialGenerations.refreshTokenHash;
    const expiryColumn =
      kind === "access"
        ? deviceCredentialGenerations.accessExpiresAt
        : deviceCredentialGenerations.refreshExpiresAt;
    try {
      const [credential] = await this.database
        .select({
          workspaceId: deviceCredentialGenerations.workspaceId,
          deviceId: deviceCredentialGenerations.deviceId,
          credentialRevokedAt: deviceCredentialGenerations.revokedAt,
          deviceRevokedAt: devices.revokedAt,
        })
        .from(deviceCredentialGenerations)
        .innerJoin(devices, eq(devices.id, deviceCredentialGenerations.deviceId))
        .where(and(eq(hashColumn, credentialHash), gt(expiryColumn, now)))
        .limit(1);
      if (credential === undefined) {
        throw new ControlRepositoryError("CREDENTIAL_INVALID");
      }
      if (credential.deviceRevokedAt !== null) {
        throw new ControlRepositoryError("DEVICE_REVOKED");
      }
      if (credential.credentialRevokedAt !== null) {
        throw new ControlRepositoryError("CREDENTIAL_INVALID");
      }
      return { workspaceId: credential.workspaceId, deviceId: credential.deviceId };
    } catch (error) {
      throw repositoryError(error);
    }
  }
}

type DatabaseExecutor = Pick<NodePgDatabase<typeof cloudSchema>, "select">;

async function requireDatabaseDevice(
  database: DatabaseExecutor,
  deviceId: string,
  workspaceId: string,
  allowRevoked: boolean,
): Promise<DeviceRecord> {
  const [device] = await database
    .select({
      id: devices.id,
      workspaceId: devices.workspaceId,
      ownerUserId: devices.ownerUserId,
      devicePublicKeyHash: devices.devicePublicKeyHash,
      platform: devices.platform,
      createdAt: devices.createdAt,
      revokedAt: devices.revokedAt,
    })
    .from(devices)
    .where(eq(devices.id, deviceId))
    .limit(1);
  if (device === undefined) {
    throw new ControlRepositoryError("DEVICE_NOT_FOUND");
  }
  if (device.workspaceId !== workspaceId) {
    throw new ControlRepositoryError("WORKSPACE_FORBIDDEN");
  }
  if (!allowRevoked && device.revokedAt !== null) {
    throw new ControlRepositoryError("DEVICE_REVOKED");
  }
  return { ...device, platform: device.platform as "macos" };
}

async function requireDatabaseOwnerMembership(
  database: DatabaseExecutor,
  workspaceId: string,
  userId: string,
): Promise<void> {
  const [membership] = await database
    .select({ userId: workspaceMembers.userId })
    .from(workspaceMembers)
    .where(
      and(
        eq(workspaceMembers.workspaceId, workspaceId),
        eq(workspaceMembers.userId, userId),
        eq(workspaceMembers.role, "owner"),
      ),
    )
    .limit(1);
  if (membership === undefined) {
    throw new ControlRepositoryError("WORKSPACE_FORBIDDEN");
  }
}

function configKey(workspaceId: string, deviceId: string): string {
  return `${workspaceId}:${deviceId}`;
}

function membershipKey(workspaceId: string, userId: string): string {
  return `${workspaceId}:${userId}`;
}

function secureEqual(left: string, right: string): boolean {
  const leftBuffer = Buffer.from(left);
  const rightBuffer = Buffer.from(right);
  return leftBuffer.length === rightBuffer.length && timingSafeEqual(leftBuffer, rightBuffer);
}

function snapshot(device: DeviceRecord, config: ConfigRecord): AgentControlSnapshot {
  return {
    device_id: device.id,
    workspace_id: device.workspaceId,
    revoked: device.revokedAt !== null,
    configuration_revision: config.configurationRevision,
    collectors: {
      network: { enabled: config.networkEnabled },
      "communication.wechat": {
        enabled: config.wechatEnabled,
        direction: "outgoing",
        message_type: "text",
        sync_mode: "full",
      },
    },
  };
}

function repositoryError(error: unknown): ControlRepositoryError {
  if (error instanceof ControlRepositoryError) {
    return error;
  }
  if (typeof error === "object" && error !== null && "code" in error && error.code === "23505") {
    return new ControlRepositoryError("CONFLICT");
  }
  if (
    typeof error === "object" &&
    error !== null &&
    "constraint" in error &&
    typeof error.constraint === "string" &&
    error.constraint.endsWith("membership_fk")
  ) {
    return new ControlRepositoryError("WORKSPACE_FORBIDDEN");
  }
  throw error;
}
