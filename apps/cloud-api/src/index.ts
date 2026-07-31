import { Hono } from "hono";

import { MemoryControlRepository, type ControlRepository } from "@pca/db-cloud/src/repository.js";

import { requireDevice, repositoryErrorResponse, requireOwner, type OwnerAuthenticator } from "./auth.js";
import { parseCollectorConfig, parseHeartbeat } from "./control.js";
import {
  accessCredentialLifetimeMs,
  errorResponse,
  hashSecret,
  opaqueCredential,
  opaqueSessionId,
  pairingSessionLifetimeMs,
  parsePairingExchange,
  parsePairingStart,
  refreshCredentialLifetimeMs,
} from "./pairing.js";

export type { OwnerPrincipal } from "./auth.js";

export interface CreateAppOptions {
  repository?: ControlRepository;
  ownerAuthenticator?: OwnerAuthenticator;
}

export function createApp(options: CreateAppOptions = {}): Hono {
  const repository = options.repository ?? new MemoryControlRepository();
  const app = new Hono();

  app.get("/health", (context) =>
    context.json({ ready: true, service: "pca-cloud-api" }),
  );

  app.post("/v1/device-pairing/sessions", async (context) => {
    const input = parsePairingStart(await context.req.json().catch(() => null));
    if (input === null) {
      return errorResponse(context, 400, "REQUEST_INVALID", "Invalid pairing session request.");
    }
    const now = new Date();
    const sessionId = opaqueSessionId();
    await repository.createPairingSession({
      sessionIdHash: hashSecret(sessionId),
      devicePublicKeyHash: hashSecret(input.device_public_key),
      codeChallenge: input.code_challenge,
      callbackUri: input.callback_uri,
      expiresAt: new Date(now.getTime() + pairingSessionLifetimeMs),
      createdAt: now,
    });
    return context.json({ session_id: sessionId }, 201);
  });

  app.post("/v1/device-pairing/sessions/:sessionId/authorize", async (context) => {
    const principal = await requireOwner(context, options.ownerAuthenticator);
    if (principal instanceof Response) {
      return principal;
    }
    const sessionId = context.req.param("sessionId");
    const authorizationCode = opaqueCredential();
    const callbackState = opaqueCredential();
    const now = new Date();
    try {
      const callbackUri = await repository.authorizePairingSession({
        sessionIdHash: hashSecret(sessionId),
        authorizationCodeHash: hashSecret(authorizationCode),
        workspaceId: principal.workspaceId,
        ownerUserId: principal.userId,
        callbackStateHash: hashSecret(callbackState),
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
      const grant = await repository.consumeAuthorizationCode({
        sessionIdHash: hashSecret(input.session_id),
        authorizationCodeHash: hashSecret(input.authorization_code),
        codeChallenge: input.code_verifier,
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
    const device = await requireDevice(context, repository, "refresh");
    if (device instanceof Response) {
      return device;
    }
    const authorization = context.req.header("authorization") ?? "";
    const refreshToken = authorization.slice("Bearer ".length);
    const now = new Date();
    const accessToken = opaqueCredential();
    const nextRefreshToken = opaqueCredential();
    const accessExpiresAt = new Date(now.getTime() + accessCredentialLifetimeMs);
    const refreshExpiresAt = new Date(now.getTime() + refreshCredentialLifetimeMs);
    try {
      const grant = await repository.rotateDeviceCredentials({
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
    const device = await requireDevice(context, repository, "access");
    if (device instanceof Response) {
      return device;
    }
    const heartbeat = parseHeartbeat(await context.req.json().catch(() => null));
    if (heartbeat === null) {
      return errorResponse(context, 400, "REQUEST_INVALID", "Invalid control request.");
    }
    const now = new Date();
    try {
      await repository.recordHeartbeat({
        ...heartbeat,
        workspaceId: device.workspaceId,
        deviceId: device.deviceId,
        receivedAt: now,
      });
      const snapshot = await repository.loadControlSnapshot(device.deviceId, device.workspaceId);
      return context.json({ snapshot, server_time: now.toISOString() });
    } catch (error) {
      return repositoryErrorResponse(context, error);
    }
  });

  app.put("/v1/devices/:deviceId/collector-config", async (context) => {
    const principal = await requireOwner(context, options.ownerAuthenticator);
    if (principal instanceof Response) {
      return principal;
    }
    const config = parseCollectorConfig(await context.req.json().catch(() => null));
    if (config === null) {
      return errorResponse(context, 400, "REQUEST_INVALID", "Invalid collector configuration.");
    }
    try {
      const revision = await repository.appendConfigAudit({
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
    if (principal instanceof Response) {
      return principal;
    }
    try {
      await repository.revokeDevice({
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

const app = createApp();

export default app;
