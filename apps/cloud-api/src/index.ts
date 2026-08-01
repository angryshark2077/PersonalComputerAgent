import { createHmac, randomUUID, timingSafeEqual } from "node:crypto";
import { isIP } from "node:net";

import { Hono } from "hono";
import { betterAuth } from "better-auth/minimal";
import { drizzleAdapter } from "better-auth/adapters/drizzle";
import { drizzle } from "drizzle-orm/node-postgres";
import { Pool } from "pg";

import {
  DrizzleControlRepository,
  type ControlRepository,
} from "@pca/db-cloud/src/repository.js";
import {
  authAccounts,
  authSessions,
  authUsers,
  cloudSchema,
} from "@pca/db-cloud/src/schema.js";

import {
  createBetterAuthOwnerAuthenticator,
  requireDevice,
  repositoryErrorResponse,
  requireOwner,
  type OwnerAuthenticator,
} from "./auth.js";
import { parseCollectorConfig, parseHeartbeat } from "./control.js";
import {
  accessCredentialLifetimeMs,
  errorResponse,
  hashSecret,
  opaqueCredential,
  opaqueSessionId,
  PairingRateLimiter,
  pairingSessionLifetimeMs,
  parsePairingExchange,
  parsePairingStart,
  pkceChallenge,
  refreshCredentialLifetimeMs,
} from "./pairing.js";

export type { OwnerPrincipal } from "./auth.js";

export interface CreateAppOptions {
  repository: ControlRepository;
  ownerAuthenticator?: OwnerAuthenticator;
  pairingRateLimiter?: PairingRateLimiter;
  clientAddress?: (request: Request) => string | undefined;
}

