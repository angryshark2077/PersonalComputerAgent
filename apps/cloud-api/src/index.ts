import { createHmac, randomUUID, timingSafeEqual } from "node:crypto";
import { isIP } from "node:net";

import { Hono } from "hono";
import { betterAuth } from "better-auth/minimal";
import { drizzleAdapter } from "better-auth/adapters/drizzle";
import { drizzle } from "drizzle-orm/node-postgres";
import { Pool } from "pg";

import {
  DrizzleControlRepository,
  type CommunicationObjectRecord,
  type CommunicationSource,
  type ControlRepository,
  type PhotoLibraryAssetRecord,
  type ScreenshotRecord,
} from "@pca/db-cloud/src/repository.js";
import {
  authAccounts,
  authSessions,
  authUsers,
  cloudSchema,
  type StoredCollectorConfig,
} from "@pca/db-cloud/src/schema.js";

import {
  createBetterAuthOwnerAuthenticator,
  requireDevice,
  repositoryErrorResponse,
  requireOwner,
  type OwnerAuthenticator,
} from "./auth.js";
import { parseCollectorConfig, parseHeartbeat } from "./control.js";
import { CountryIsGeoEnricher, type GeoEnrichmentPort } from "./geo.js";
import { createR2ObjectStore, type R2ObjectHead, type R2ObjectStore } from "./r2.js";
import { parseCommunicationSyncBatch, parseSyncBatch } from "./sync.js";
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

const productionDashboardOrigin = "https://pca-dashboard-production.up.railway.app";

export interface CreateAppOptions {
  repository: ControlRepository;
  ownerAuthenticator?: OwnerAuthenticator;
  pairingRateLimiter?: PairingRateLimiter;
  clientAddress?: (request: Request) => string | undefined;
  geoEnricher?: GeoEnrichmentPort;
  objectStore?: R2ObjectStore;
}

