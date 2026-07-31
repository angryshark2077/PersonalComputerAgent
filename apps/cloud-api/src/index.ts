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
}

export function createProductionApp(environment: ProductionEnvironment = process.env): Hono {
  const connectionString = requiredEnvironment(environment, "DATABASE_URL");
  const secret = requiredEnvironment(environment, "BETTER_AUTH_SECRET");
  const baseURL = requiredEnvironment(environment, "BETTER_AUTH_URL");
  const pool = new Pool({ connectionString });
  const database = drizzle(pool, { schema: cloudSchema });
  const repository = new DrizzleControlRepository(database);
  const auth = betterAuth({
    database: drizzleAdapter(database, {
      provider: "pg",
      schema: { user: authUsers, session: authSessions, account: authAccounts },
      transaction: true,
    }),
    baseURL,
    secret,
    emailAndPassword: { enabled: true },
    user: { fields: { image: "imageUrl" } },
    session: { fields: { token: "sessionToken" } },
    account: { fields: { password: "passwordHash" } },
  });
  const app = createApp({
    repository,
    ownerAuthenticator: createBetterAuthOwnerAuthenticator(auth, repository),
  });
  app.all("/api/auth/*", (context) => auth.handler(context.req.raw));
  return app;
}

function parseCallbackState(value: unknown): string | null {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return null;
  const body = value as Record<string, unknown>;
  return Object.keys(body).length === 1 && typeof body.callback_state === "string" && /^[A-Za-z0-9_-]{43,}$/.test(body.callback_state)
    ? body.callback_state
    : null;
}

function requiredEnvironment(environment: ProductionEnvironment, key: keyof ProductionEnvironment): string {
  const value = environment[key];
  if (value === undefined || value.length === 0) {
    throw new Error(`missing required configuration: ${key}`);
  }
  return value;
}

export default createProductionApp;