export function createApp(options: CreateAppOptions): Hono {
  const pairingRateLimiter = options.pairingRateLimiter ?? new PairingRateLimiter();
  const app = new Hono();

  app.get("/health", (context) => context.json({ ready: true, service: "pca-cloud-api" }));

  app.post("/v1/device-pairing/sessions", async (context) => {
    const input = parsePairingStart(await context.req.json().catch(() => null));
    if (input === null) {
      return errorResponse(context, 400, "REQUEST_INVALID", "Invalid pairing session request.");
    }
    const retryAfter = pairingRateLimiter.check(
      options.clientAddress?.(context.req.raw) ?? "unattributed",
      hashSecret(input.device_public_key),
    );
    if (retryAfter !== null) {
      context.header("retry-after", String(retryAfter));
      return errorResponse(context, 429, "PAIRING_RATE_LIMITED", "Too many pairing requests.");
    }
    const now = new Date();
    const sessionId = opaqueSessionId();
    await options.repository.createPairingSession({
      sessionIdHash: hashSecret(sessionId),
      devicePublicKeyHash: hashSecret(input.device_public_key),
      codeChallenge: input.code_challenge,
      callbackUri: input.callback_uri,
      callbackStateHash: hashSecret(input.callback_state),
      expiresAt: new Date(now.getTime() + pairingSessionLifetimeMs),
      createdAt: now,
    });
    return context.json({ session_id: sessionId }, 201);
  });

  app.post("/v1/device-pairing/sessions/:sessionId/authorize", async (context) => {
    const principal = await requireOwner(context, options.ownerAuthenticator);
    if (principal instanceof Response) return principal;
    const callbackState = parseCallbackState(await context.req.json().catch(() => null));
    if (callbackState === null) {
      return errorResponse(context, 400, "REQUEST_INVALID", "Invalid callback state.");
    }
    const sessionId = context.req.param("sessionId");
    const authorizationCode = opaqueCredential();
    const now = new Date();
    try {
      const callbackUri = await options.repository.authorizePairingSession({
        sessionIdHash: hashSecret(sessionId),
        authorizationCodeHash: hashSecret(authorizationCode),
        callbackStateHash: hashSecret(callbackState),
        workspaceId: principal.workspaceId,
        ownerUserId: principal.userId,
        expiresAt: new Date(now.getTime() + pairingSessionLifetimeMs),
        now,
      });
      const redirectUri = new URL(callbackUri);
      redirectUri.search = new URLSearchParams({ code: authorizationCode, state: callbackState }).toString();
      return context.redirect(redirectUri.toString(), 302);
    } catch (error) {
      return repositoryErrorResponse(context, error);
    }
  });

  app.post("/v1/device-pairing/exchange", async (context) => {
    const input = parsePairingExchange(await context.req.json().catch(() => null));
    if (input === null) {
      return errorResponse(context, 400, "REQUEST_INVALID", "Invalid pairing exchange request.");
    }
    const now = new Date();
    const accessToken = opaqueCredential();
    const refreshToken = opaqueCredential();
    const accessExpiresAt = new Date(now.getTime() + accessCredentialLifetimeMs);
    const refreshExpiresAt = new Date(now.getTime() + refreshCredentialLifetimeMs);
    try {
      const grant = await options.repository.consumeAuthorizationCode({
        sessionIdHash: hashSecret(input.session_id),
        authorizationCodeHash: hashSecret(input.authorization_code),
        codeChallenge: pkceChallenge(input.code_verifier),
        deviceId: opaqueSessionId(),
        accessTokenHash: hashSecret(accessToken),
        refreshTokenHash: hashSecret(refreshToken),
        accessExpiresAt,
        refreshExpiresAt,
        now,
      });
      return context.json({
        workspace_id: grant.workspaceId,
        device_id: grant.deviceId,
        device_access_token: accessToken,
        refresh_token: refreshToken,
        access_expires_at: grant.accessExpiresAt.toISOString(),
        refresh_expires_at: grant.refreshExpiresAt.toISOString(),
      });
    } catch (error) {
      return repositoryErrorResponse(context, error);
    }
  });

  app.post("/v1/devices/token/refresh", async (context) => {
    const device = await requireDevice(context, options.repository, "refresh");
    if (device instanceof Response) return device;
    const refreshToken = (context.req.header("authorization") ?? "").slice("Bearer ".length);
    const now = new Date();
    const accessToken = opaqueCredential();
    const nextRefreshToken = opaqueCredential();
    const accessExpiresAt = new Date(now.getTime() + accessCredentialLifetimeMs);
    const refreshExpiresAt = new Date(now.getTime() + refreshCredentialLifetimeMs);
    try {
      const grant = await options.repository.rotateDeviceCredentials({
        workspaceId: device.workspaceId,
        deviceId: device.deviceId,
        currentRefreshTokenHash: hashSecret(refreshToken),
        newAccessTokenHash: hashSecret(accessToken),
        newRefreshTokenHash: hashSecret(nextRefreshToken),
        accessExpiresAt,
        refreshExpiresAt,
        now,
      });
      return context.json({
        workspace_id: grant.workspaceId,
        device_id: grant.deviceId,
        device_access_token: accessToken,
        refresh_token: nextRefreshToken,
        access_expires_at: grant.accessExpiresAt.toISOString(),
        refresh_expires_at: grant.refreshExpiresAt.toISOString(),
      });
    } catch (error) {
      return repositoryErrorResponse(context, error);
    }
  });

  app.post("/v1/agent/control", async (context) => {
    const device = await requireDevice(context, options.repository, "access");
    if (device instanceof Response) return device;
    const heartbeat = parseHeartbeat(await context.req.json().catch(() => null));
    if (heartbeat === null) {
      return errorResponse(context, 400, "REQUEST_INVALID", "Invalid control request.");
    }
    const now = new Date();
    try {
      await options.repository.recordHeartbeat({
        ...heartbeat,
        workspaceId: device.workspaceId,
        deviceId: device.deviceId,
        receivedAt: now,
      });
      const snapshot = await options.repository.loadControlSnapshot(device.deviceId, device.workspaceId);
      return context.json({ snapshot, server_time: now.toISOString() });
    } catch (error) {
      return repositoryErrorResponse(context, error);
    }
  });

  app.get("/v1/workspaces", async (context) => {
    const principal = await requireOwner(context, options.ownerAuthenticator);
    if (principal instanceof Response) return principal;
    try {
      const workspaces = await options.repository.listOwnerWorkspaces(principal.userId);
      return context.json({
        workspaces: workspaces.map((workspace) => ({
          workspace_id: workspace.workspaceId,
          name: workspace.name,
        })),
      });
    } catch (error) {
      return repositoryErrorResponse(context, error);
    }
  });

  app.get("/v1/devices", async (context) => {
    const principal = await requireOwner(context, options.ownerAuthenticator);
    if (principal instanceof Response) return principal;
    try {
      const devices = await options.repository.listOwnerDevices(
        principal.workspaceId,
        principal.userId,
      );
      return context.json({ devices: devices.map(ownerDeviceSummaryResponse) });
    } catch (error) {
      return repositoryErrorResponse(context, error);
    }
  });

  app.get("/v1/devices/:deviceId", async (context) => {
    const principal = await requireOwner(context, options.ownerAuthenticator);
    if (principal instanceof Response) return principal;
    try {
      const device = await options.repository.loadOwnerDevice(
        context.req.param("deviceId"),
        principal.workspaceId,
        principal.userId,
      );
      return context.json({
        ...ownerDeviceSummaryResponse(device),
        collectors: device.snapshot.collectors,
      });
    } catch (error) {
      return repositoryErrorResponse(context, error);
    }
  });

  app.get("/v1/devices/:deviceId/collector-config/audit", async (context) => {
    const principal = await requireOwner(context, options.ownerAuthenticator);
    if (principal instanceof Response) return principal;
    try {
      const audit = await options.repository.listCollectorConfigAudit(
        context.req.param("deviceId"),
        principal.workspaceId,
        principal.userId,
      );
      return context.json({
        audit: audit.map((record) => ({
          actor_user_id: record.actorUserId,
          configuration_revision: record.configurationRevision,
          old_config: collectorConfigResponse(record.oldConfig),
          new_config: collectorConfigResponse(record.newConfig),
          created_at: record.createdAt.toISOString(),
        })),
      });
    } catch (error) {
      return repositoryErrorResponse(context, error);
    }
  });

  app.put("/v1/devices/:deviceId/collector-config", async (context) => {
    const principal = await requireOwner(context, options.ownerAuthenticator);
    if (principal instanceof Response) return principal;
    const config = parseCollectorConfig(await context.req.json().catch(() => null));
    if (config === null) {
      return errorResponse(context, 400, "REQUEST_INVALID", "Invalid collector configuration.");
    }
    try {
      const revision = await options.repository.appendConfigAudit({
        auditId: opaqueSessionId(),
        actorUserId: principal.userId,
        workspaceId: principal.workspaceId,
        deviceId: context.req.param("deviceId"),
        config,
        now: new Date(),
      });
      return context.json({ configuration_revision: revision });
    } catch (error) {
      return repositoryErrorResponse(context, error);
    }
  });

  app.post("/v1/devices/:deviceId/revoke", async (context) => {
    const principal = await requireOwner(context, options.ownerAuthenticator);
    if (principal instanceof Response) return principal;
    try {
      await options.repository.revokeDevice({
        auditId: opaqueSessionId(),
        actorUserId: principal.userId,
        workspaceId: principal.workspaceId,
        deviceId: context.req.param("deviceId"),
        now: new Date(),
      });
      return context.body(null, 204);
    } catch (error) {
      return repositoryErrorResponse(context, error);
    }
  });

  return app;
}