export function createApp(options: CreateAppOptions): Hono {
  const pairingRateLimiter = options.pairingRateLimiter ?? new PairingRateLimiter();
  const app = new Hono();

  app.get("/healthz", (context) => context.json({ status: "ok" }));
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
    return context.json({
      session_id: sessionId,
      authorization_url: pairingAuthorizationURL(sessionId, input.callback_state),
    }, 201);
  });

  app.post("/v1/device-pairing/sessions/:sessionId/authorize", async (context) => {
    const principal = await requireOwner(context, options.ownerAuthenticator);
    if (principal instanceof Response) return principal;
    const callbackState = await parseCallbackStateRequest(context.req.raw);
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
      const observedExitIp = heartbeat.network === null
        ? null
        : options.clientAddress?.(context.req.raw) ?? null;
      const ipLocation = observedExitIp === null || options.geoEnricher === undefined
        ? null
        : await options.geoEnricher.locate(observedExitIp).catch(() => null);
      await options.repository.recordHeartbeat({
        ...heartbeat,
        network: heartbeat.network === null ? null : {
          ...heartbeat.network,
          observedExitIp,
          ipLocation,
        },
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

  app.post("/v1/agent/sync/events", async (context) => {
    const device = await requireDevice(context, options.repository, "access");
    if (device instanceof Response) return device;
    const batch = parseSyncBatch(await context.req.json().catch(() => null));
    if (batch === null || batch.deviceId !== device.deviceId) {
      return errorResponse(context, 400, "REQUEST_INVALID", "Invalid system event batch.");
    }
    if (batch.events.some((event) => event.workspaceId !== device.workspaceId || event.deviceId !== device.deviceId)) {
      return errorResponse(context, 403, "WORKSPACE_FORBIDDEN", "The requested Workspace is forbidden.");
    }
    try {
      const result = await options.repository.appendSystemEvents(
        device.workspaceId,
        device.deviceId,
        batch.events,
      );
      return context.json({
        batch_id: batch.batchId,
        accepted: result.acceptedEventIds,
        duplicates: result.duplicateEventIds,
        rejected: [],
        server_time: new Date().toISOString(),
      });
    } catch (error) {
      return repositoryErrorResponse(context, error);
    }
  });

  app.post("/v1/agent/sync/communication/events", async (context) => {
    const device = await requireDevice(context, options.repository, "access");
    if (device instanceof Response) return device;
    const batch = parseCommunicationSyncBatch(await context.req.json().catch(() => null));
    if (batch === null || batch.deviceId !== device.deviceId) {
      return errorResponse(context, 400, "REQUEST_INVALID", "Invalid communication event batch.");
    }
    if (batch.events.some((event) => event.workspaceId !== device.workspaceId || event.deviceId !== device.deviceId)) {
      return errorResponse(context, 403, "WORKSPACE_FORBIDDEN", "The requested Workspace is forbidden.");
    }
    try {
      const result = await options.repository.appendCommunicationEvents(
        device.workspaceId,
        device.deviceId,
        batch.events,
      );
      return context.json({
        batch_id: batch.batchId,
        accepted: result.acceptedEventIds,
        duplicates: result.duplicateEventIds,
        rejected: [],
        server_time: new Date().toISOString(),
      });
    } catch (error) {
      return repositoryErrorResponse(context, error);
    }
  });

  app.post("/v1/agent/communication/objects/prepare", async (context) => {
    const device = await requireDevice(context, options.repository, "access");
    if (device instanceof Response) return device;
    const input = parseObjectReference(await context.req.json().catch(() => null));
    if (input === null) {
      return errorResponse(context, 400, "REQUEST_INVALID", "Invalid communication object request.");
    }
    if (options.objectStore === undefined) {
      return errorResponse(context, 503, "OBJECT_STORE_UNAVAILABLE", "Private media storage is unavailable.");
    }
    let object: CommunicationObjectRecord;
    try {
      object = await options.repository.prepareCommunicationObject(device.workspaceId, device.deviceId, {
        objectId: randomUUID(),
        objectKey: `communication/${randomUUID()}`,
        eventId: input.eventId,
        attachmentId: input.attachmentId,
        now: new Date(),
      });
    } catch (error) {
      return repositoryErrorResponse(context, error);
    }
    if (object.state === "completed") {
      return context.json({ object_id: object.objectId, state: "completed" });
    }
    try {
      const upload = await options.objectStore.signUpload(object);
      return context.json({
        object_id: object.objectId,
        state: "prepared",
        upload: {
          url: upload.url,
          headers: upload.headers,
          expires_at: new Date(Date.now() + 300_000).toISOString(),
        },
      });
    } catch (error) {
      return errorResponse(context, 503, "OBJECT_STORE_UNAVAILABLE", "Private media storage is unavailable.");
    }
  });

  app.post("/v1/agent/communication/objects/complete", async (context) => {
    const device = await requireDevice(context, options.repository, "access");
    if (device instanceof Response) return device;
    const input = parseObjectId(await context.req.json().catch(() => null));
    if (input === null) {
      return errorResponse(context, 400, "REQUEST_INVALID", "Invalid communication object completion.");
    }
    if (options.objectStore === undefined) {
      return errorResponse(context, 503, "OBJECT_STORE_UNAVAILABLE", "Private media storage is unavailable.");
    }
    let object: CommunicationObjectRecord;
    try {
      object = await options.repository.loadDeviceCommunicationObject(
        device.workspaceId,
        device.deviceId,
        input.objectId,
      );
    } catch (error) {
      return repositoryErrorResponse(context, error);
    }
    if (object.state === "completed") {
      return context.json({ object_id: object.objectId, state: "completed" });
    }
    let actual: R2ObjectHead | null;
    try {
      actual = await options.objectStore.headObject(object.objectKey);
    } catch {
      return errorResponse(context, 503, "OBJECT_STORE_UNAVAILABLE", "Private media storage is unavailable.");
    }
    if (
      actual === null
      || actual.sizeBytes !== object.expectedSizeBytes
      || actual.mimeType !== object.expectedMimeType
      || actual.sha256 !== object.expectedSha256
    ) {
      if (actual !== null) {
        try {
          await options.objectStore.deleteObject(object.objectKey);
        } catch {
          return errorResponse(context, 503, "OBJECT_STORE_UNAVAILABLE", "Private media storage is unavailable.");
        }
      }
      return errorResponse(context, 409, "OBJECT_INVALID", "The uploaded media does not match its manifest.");
    }
    try {
      const completed = await options.repository.completeCommunicationObject(
        device.workspaceId,
        device.deviceId,
        object.objectId,
        new Date(),
      );
      return context.json({ object_id: completed.objectId, state: completed.state });
    } catch (error) {
      return repositoryErrorResponse(context, error);
    }
  });

  app.post("/v1/agent/screenshots/prepare", async (context) => {
    const device = await requireDevice(context, options.repository, "access");
    if (device instanceof Response) return device;
    const input = parseScreenshotPrepare(await context.req.json().catch(() => null));
    if (input === null) {
      return errorResponse(context, 400, "REQUEST_INVALID", "Invalid screenshot upload request.");
    }
    if (options.objectStore === undefined) {
      return errorResponse(context, 503, "OBJECT_STORE_UNAVAILABLE", "Private screenshot storage is unavailable.");
    }
    let screenshot: ScreenshotRecord;
    try {
      screenshot = await options.repository.prepareScreenshot(device.workspaceId, device.deviceId, {
        screenshotId: input.screenshotId,
        objectKey: `screenshots/${randomUUID()}`,
        requestId: input.requestId,
        trigger: input.trigger,
        capturedAt: input.capturedAt,
        appBundleId: input.appBundleId,
        pixelWidth: input.pixelWidth,
        pixelHeight: input.pixelHeight,
        expectedSha256: input.sha256,
        expectedSizeBytes: input.sizeBytes,
        expectedMimeType: "image/jpeg",
        now: new Date(),
      });
    } catch (error) {
      return repositoryErrorResponse(context, error);
    }
    if (screenshot.state === "completed") {
      return context.json({ screenshot_id: screenshot.screenshotId, state: "completed" });
    }
    try {
      const upload = await options.objectStore.signUpload(screenshot);
      return context.json({
        screenshot_id: screenshot.screenshotId,
        state: "prepared",
        upload: {
          url: upload.url,
          headers: upload.headers,
          expires_at: new Date(Date.now() + 300_000).toISOString(),
        },
      });
    } catch {
      return errorResponse(context, 503, "OBJECT_STORE_UNAVAILABLE", "Private screenshot storage is unavailable.");
    }
  });

  app.post("/v1/agent/photos/prepare", async (context) => {
    const device = await requireDevice(context, options.repository, "access");
    if (device instanceof Response) return device;
    const input = parsePhotoPrepare(await context.req.json().catch(() => null));
    if (input === null) return errorResponse(context, 400, "REQUEST_INVALID", "Invalid photo upload request.");
    if (options.objectStore === undefined) return errorResponse(context, 503, "OBJECT_STORE_UNAVAILABLE", "Private photo storage is unavailable.");
    let photo: PhotoLibraryAssetRecord;
    try {
      photo = await options.repository.preparePhotoLibraryAsset(device.workspaceId, device.deviceId, {
        ...input, objectKey: `photos/${device.deviceId}/${input.photoId}`, now: new Date(),
      });
    } catch (error) { return repositoryErrorResponse(context, error); }
    if (photo.state === "completed") return context.json({ photo_id: photo.photoId, state: "completed" });
    try {
      const upload = await options.objectStore.signUpload(photo);
      return context.json({ photo_id: photo.photoId, state: "prepared", upload: { url: upload.url, headers: upload.headers, expires_at: new Date(Date.now() + 300_000).toISOString() } });
    } catch { return errorResponse(context, 503, "OBJECT_STORE_UNAVAILABLE", "Private photo storage is unavailable."); }
  });

  app.post("/v1/agent/photos/complete", async (context) => {
    const device = await requireDevice(context, options.repository, "access");
    if (device instanceof Response) return device;
    const input = parsePhotoId(await context.req.json().catch(() => null));
    if (input === null) return errorResponse(context, 400, "REQUEST_INVALID", "Invalid photo completion.");
    if (options.objectStore === undefined) return errorResponse(context, 503, "OBJECT_STORE_UNAVAILABLE", "Private photo storage is unavailable.");
    let photo: PhotoLibraryAssetRecord;
    try { photo = await options.repository.loadDevicePhotoLibraryAsset(device.workspaceId, device.deviceId, input.photoId); }
    catch (error) { return repositoryErrorResponse(context, error); }
    if (photo.state === "completed") return context.json({ photo_id: photo.photoId, state: "completed" });
    let actual: R2ObjectHead | null;
    try { actual = await options.objectStore.headObject(photo.objectKey); }
    catch { return errorResponse(context, 503, "OBJECT_STORE_UNAVAILABLE", "Private photo storage is unavailable."); }
    if (actual === null || actual.sizeBytes !== photo.expectedSizeBytes || actual.mimeType !== photo.expectedMimeType || actual.sha256 !== photo.expectedSha256) {
      if (actual !== null) await options.objectStore.deleteObject(photo.objectKey).catch(() => undefined);
      return errorResponse(context, 409, "OBJECT_INVALID", "The uploaded photo does not match its manifest.");
    }
    try {
      const completed = await options.repository.completePhotoLibraryAsset(device.workspaceId, device.deviceId, photo.photoId, new Date());
      return context.json({ photo_id: completed.photoId, state: completed.state });
    } catch (error) { return repositoryErrorResponse(context, error); }
  });

  app.post("/v1/agent/screenshots/complete", async (context) => {
    const device = await requireDevice(context, options.repository, "access");
    if (device instanceof Response) return device;
    const input = parseScreenshotId(await context.req.json().catch(() => null));
    if (input === null) {
      return errorResponse(context, 400, "REQUEST_INVALID", "Invalid screenshot completion.");
    }
    if (options.objectStore === undefined) {
      return errorResponse(context, 503, "OBJECT_STORE_UNAVAILABLE", "Private screenshot storage is unavailable.");
    }
    let screenshot: ScreenshotRecord;
    try {
      screenshot = await options.repository.loadDeviceScreenshot(
        device.workspaceId,
        device.deviceId,
        input.screenshotId,
      );
    } catch (error) {
      return repositoryErrorResponse(context, error);
    }
    if (screenshot.state === "completed") {
      return context.json({ screenshot_id: screenshot.screenshotId, state: "completed" });
    }
    let actual: R2ObjectHead | null;
    try {
      actual = await options.objectStore.headObject(screenshot.objectKey);
    } catch {
      return errorResponse(context, 503, "OBJECT_STORE_UNAVAILABLE", "Private screenshot storage is unavailable.");
    }
    if (
      actual === null
      || actual.sizeBytes !== screenshot.expectedSizeBytes
      || actual.mimeType !== screenshot.expectedMimeType
      || actual.sha256 !== screenshot.expectedSha256
    ) {
      if (actual !== null) {
        try {
          await options.objectStore.deleteObject(screenshot.objectKey);
        } catch {
          return errorResponse(context, 503, "OBJECT_STORE_UNAVAILABLE", "Private screenshot storage is unavailable.");
        }
      }
      return errorResponse(context, 409, "OBJECT_INVALID", "The uploaded screenshot does not match its manifest.");
    }
    try {
      const completed = await options.repository.completeScreenshot(
        device.workspaceId,
        device.deviceId,
        screenshot.screenshotId,
        new Date(),
      );
      return context.json({ screenshot_id: completed.screenshotId, state: completed.state });
    } catch (error) {
      return repositoryErrorResponse(context, error);
    }
  });

  app.post("/v1/agent/screenshots/fail", async (context) => {
    const device = await requireDevice(context, options.repository, "access");
    if (device instanceof Response) return device;
    const input = parseScreenshotFailure(await context.req.json().catch(() => null));
    if (input === null) {
      return errorResponse(context, 400, "REQUEST_INVALID", "Invalid screenshot failure report.");
    }
    try {
      await options.repository.failScreenshotRequest(
        device.workspaceId,
        device.deviceId,
        input.requestId,
        input.errorCode,
        new Date(),
      );
      return context.body(null, 204);
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

  app.get("/v1/network-locations", async (context) => {
    const principal = await requireOwner(context, options.ownerAuthenticator);
    if (principal instanceof Response) return principal;
    try {
      const locations = await options.repository.listOwnerNetworkLocations(
        principal.workspaceId,
        principal.userId,
      );
      return context.json({ locations: locations.map(networkLocationResponse) });
    } catch (error) {
      return repositoryErrorResponse(context, error);
    }
  });

  app.post("/v1/network-locations", async (context) => {
    const principal = await requireOwner(context, options.ownerAuthenticator);
    if (principal instanceof Response) return principal;
    const input = parseNetworkLocation(await context.req.json().catch(() => null));
    if (input === null) {
      return errorResponse(context, 400, "REQUEST_INVALID", "Invalid network location.");
    }
    try {
      const location = await options.repository.createOwnerNetworkLocation({
        locationId: randomUUID(),
        workspaceId: principal.workspaceId,
        actorUserId: principal.userId,
        ...input,
        now: new Date(),
      });
      return context.json({ location: networkLocationResponse(location) }, 201);
    } catch (error) {
      return repositoryErrorResponse(context, error);
    }
  });

  app.delete("/v1/network-locations/:locationId", async (context) => {
    const principal = await requireOwner(context, options.ownerAuthenticator);
    if (principal instanceof Response) return principal;
    const locationId = context.req.param("locationId");
    if (!isUuid(locationId)) {
      return errorResponse(context, 400, "REQUEST_INVALID", "Invalid network location.");
    }
    try {
      await options.repository.deleteOwnerNetworkLocation(
        locationId,
        principal.workspaceId,
        principal.userId,
      );
      return context.body(null, 204);
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
        local_media_cleanup: localMediaCleanupResponse(device.localMediaCleanup),
      });
    } catch (error) {
      return repositoryErrorResponse(context, error);
    }
  });

  app.get("/v1/devices/:deviceId/system-metrics", async (context) => {
    const principal = await requireOwner(context, options.ownerAuthenticator);
    if (principal instanceof Response) return principal;
    const limit = parseMetricLimit(context.req.query("limit"));
    if (limit === null) {
      return errorResponse(context, 400, "REQUEST_INVALID", "Invalid system metric limit.");
    }
    try {
      const metrics = await options.repository.listOwnerSystemMetrics(
        context.req.param("deviceId"),
        principal.workspaceId,
        principal.userId,
        limit,
      );
      return context.json({
        metrics: metrics.map((metric) => ({
          event_id: metric.eventId,
          occurred_at: metric.occurredAt.toISOString(),
          metric_group: metric.metricGroup,
          payload: metric.payload,
        })),
      });
    } catch (error) {
      return repositoryErrorResponse(context, error);
    }
  });

  app.get("/v1/devices/:deviceId/communication/conversations", async (context) => {
    const principal = await requireOwner(context, options.ownerAuthenticator);
    if (principal instanceof Response) return principal;
    const limit = parseCommunicationLimit(context.req.query("limit"));
    const source = parseCommunicationSource(context.req.query("source"));
    if (limit === null || source === null) {
      return errorResponse(context, 400, "REQUEST_INVALID", "Invalid communication limit.");
    }
    try {
      const conversations = await options.repository.listOwnerCommunicationConversations(
        context.req.param("deviceId"),
        principal.workspaceId,
        principal.userId,
        source,
        limit,
      );
      return context.json({
        conversations: conversations.map((conversation) => ({
          conversation_id: conversation.conversationId,
          display_name: conversation.displayName,
          avatar_url: conversation.avatarUrl,
          scope: conversation.scope,
          member_count: conversation.memberCount,
          message_count: conversation.messageCount,
          last_message_at: conversation.lastMessageAt.toISOString(),
        })),
      });
    } catch (error) {
      return repositoryErrorResponse(context, error);
    }
  });

  app.get("/v1/devices/:deviceId/communication/objects/:objectId/read", async (context) => {
    const principal = await requireOwner(context, options.ownerAuthenticator);
    if (principal instanceof Response) return principal;
    if (options.objectStore === undefined) {
      return errorResponse(context, 503, "OBJECT_STORE_UNAVAILABLE", "Private media storage is unavailable.");
    }
    let object: CommunicationObjectRecord;
    try {
      object = await options.repository.loadOwnerCompletedCommunicationObject(
        principal.workspaceId,
        principal.userId,
        context.req.param("deviceId"),
        context.req.param("objectId"),
      );
    } catch (error) {
      return repositoryErrorResponse(context, error);
    }
    try {
      const read = await options.objectStore.signRead(object.objectKey);
      return context.json({ url: read.url, expires_at: read.expiresAt.toISOString() });
    } catch {
      return errorResponse(context, 503, "OBJECT_STORE_UNAVAILABLE", "Private media storage is unavailable.");
    }
  });

  app.get("/v1/devices/:deviceId/communication/conversations/:conversationId/messages", async (context) => {
    const principal = await requireOwner(context, options.ownerAuthenticator);
    if (principal instanceof Response) return principal;
    const limit = parseCommunicationLimit(context.req.query("limit"));
    const source = parseCommunicationSource(context.req.query("source"));
    const before = parseCommunicationMessageCursor(
      context.req.query("before"),
      context.req.query("before_event_id"),
    );
    if (limit === null || source === null || before === undefined) {
      return errorResponse(context, 400, "REQUEST_INVALID", "Invalid communication limit.");
    }
    try {
      const messages = await options.repository.listOwnerCommunicationMessages(
        context.req.param("deviceId"),
        context.req.param("conversationId"),
        principal.workspaceId,
        principal.userId,
        source,
        limit,
        before,
      );
      return context.json({
        messages: messages.map((message) => ({
          event_id: message.eventId,
          message_id: message.messageId,
          sender_id: message.senderId,
          sender_display_name: message.senderDisplayName,
          sender_avatar_url: message.senderAvatarUrl,
          occurred_at: message.occurredAt.toISOString(),
          direction: message.direction,
          kind: message.kind,
          text: message.text,
          attachments: message.attachments.map((attachment) => ({
            attachment_id: attachment.attachmentId,
            kind: attachment.kind,
            sha256: attachment.sha256,
            size_bytes: attachment.sizeBytes,
            mime_type: attachment.mimeType,
            file_name: attachment.fileName ?? undefined,
            object_id: attachment.objectId,
            object_state: attachment.objectState,
          })),
        })),
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

  app.post("/v1/devices/:deviceId/purge", async (context) => {
    const principal = await requireOwner(context, options.ownerAuthenticator);
    if (principal instanceof Response) return principal;
    const deviceId = context.req.param("deviceId");
    const body = await context.req.json().catch(() => null) as { confirmation?: unknown } | null;
    if (body?.confirmation !== deviceId) {
      return errorResponse(context, 400, "REQUEST_INVALID", "Device deletion confirmation does not match.");
    }
    if (options.objectStore === undefined) {
      return errorResponse(context, 503, "OBJECT_STORE_UNAVAILABLE", "Private media storage is unavailable.");
    }
    try {
      const objectKeys = await options.repository.listOwnerDeviceObjectKeys(deviceId, principal.workspaceId, principal.userId);
      for (let offset = 0; offset < objectKeys.length; offset += 16) {
        await Promise.all(objectKeys.slice(offset, offset + 16).map((objectKey) => options.objectStore!.deleteObject(objectKey)));
      }
      await options.repository.deleteOwnerDevice(deviceId, principal.workspaceId, principal.userId);
      return context.json({ deleted: true, deleted_object_count: objectKeys.length });
    } catch (error) {
      return repositoryErrorResponse(context, error);
    }
  });

  app.post("/v1/devices/:deviceId/communication/local-media/cleanup", async (context) => {
    const principal = await requireOwner(context, options.ownerAuthenticator);
    if (principal instanceof Response) return principal;
    try {
      const cleanup = await options.repository.requestLocalMediaCleanup({
        requestId: randomUUID(),
        actorUserId: principal.userId,
        workspaceId: principal.workspaceId,
        deviceId: context.req.param("deviceId"),
        now: new Date(),
      });
      return context.json({ cleanup: localMediaCleanupResponse(cleanup) }, 202);
    } catch (error) {
      return repositoryErrorResponse(context, error);
    }
  });

  app.post("/v1/devices/:deviceId/screenshots", async (context) => {
    const principal = await requireOwner(context, options.ownerAuthenticator);
    if (principal instanceof Response) return principal;
    try {
      const request = await options.repository.requestScreenshot({
        requestId: randomUUID(),
        actorUserId: principal.userId,
        workspaceId: principal.workspaceId,
        deviceId: context.req.param("deviceId"),
        now: new Date(),
      });
      return context.json({ request: screenshotRequestResponse(request) }, 202);
    } catch (error) {
      return repositoryErrorResponse(context, error);
    }
  });

  app.get("/v1/devices/:deviceId/screenshots", async (context) => {
    const principal = await requireOwner(context, options.ownerAuthenticator);
    if (principal instanceof Response) return principal;
    const limit = parseScreenshotLimit(context.req.query("limit"));
    if (limit === null) return errorResponse(context, 400, "REQUEST_INVALID", "Invalid screenshot limit.");
    try {
      const screenshots = await options.repository.listOwnerScreenshots(
        context.req.param("deviceId"),
        principal.workspaceId,
        principal.userId,
        limit,
      );
      return context.json({ screenshots: screenshots.map(screenshotResponse) });
    } catch (error) {
      return repositoryErrorResponse(context, error);
    }
  });

  app.get("/v1/devices/:deviceId/screenshots/:screenshotId/read", async (context) => {
    const principal = await requireOwner(context, options.ownerAuthenticator);
    if (principal instanceof Response) return principal;
    if (options.objectStore === undefined) {
      return errorResponse(context, 503, "OBJECT_STORE_UNAVAILABLE", "Private screenshot storage is unavailable.");
    }
    let screenshot: ScreenshotRecord;
    try {
      screenshot = await options.repository.loadOwnerCompletedScreenshot(
        principal.workspaceId,
        principal.userId,
        context.req.param("deviceId"),
        context.req.param("screenshotId"),
      );
    } catch (error) {
      return repositoryErrorResponse(context, error);
    }
    try {
      const read = await options.objectStore.signRead(screenshot.objectKey);
      return context.json({ url: read.url, expires_at: read.expiresAt.toISOString() });
    } catch {
      return errorResponse(context, 503, "OBJECT_STORE_UNAVAILABLE", "Private screenshot storage is unavailable.");
    }
  });

  app.get("/v1/devices/:deviceId/photos", async (context) => {
    const principal = await requireOwner(context, options.ownerAuthenticator);
    if (principal instanceof Response) return principal;
    const limit = parseScreenshotLimit(context.req.query("limit"));
    if (limit === null) return errorResponse(context, 400, "REQUEST_INVALID", "Invalid photo limit.");
    try {
      const photos = await options.repository.listOwnerPhotoLibraryAssets(context.req.param("deviceId"), principal.workspaceId, principal.userId, limit);
      return context.json({ photos: photos.map(photoResponse) });
    } catch (error) { return repositoryErrorResponse(context, error); }
  });

  app.get("/v1/devices/:deviceId/photos/:photoId/read", async (context) => {
    const principal = await requireOwner(context, options.ownerAuthenticator);
    if (principal instanceof Response) return principal;
    if (options.objectStore === undefined) return errorResponse(context, 503, "OBJECT_STORE_UNAVAILABLE", "Private photo storage is unavailable.");
    try {
      const photo = await options.repository.loadOwnerCompletedPhotoLibraryAsset(principal.workspaceId, principal.userId, context.req.param("deviceId"), context.req.param("photoId"));
      const read = await options.objectStore.signRead(photo.objectKey);
      return context.json({ url: read.url, expires_at: read.expiresAt.toISOString() });
    } catch (error) { return repositoryErrorResponse(context, error); }
  });

  return app;
}

function pairingAuthorizationURL(sessionId: string, callbackState: string): string {
  const url = new URL("/pair", productionDashboardOrigin);
  url.search = new URLSearchParams({ session_id: sessionId, callback_state: callbackState }).toString();
  return url.toString();
}

function parseMetricLimit(value: string | undefined): number | null {
  if (value === undefined) return 20;
  const limit = Number(value);
  return Number.isInteger(limit) && limit >= 1 && limit <= 100 ? limit : null;
}

function parseCommunicationLimit(value: string | undefined): number | null {
  if (value === undefined) return 50;
  const limit = Number(value);
  return Number.isInteger(limit) && limit >= 1 && limit <= 100 ? limit : null;
}

function parseCommunicationSource(value: string | undefined): CommunicationSource | null {
  if (value === undefined || value === "communication.wechat") return "communication.wechat";
  return value === "communication.messages" ? value : null;
}

function parseScreenshotLimit(value: string | undefined): number | null {
  if (value === undefined) return 50;
  const limit = Number(value);
  return Number.isInteger(limit) && limit >= 1 && limit <= 100 ? limit : null;
}

function parseCommunicationMessageCursor(
  occurredAt: string | undefined,
  eventId: string | undefined,
): { occurredAt: Date; eventId: string } | null | undefined {
  if (occurredAt === undefined && eventId === undefined) return null;
  if (occurredAt === undefined || eventId === undefined || !isUuid(eventId)) return undefined;
  const parsed = new Date(occurredAt);
  return Number.isNaN(parsed.getTime()) ? undefined : { occurredAt: parsed, eventId };
}

function parseObjectReference(value: unknown): { eventId: string; attachmentId: string } | null {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return null;
  const body = value as Record<string, unknown>;
  return Object.keys(body).length === 2
    && isUuid(body.event_id)
    && typeof body.attachment_id === "string"
    && body.attachment_id.length > 0
    ? { eventId: body.event_id, attachmentId: body.attachment_id }
    : null;
}

function parseObjectId(value: unknown): { objectId: string } | null {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return null;
  const body = value as Record<string, unknown>;
  return Object.keys(body).length === 1 && isUuid(body.object_id) ? { objectId: body.object_id } : null;
}

function parsePhotoPrepare(value: unknown) {
  if (!isObjectRecord(value) || !hasOnlyKeys(value, [
    "photo_id", "event_id", "asset_id", "captured_at", "media_type", "original_filename",
    "mime_type", "pixel_width", "pixel_height", "duration_seconds", "album_names", "sha256", "size_bytes",
  ])) return null;
  const capturedAt = new Date(String(value.captured_at));
  if (!isUuid(value.photo_id) || !isUuid(value.event_id)
    || typeof value.asset_id !== "string" || value.asset_id.length === 0 || value.asset_id.length > 1024
    || Number.isNaN(capturedAt.getTime()) || (value.media_type !== "image" && value.media_type !== "video")
    || typeof value.original_filename !== "string" || value.original_filename.length === 0 || value.original_filename.length > 1024
    || typeof value.mime_type !== "string" || value.mime_type.length === 0 || value.mime_type.length > 255
    || !Number.isInteger(value.pixel_width) || (value.pixel_width as number) <= 0
    || !Number.isInteger(value.pixel_height) || (value.pixel_height as number) <= 0
    || typeof value.duration_seconds !== "number" || !Number.isFinite(value.duration_seconds) || value.duration_seconds < 0
    || !Array.isArray(value.album_names) || value.album_names.length > 100 || value.album_names.some((name) => typeof name !== "string" || name.length === 0 || name.length > 1024)
    || typeof value.sha256 !== "string" || !/^[a-f0-9]{64}$/.test(value.sha256)
    || !Number.isSafeInteger(value.size_bytes) || (value.size_bytes as number) <= 0) return null;
  return {
    photoId: value.photo_id, eventId: value.event_id, assetId: value.asset_id, capturedAt,
    mediaType: value.media_type as "image" | "video", originalFilename: value.original_filename,
    expectedMimeType: value.mime_type, pixelWidth: value.pixel_width as number, pixelHeight: value.pixel_height as number,
    durationSeconds: value.duration_seconds, albumNames: [...value.album_names] as string[],
    expectedSha256: value.sha256, expectedSizeBytes: value.size_bytes as number,
  };
}

function parsePhotoId(value: unknown): { photoId: string } | null {
  return isObjectRecord(value) && hasOnlyKeys(value, ["photo_id"]) && isUuid(value.photo_id)
    ? { photoId: value.photo_id }
    : null;
}

function hasOnlyKeys(value: Record<string, unknown>, keys: string[]): boolean {
  const allowed = new Set(keys);
  return Object.keys(value).length === keys.length && Object.keys(value).every((key) => allowed.has(key));
}

function isObjectRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function parseScreenshotPrepare(value: unknown): {
  screenshotId: string;
  requestId: string | null;
  trigger: "manual" | "scheduled" | "activity";
  capturedAt: Date;
  appBundleId: string | null;
  pixelWidth: number;
  pixelHeight: number;
  sha256: string;
  sizeBytes: number;
} | null {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return null;
  const body = value as Record<string, unknown>;
  if (Object.keys(body).some((key) => ![
    "screenshot_id",
    "request_id",
    "trigger",
    "captured_at",
    "app_bundle_id",
    "pixel_width",
    "pixel_height",
    "sha256",
    "size_bytes",
    "mime_type",
  ].includes(key)) || Object.keys(body).length !== 10) return null;
  const capturedAt = typeof body.captured_at === "string" ? new Date(body.captured_at) : null;
  if (
    !isUuid(body.screenshot_id)
    || !(body.request_id === null || isUuid(body.request_id))
    || (body.trigger !== "manual" && body.trigger !== "scheduled" && body.trigger !== "activity")
    || capturedAt === null
    || Number.isNaN(capturedAt.getTime())
    || !(body.app_bundle_id === null || (
      typeof body.app_bundle_id === "string"
      && body.app_bundle_id.length > 0
      && body.app_bundle_id.length <= 255
      && /^[A-Za-z0-9.-]+$/.test(body.app_bundle_id)
    ))
    || !Number.isInteger(body.pixel_width)
    || (body.pixel_width as number) <= 0
    || (body.pixel_width as number) > 20_000
    || !Number.isInteger(body.pixel_height)
    || (body.pixel_height as number) <= 0
    || (body.pixel_height as number) > 20_000
    || typeof body.sha256 !== "string"
    || !/^[0-9a-f]{64}$/.test(body.sha256)
    || !Number.isSafeInteger(body.size_bytes)
    || (body.size_bytes as number) <= 0
    || (body.size_bytes as number) > 100 * 1024 * 1024
    || body.mime_type !== "image/jpeg"
  ) return null;
  return {
    screenshotId: body.screenshot_id,
    requestId: body.request_id,
    trigger: body.trigger,
    capturedAt,
    appBundleId: body.app_bundle_id,
    pixelWidth: body.pixel_width as number,
    pixelHeight: body.pixel_height as number,
    sha256: body.sha256,
    sizeBytes: body.size_bytes as number,
  };
}

function parseScreenshotId(value: unknown): { screenshotId: string } | null {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return null;
  const body = value as Record<string, unknown>;
  return Object.keys(body).length === 1 && isUuid(body.screenshot_id)
    ? { screenshotId: body.screenshot_id }
    : null;
}

function parseScreenshotFailure(value: unknown): { requestId: string; errorCode: string } | null {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return null;
  const body = value as Record<string, unknown>;
  return Object.keys(body).length === 2
    && isUuid(body.request_id)
    && typeof body.error_code === "string"
    && /^[A-Z][A-Z0-9_]{0,63}$/.test(body.error_code)
    ? { requestId: body.request_id, errorCode: body.error_code }
    : null;
}

function isUuid(value: unknown): value is string {
  return typeof value === "string" && /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(value);
}

function parseNetworkLocation(value: unknown): {
  name: string;
  matchSsid: string | null;
  matchBssid: string | null;
  country: string | null;
  region: string | null;
  city: string | null;
} | null {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return null;
  const body = value as Record<string, unknown>;
  if (Object.keys(body).some((key) => !["name", "match_ssid", "match_bssid", "country", "region", "city"].includes(key))) return null;
  const name = normalizedText(body.name, 100);
  const matchSsid = nullableText(body.match_ssid, 128);
  const rawBssid = nullableText(body.match_bssid, 17);
  const matchBssid = rawBssid?.toUpperCase() ?? null;
  const country = nullableText(body.country, 100);
  const region = nullableText(body.region, 100);
  const city = nullableText(body.city, 100);
  if (
    name === null
    || matchSsid === undefined
    || rawBssid === undefined
    || country === undefined
    || region === undefined
    || city === undefined
    || (matchSsid === null && matchBssid === null)
    || (matchBssid !== null && !/^[0-9A-F]{2}(:[0-9A-F]{2}){5}$/.test(matchBssid))
  ) return null;
  return { name, matchSsid, matchBssid, country, region, city };
}

function normalizedText(value: unknown, maximum: number): string | null {
  if (typeof value !== "string") return null;
  const text = value.trim();
  return text.length > 0 && text.length <= maximum ? text : null;
}

function nullableText(value: unknown, maximum: number): string | null | undefined {
  if (value === null || value === undefined || value === "") return null;
  return typeof value === "string" ? normalizedText(value, maximum) ?? undefined : undefined;
}

export interface ProductionEnvironment {
  DATABASE_URL?: string;
  BETTER_AUTH_SECRET?: string;
  BETTER_AUTH_URL?: string;
  R2_ENDPOINT?: string;
  R2_ACCESS_KEY_ID?: string;
  R2_SECRET_ACCESS_KEY?: string;
  R2_BUCKET?: string;
  R2_BUCKET_PUBLIC?: string;
  RAILWAY_ENVIRONMENT?: string;
}

export interface ProductionRuntime {
  app: Hono;
  repository: DrizzleControlRepository;
  objectStore?: R2ObjectStore;
}

export function createProductionRuntime(
  environment: ProductionEnvironment = process.env,
): ProductionRuntime {
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
  const objectStore = createR2ObjectStore(environment);
  const app = createApp({
    repository,
    ownerAuthenticator: createBetterAuthOwnerAuthenticator(auth, repository),
    clientAddress: createRailwayClientAddress(environment),
    geoEnricher: new CountryIsGeoEnricher(),
    ...(objectStore === undefined ? {} : { objectStore }),
  });
  app.all("/api/auth/*", (context) => auth.handler(context.req.raw));
  return {
    app,
    repository,
    ...(objectStore === undefined ? {} : { objectStore }),
  };
}

export function createProductionApp(environment: ProductionEnvironment = process.env): Hono {
  return createProductionRuntime(environment).app;
}

export function createRailwayClientAddress(
  environment: { RAILWAY_ENVIRONMENT?: string },
): (request: Request) => string | undefined {
  const onRailway = typeof environment.RAILWAY_ENVIRONMENT === "string"
    && environment.RAILWAY_ENVIRONMENT.length > 0;
  return (request) => {
    if (!onRailway) return undefined;
    const value = request.headers.get("x-real-ip");
    return value !== null && isIP(value) !== 0 ? value : undefined;
  };
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
  environment: { TRUSTED_PROXY_CLIENT_IP_HMAC_SECRET?: string },
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

async function parseCallbackStateRequest(request: Request): Promise<string | null> {
  const contentType = request.headers.get("content-type") ?? "";
  if (contentType.startsWith("application/json")) {
    return parseCallbackState(await request.json().catch(() => null));
  }
  if (contentType.startsWith("application/x-www-form-urlencoded")) {
    const parameters = new URLSearchParams(await request.text().catch(() => ""));
    const entries = [...parameters.entries()];
    return entries.length === 1 && entries[0]?.[0] === "callback_state"
      ? parseCallbackState({ callback_state: entries[0][1] })
      : null;
  }
  return null;
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
    localMedia: {
      completedFileCount: number;
      completedBytes: number;
      protectedFileCount: number;
      protectedBytes: number;
    };
    network: {
      interfaceType: string;
      wifiIdentityAvailable: boolean;
      ssid: string | null;
      bssid: string | null;
      localIpv4: string | null;
      localIpv6: string | null;
      observedExitIp: string | null;
      ipLocation: { country: string | null; region: string | null; city: string | null; accuracy: string } | null;
      location: {
        latitude: number;
        longitude: number;
        horizontalAccuracyMeters: number;
        observedAt: Date;
      } | null;
    } | null;
    matchedLocation: {
      locationId: string;
      name: string;
      matchSsid: string | null;
      matchBssid: string | null;
      country: string | null;
      region: string | null;
      city: string | null;
      createdAt: Date;
      updatedAt: Date;
    } | null;
    previousNetwork: {
      network: {
        interfaceType: string;
        wifiIdentityAvailable: boolean;
        ssid: string | null;
        bssid: string | null;
        localIpv4: string | null;
        localIpv6: string | null;
        observedExitIp: string | null;
        ipLocation: { country: string | null; region: string | null; city: string | null; accuracy: string } | null;
        location: {
          latitude: number;
          longitude: number;
          horizontalAccuracyMeters: number;
          observedAt: Date;
        } | null;
      };
      matchedLocation: {
        locationId: string;
        name: string;
        matchSsid: string | null;
        matchBssid: string | null;
        country: string | null;
        region: string | null;
        city: string | null;
        createdAt: Date;
        updatedAt: Date;
      } | null;
      observedAt: Date;
    } | null;
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
            local_media: {
              completed_file_count: device.status.localMedia.completedFileCount,
              completed_bytes: device.status.localMedia.completedBytes,
              protected_file_count: device.status.localMedia.protectedFileCount,
              protected_bytes: device.status.localMedia.protectedBytes,
            },
            network: device.status.network === null ? null : {
              interface_type: device.status.network.interfaceType,
              wifi_identity_available: device.status.network.wifiIdentityAvailable,
              ssid: device.status.network.ssid,
              bssid: device.status.network.bssid,
              local_ipv4: device.status.network.localIpv4,
              local_ipv6: device.status.network.localIpv6,
              observed_exit_ip: device.status.network.observedExitIp,
              exit_ip_location: device.status.network.ipLocation,
              device_location: device.status.network.location === null ? null : {
                latitude: device.status.network.location.latitude,
                longitude: device.status.network.location.longitude,
                horizontal_accuracy_meters: device.status.network.location.horizontalAccuracyMeters,
                observed_at: device.status.network.location.observedAt.toISOString(),
              },
              matched_location: device.status.matchedLocation === null
                ? null
                : networkLocationResponse(device.status.matchedLocation),
            },
            previous_network: device.status.previousNetwork === null ? null : {
              interface_type: device.status.previousNetwork.network.interfaceType,
              wifi_identity_available: device.status.previousNetwork.network.wifiIdentityAvailable,
              ssid: device.status.previousNetwork.network.ssid,
              bssid: device.status.previousNetwork.network.bssid,
              local_ipv4: device.status.previousNetwork.network.localIpv4,
              local_ipv6: device.status.previousNetwork.network.localIpv6,
              observed_exit_ip: device.status.previousNetwork.network.observedExitIp,
              exit_ip_location: device.status.previousNetwork.network.ipLocation,
              device_location: device.status.previousNetwork.network.location === null ? null : {
                latitude: device.status.previousNetwork.network.location.latitude,
                longitude: device.status.previousNetwork.network.location.longitude,
                horizontal_accuracy_meters: device.status.previousNetwork.network.location.horizontalAccuracyMeters,
                observed_at: device.status.previousNetwork.network.location.observedAt.toISOString(),
              },
              matched_location: device.status.previousNetwork.matchedLocation === null
                ? null
                : networkLocationResponse(device.status.previousNetwork.matchedLocation),
              observed_at: device.status.previousNetwork.observedAt.toISOString(),
            },
            observed_at: device.status.observedAt.toISOString(),
          },
  };
}

function networkLocationResponse(location: {
  locationId: string;
  name: string;
  matchSsid: string | null;
  matchBssid: string | null;
  country: string | null;
  region: string | null;
  city: string | null;
  createdAt: Date;
  updatedAt: Date;
}) {
  return {
    location_id: location.locationId,
    name: location.name,
    match_ssid: location.matchSsid,
    match_bssid: location.matchBssid,
    country: location.country,
    region: location.region,
    city: location.city,
    created_at: location.createdAt.toISOString(),
    updated_at: location.updatedAt.toISOString(),
  };
}

function localMediaCleanupResponse(cleanup: {
  requestId: string;
  status: string;
  requestedAt: Date;
  completedAt: Date | null;
  deletedFileCount: number | null;
  freedBytes: number | null;
  errorCode: string | null;
} | null) {
  return cleanup === null ? null : {
    request_id: cleanup.requestId,
    status: cleanup.status,
    requested_at: cleanup.requestedAt.toISOString(),
    completed_at: cleanup.completedAt?.toISOString() ?? null,
    deleted_file_count: cleanup.deletedFileCount,
    freed_bytes: cleanup.freedBytes,
    error_code: cleanup.errorCode,
  };
}

function collectorConfigResponse(config: StoredCollectorConfig) {
  return {
    network: { enabled: config.networkEnabled },
    "screen.capture": {
      enabled: config.screenCaptureEnabled,
      scheduled_enabled: config.screenCaptureScheduledEnabled,
      interval_seconds: config.screenCaptureIntervalSeconds,
      activity_enabled: config.screenCaptureActivityEnabled,
      activity_min_interval_seconds: config.screenCaptureActivityMinIntervalSeconds,
      excluded_bundle_ids: [...config.screenCaptureExcludedBundleIds],
    },
    "communication.wechat": {
      enabled: config.wechatEnabled,
      directions: ["incoming", "outgoing"] as const,
      message_types: ["text", "audio", "image", "video"] as const,
      conversation_scope: "direct_and_group_at_most_fifteen_members" as const,
      max_group_members: 15,
      sync_mode: "full" as const,
      retention_days: 180,
    },
    "communication.messages": {
      enabled: config.messagesEnabled,
      directions: ["incoming", "outgoing"] as const,
      message_types: ["text"] as const,
      conversation_scope: "all" as const,
      initial_lookback_days: 7,
      sync_mode: "full" as const,
      attachments_enabled: false,
      attachment_retention_days: 7,
    },
    "photos.library": {
      enabled: config.photosEnabled,
      media_types: ["image", "video"] as const,
      include_originals: true,
      include_album_names: true,
      initial_lookback_days: 60,
      cloud_retention: "permanent" as const,
    },
  };
}

function screenshotRequestResponse(request: {
  requestId: string;
  status: string;
  requestedAt: Date;
  completedAt: Date | null;
  screenshotId: string | null;
  errorCode: string | null;
}) {
  return {
    request_id: request.requestId,
    status: request.status,
    requested_at: request.requestedAt.toISOString(),
    completed_at: request.completedAt?.toISOString() ?? null,
    screenshot_id: request.screenshotId,
    error_code: request.errorCode,
  };
}

function screenshotResponse(screenshot: ScreenshotRecord) {
  return {
    screenshot_id: screenshot.screenshotId,
    request_id: screenshot.requestId,
    trigger: screenshot.trigger,
    captured_at: screenshot.capturedAt.toISOString(),
    app_bundle_id: screenshot.appBundleId,
    pixel_width: screenshot.pixelWidth,
    pixel_height: screenshot.pixelHeight,
    size_bytes: screenshot.expectedSizeBytes,
    mime_type: screenshot.expectedMimeType,
  };
}

function photoResponse(photo: PhotoLibraryAssetRecord) {
  return {
    photo_id: photo.photoId,
    captured_at: photo.capturedAt.toISOString(),
    media_type: photo.mediaType,
    original_filename: photo.originalFilename,
    mime_type: photo.expectedMimeType,
    pixel_width: photo.pixelWidth,
    pixel_height: photo.pixelHeight,
    duration_seconds: photo.durationSeconds,
    album_names: [...photo.albumNames],
    size_bytes: photo.expectedSizeBytes,
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