export interface ProductionEnvironment {
  DATABASE_URL?: string;
  BETTER_AUTH_SECRET?: string;
  BETTER_AUTH_URL?: string;
  TRUSTED_PROXY_CLIENT_IP_HMAC_SECRET?: string;
}

export function createProductionApp(environment: ProductionEnvironment = process.env): Hono {
  const connectionString = requiredEnvironment(environment, "DATABASE_URL");
  const secret = requiredEnvironment(environment, "BETTER_AUTH_SECRET");
  const baseURL = requiredEnvironment(environment, "BETTER_AUTH_URL");
  const pool = new Pool({ connectionString });
  const database = drizzle(pool, { schema: cloudSchema });
  const repository = new DrizzleControlRepository(database);
  const auth = betterAuth({
    database: createHashedSessionAdapter(database),
    baseURL,
    secret,
    emailAndPassword: { enabled: true },
    advanced: { database: { generateId: () => randomUUID() } },
    user: { fields: { image: "imageUrl" } },
    session: { fields: { token: "sessionTokenHash" } },
    account: { fields: { password: "passwordHash" } },
    databaseHooks: createOwnerWorkspaceBootstrapHooks(repository),
  });
  const app = createApp({
    repository,
    ownerAuthenticator: createBetterAuthOwnerAuthenticator(auth, repository),
    clientAddress: createTrustedProxyClientAddress(environment),
  });
  app.all("/api/auth/*", (context) => auth.handler(context.req.raw));
  return app;
}

/** The Better Auth hook runs after a new user exists and creates its sole Owner Workspace. */
export function createOwnerWorkspaceBootstrapHooks(
  repository: Pick<ControlRepository, "bootstrapOwnerWorkspace">,
) {
  return {
    user: {
      create: {
        after: async (user: { id: string }) => {
          await repository.bootstrapOwnerWorkspace(user.id);
        },
      },
    },
  };
}

/**
 * Better Auth owns the raw bearer token in its signed browser cookie. Its
 * Drizzle adapter must still address our `session_token_hash` column, so this
 * wrapper hashes every session-token predicate before it reaches PostgreSQL.
 * Results are restored only when the raw token was supplied to this operation.
 */
export function createHashedSessionAdapter(database: Parameters<typeof drizzleAdapter>[0]) {
  const adapterFactory = drizzleAdapter(database, {
    provider: "pg",
    schema: { user: authUsers, session: authSessions, account: authAccounts },
    transaction: true,
  });
  return (options: Parameters<typeof adapterFactory>[0]) =>
    wrapHashedSessionAdapter(adapterFactory(options));
}

type SessionAdapterInput = Record<string, unknown> & {
  model?: string;
  where?: SessionWhere[];
  data?: Record<string, unknown>;
  update?: Record<string, unknown>;
  set?: Record<string, unknown>;
};
type SessionWhere = Record<string, unknown> & { field?: string; value?: unknown };

function wrapHashedSessionAdapter<T extends object>(adapter: T): T {
  return new Proxy(adapter, {
    get(target, property, receiver) {
      const operation = Reflect.get(target, property, receiver);
      if (property === "transaction" && typeof operation === "function") {
        return ((callback: (transaction: object) => Promise<unknown>) =>
          (operation as (callback_: (transaction: object) => Promise<unknown>) => unknown).call(
            target,
            (transaction) => callback(wrapHashedSessionAdapter(transaction)),
          )) as typeof operation;
      }
      if (typeof operation !== "function" || !hashedSessionOperations.has(property)) {
        return operation;
      }
      return (async (input: SessionAdapterInput) => {
        const transformed = hashSessionOperation(input);
        const result = await operation.call(target, transformed.input);
        return restoreRawSessionToken(result, transformed.rawTokenByHash);
      }) as typeof operation;
    },
  });
}

const hashedSessionOperations = new Set<PropertyKey>([
  "create",
  "findOne",
  "findMany",
  "count",
  "update",
  "updateMany",
  "delete",
  "deleteMany",
  "consumeOne",
  "incrementOne",
]);

function hashSessionOperation(input: SessionAdapterInput) {
  if (input.model !== "session") return { input, rawTokenByHash: new Map<string, string>() };
  const rawTokenByHash = new Map<string, string>();
  const hashToken = (value: unknown) => {
    if (typeof value !== "string") return value;
    const hash = hashSecret(value);
    rawTokenByHash.set(hash, value);
    return hash;
  };
  const hashValues = (values: Record<string, unknown> | undefined) => {
    if (values === undefined) return values;
    return {
      ...values,
      ...(values.token === undefined ? {} : { token: hashToken(values.token) }),
      ...(values.ipAddress === "" ? { ipAddress: null } : {}),
    };
  };
  return {
    input: {
      ...input,
      where: input.where?.map((where) =>
        where.field === "token"
          ? {
              ...where,
              value: Array.isArray(where.value) ? where.value.map(hashToken) : hashToken(where.value),
            }
          : where,
      ),
      data: hashValues(input.data),
      update: hashValues(input.update),
      set: hashValues(input.set),
    },
    rawTokenByHash,
  };
}

function restoreRawSessionToken(result: unknown, rawTokenByHash: ReadonlyMap<string, string>): unknown {
  if (Array.isArray(result)) return result.map((row) => restoreRawSessionToken(row, rawTokenByHash));
  if (typeof result !== "object" || result === null) return result;
  const row = result as Record<string, unknown>;
  const stored = typeof row.token === "string" ? row.token : row.sessionTokenHash;
  const raw = typeof stored === "string" ? rawTokenByHash.get(stored) : undefined;
  return raw === undefined ? result : { ...row, token: raw };
}

/**
 * A public origin ignores all forwarding headers. A configured trusted proxy may
 * instead send the literal client IP in `x-pca-client-ip` and an HMAC-SHA256
 * base64url signature in `x-pca-client-ip-signature`. The proxy secret must not
 * be available to clients. Invalid, unsigned, or non-IP values are unattributed.
 */
export function createTrustedProxyClientAddress(
  environment: Pick<ProductionEnvironment, "TRUSTED_PROXY_CLIENT_IP_HMAC_SECRET">,
): (request: Request) => string | undefined {
  const secret = environment.TRUSTED_PROXY_CLIENT_IP_HMAC_SECRET;
  return (request) => {
    if (secret === undefined || secret.length === 0) return undefined;
    const clientIp = request.headers.get("x-pca-client-ip");
    const signature = request.headers.get("x-pca-client-ip-signature");
    if (clientIp === null || signature === null || isIP(clientIp) === 0) return undefined;
    const expected = createHmac("sha256", secret).update(clientIp).digest();
    const received = Buffer.from(signature, "base64url");
    return received.length === expected.length && timingSafeEqual(received, expected)
      ? clientIp
      : undefined;
  };
}

function parseCallbackState(value: unknown): string | null {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return null;
  const body = value as Record<string, unknown>;
  return Object.keys(body).length === 1 && typeof body.callback_state === "string" && /^[A-Za-z0-9_-]{43,}$/.test(body.callback_state)
    ? body.callback_state
    : null;
}

function ownerDeviceSummaryResponse(device: {
  deviceId: string;
  workspaceId: string;
  platform: string;
  pairedAt: Date;
  revoked: boolean;
  configurationRevision: number;
  status: {
    presence: string;
    agentVersion: string;
    outboxDepth: number;
    observedAt: Date;
  } | null;
}) {
  return {
    device_id: device.deviceId,
    workspace_id: device.workspaceId,
    platform: device.platform,
    paired_at: device.pairedAt.toISOString(),
    revoked: device.revoked,
    configuration_revision: device.configurationRevision,
    status:
      device.status === null
        ? null
        : {
            presence: device.status.presence,
            agent_version: device.status.agentVersion,
            outbox_depth: device.status.outboxDepth,
            observed_at: device.status.observedAt.toISOString(),
          },
  };
}

function collectorConfigResponse(config: { networkEnabled: boolean; wechatEnabled: boolean }) {
  return {
    network: { enabled: config.networkEnabled },
    "communication.wechat": {
      enabled: config.wechatEnabled,
      direction: "outgoing" as const,
      message_type: "text" as const,
      sync_mode: "full" as const,
    },
  };
}

function requiredEnvironment(environment: ProductionEnvironment, key: keyof ProductionEnvironment): string {
  const value = environment[key];
  if (value === undefined || value.length === 0) {
    throw new Error(`missing required configuration: ${key}`);
  }
  return value;
}

export default createProductionApp;
