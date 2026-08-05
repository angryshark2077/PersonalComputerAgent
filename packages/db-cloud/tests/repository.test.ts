import assert from "node:assert/strict";
import test from "node:test";

import {
  ControlRepositoryError,
  MemoryControlRepository,
} from "../src/repository.js";

const hash = (character: string): string => character.repeat(64);
const now = new Date("2026-07-31T12:00:00.000Z");
const later = new Date("2026-07-31T12:05:00.000Z");
const workspaceId = "01982222-7222-8222-8222-222222222222";
const ownerUserId = "01983333-7333-8333-8333-333333333333";
const membership = { workspaceId, userId: ownerUserId };

async function pairDevice(repository: MemoryControlRepository): Promise<void> {
  await repository.createPairingSession({
    sessionIdHash: hash("1"),
    devicePublicKeyHash: hash("2"),
    codeChallenge: "challenge-a",
    callbackUri: "http://127.0.0.1:43123/pca/pair/callback",
    callbackStateHash: hash("s"),
    expiresAt: later,
    createdAt: now,
  });
  await repository.authorizePairingSession({
    sessionIdHash: hash("1"),
    authorizationCodeHash: hash("3"),
    workspaceId,
    ownerUserId,
    callbackStateHash: hash("s"),
    expiresAt: later,
    now,
  });
  await repository.consumeAuthorizationCode({
    sessionIdHash: hash("1"),
    authorizationCodeHash: hash("3"),
    codeChallenge: "challenge-a",
    deviceId: "01981111-7111-8111-8111-111111111111",
    accessTokenHash: hash("5"),
    refreshTokenHash: hash("6"),
    accessExpiresAt: later,
    refreshExpiresAt: new Date("2026-08-30T12:00:00.000Z"),
    now,
  });
}

test("authorization codes are PKCE-bound and consumed only once", async () => {
  const repository = new MemoryControlRepository([membership]);
  await pairDevice(repository);

  await assert.rejects(
    repository.consumeAuthorizationCode({
      sessionIdHash: hash("1"),
      authorizationCodeHash: hash("3"),
      codeChallenge: "challenge-a",
      deviceId: "01984444-7444-8444-8444-444444444444",
      accessTokenHash: hash("7"),
      refreshTokenHash: hash("8"),
      accessExpiresAt: later,
      refreshExpiresAt: later,
      now,
    }),
    (error) =>
      error instanceof ControlRepositoryError && error.code === "PAIRING_REPLAYED",
  );
});

test("pairing session identifiers are unique and expired sessions cannot be authorized", async () => {
  const repository = new MemoryControlRepository([membership]);
  const session = {
    sessionIdHash: hash("9"),
    devicePublicKeyHash: hash("8"),
    codeChallenge: "challenge-expired",
    callbackUri: "http://127.0.0.1:43123/pca/pair/callback",
    callbackStateHash: hash("s"),
    expiresAt: now,
    createdAt: new Date("2026-07-31T11:55:00.000Z"),
  };
  await repository.createPairingSession(session);
  await assert.rejects(
    repository.createPairingSession(session),
    (error) => error instanceof ControlRepositoryError && error.code === "CONFLICT",
  );
  await assert.rejects(
    repository.authorizePairingSession({
      sessionIdHash: hash("9"),
      authorizationCodeHash: hash("7"),
      workspaceId,
      ownerUserId,
      callbackStateHash: hash("s"),
      expiresAt: later,
      now,
    }),
    (error) =>
      error instanceof ControlRepositoryError && error.code === "PAIRING_EXPIRED",
  );
});

test("wrong PKCE challenge does not consume the authorization code", async () => {
  const repository = new MemoryControlRepository([membership]);
  await repository.createPairingSession({
    sessionIdHash: hash("a"),
    devicePublicKeyHash: hash("b"),
    codeChallenge: "expected-challenge",
    callbackUri: "http://127.0.0.1:43123/pca/pair/callback",
    callbackStateHash: hash("s"),
    expiresAt: later,
    createdAt: now,
  });
  await repository.authorizePairingSession({
    sessionIdHash: hash("a"),
    authorizationCodeHash: hash("c"),
    workspaceId,
    ownerUserId,
    callbackStateHash: hash("s"),
    expiresAt: later,
    now,
  });

  const exchange = {
    sessionIdHash: hash("a"),
    authorizationCodeHash: hash("c"),
    deviceId: "01981111-7111-8111-8111-111111111111",
    accessTokenHash: hash("e"),
    refreshTokenHash: hash("f"),
    accessExpiresAt: later,
    refreshExpiresAt: later,
    now,
  };
  await assert.rejects(
    repository.consumeAuthorizationCode({
      ...exchange,
      codeChallenge: "wrong-challenge",
    }),
    (error) => error instanceof ControlRepositoryError && error.code === "PKCE_INVALID",
  );
  const grant = await repository.consumeAuthorizationCode({
    ...exchange,
    codeChallenge: "expected-challenge",
  });
  assert.equal(grant.credentialGeneration, 1);
});

test("control state is Workspace-scoped, monotonic, and audited", async () => {
  const repository = new MemoryControlRepository([membership]);
  await pairDevice(repository);

  await assert.rejects(
    repository.loadControlSnapshot(
      "01981111-7111-8111-8111-111111111111",
      "01989999-7999-8999-8999-999999999999",
    ),
    (error) =>
      error instanceof ControlRepositoryError && error.code === "WORKSPACE_FORBIDDEN",
  );

  const revision = await repository.appendConfigAudit({
    auditId: "01985555-7555-8555-8555-555555555555",
    actorUserId: ownerUserId,
    workspaceId,
    deviceId: "01981111-7111-8111-8111-111111111111",
    config: {
      networkEnabled: true,
      wechatEnabled: false,
      messagesEnabled: false,
      photosEnabled: false,
      screenCaptureEnabled: false,
      screenCaptureScheduledEnabled: true,
      screenCaptureIntervalSeconds: 300,
      screenCaptureActivityEnabled: true,
      screenCaptureActivityMinIntervalSeconds: 30,
      screenCaptureExcludedBundleIds: [],
    },
    now,
  });
  assert.equal(revision, 1);
  const snapshot = await repository.loadControlSnapshot(
    "01981111-7111-8111-8111-111111111111",
    workspaceId,
  );
  assert.equal(snapshot.configuration_revision, 1);
  assert.deepEqual(snapshot.collectors.network, { enabled: true });
  assert.deepEqual(snapshot.collectors["communication.wechat"], {
    enabled: false,
    directions: ["incoming", "outgoing"],
    message_types: ["text", "audio", "image", "video"],
    conversation_scope: "direct_and_group_at_most_fifteen_members",
    max_group_members: 15,
    sync_mode: "full",
    retention_days: 180,
  });
});

test("credential rotation revokes the prior refresh hash", async () => {
  const repository = new MemoryControlRepository([membership]);
  await pairDevice(repository);
  const rotation = {
    workspaceId,
    deviceId: "01981111-7111-8111-8111-111111111111",
    currentRefreshTokenHash: hash("6"),
    newAccessTokenHash: hash("a"),
    newRefreshTokenHash: hash("b"),
    accessExpiresAt: later,
    refreshExpiresAt: new Date("2026-09-30T12:00:00.000Z"),
    now,
  };

  const grant = await repository.rotateDeviceCredentials(rotation);
  assert.equal(grant.credentialGeneration, 2);
  await assert.rejects(
    repository.rotateDeviceCredentials({
      ...rotation,
      newAccessTokenHash: hash("c"),
      newRefreshTokenHash: hash("d"),
    }),
    (error) =>
      error instanceof ControlRepositoryError && error.code === "CREDENTIAL_INVALID",
  );
});

test("heartbeats are append-only and Workspace-scoped", async () => {
  const repository = new MemoryControlRepository([membership]);
  await pairDevice(repository);
  const heartbeat = {
    heartbeatId: "01986666-7666-8666-8666-666666666666",
    workspaceId,
    deviceId: "01981111-7111-8111-8111-111111111111",
    receivedAt: now,
    agentVersion: "0.3.0",
    presence: "online" as const,
    outboxDepth: 0,
    localMedia: {
      completedFileCount: 2,
      completedBytes: 1024,
      protectedFileCount: 1,
      protectedBytes: 512,
    },
    cleanupResult: null,
    network: null,
  };
  await repository.recordHeartbeat(heartbeat);
  await assert.rejects(
    repository.recordHeartbeat(heartbeat),
    (error) => error instanceof ControlRepositoryError && error.code === "CONFLICT",
  );
});

test("screenshot retention selects and deletes only expired completed captures", async () => {
  const repository = new MemoryControlRepository([membership]);
  await pairDevice(repository);
  const deviceId = "01981111-7111-8111-8111-111111111111";
  const prepare = async (screenshotId: string, capturedAt: Date) => repository.prepareScreenshot(
    workspaceId,
    deviceId,
    {
      screenshotId,
      objectKey: `screenshots/${screenshotId}`,
      requestId: null,
      trigger: "activity",
      capturedAt,
      appBundleId: "com.example.App",
      pixelWidth: 1920,
      pixelHeight: 1080,
      expectedSha256: hash("a"),
      expectedSizeBytes: 1024,
      expectedMimeType: "image/jpeg",
      now: capturedAt,
    },
  );
  const expiredId = "01981111-7111-8111-8111-111111111112";
  const recentId = "01981111-7111-8111-8111-111111111113";
  const preparedId = "01981111-7111-8111-8111-111111111114";
  await prepare(expiredId, new Date("2026-07-20T12:00:00.000Z"));
  await repository.completeScreenshot(workspaceId, deviceId, expiredId, now);
  await prepare(recentId, new Date("2026-07-30T12:00:00.000Z"));
  await repository.completeScreenshot(workspaceId, deviceId, recentId, now);
  await prepare(preparedId, new Date("2026-07-20T12:00:00.000Z"));

  const cutoff = new Date("2026-07-24T12:00:00.000Z");
  const expired = await repository.listExpiredCompletedScreenshots(cutoff, 100);

  assert.deepEqual(expired.map((screenshot) => screenshot.screenshotId), [expiredId]);
  assert.equal(await repository.deleteExpiredCompletedScreenshot(recentId, cutoff), false);
  assert.equal(await repository.deleteExpiredCompletedScreenshot(expiredId, cutoff), true);
  assert.equal((await repository.listExpiredCompletedScreenshots(cutoff, 100)).length, 0);
});

test("pairing authorization and config audit require Owner membership", async () => {
  const repository = new MemoryControlRepository([membership]);
  await repository.createPairingSession({
    sessionIdHash: hash("a"),
    devicePublicKeyHash: hash("b"),
    codeChallenge: "challenge-owner",
    callbackUri: "http://127.0.0.1:43123/pca/pair/callback",
    callbackStateHash: hash("s"),
    expiresAt: later,
    createdAt: now,
  });
  await assert.rejects(
    repository.authorizePairingSession({
      sessionIdHash: hash("a"),
      authorizationCodeHash: hash("c"),
      workspaceId,
      ownerUserId: "01987777-7777-8777-8777-777777777777",
      callbackStateHash: hash("s"),
      expiresAt: later,
      now,
    }),
    (error) =>
      error instanceof ControlRepositoryError && error.code === "WORKSPACE_FORBIDDEN",
  );

  await pairDevice(repository);
  await assert.rejects(
    repository.appendConfigAudit({
      auditId: "01988888-7888-8888-8888-888888888888",
      actorUserId: "01987777-7777-8777-8777-777777777777",
      workspaceId,
      deviceId: "01981111-7111-8111-8111-111111111111",
      config: {
        networkEnabled: false,
        wechatEnabled: false,
        messagesEnabled: false,
        photosEnabled: false,
        screenCaptureEnabled: false,
        screenCaptureScheduledEnabled: true,
        screenCaptureIntervalSeconds: 300,
        screenCaptureActivityEnabled: true,
        screenCaptureActivityMinIntervalSeconds: 30,
        screenCaptureExcludedBundleIds: [],
      },
      now,
    }),
    (error) =>
      error instanceof ControlRepositoryError && error.code === "WORKSPACE_FORBIDDEN",
  );
});

test("device and credential hashes are globally unique", async () => {
  const repository = new MemoryControlRepository([membership]);
  await pairDevice(repository);
  await repository.createPairingSession({
    sessionIdHash: hash("7"),
    devicePublicKeyHash: hash("2"),
    codeChallenge: "challenge-duplicate-key",
    callbackUri: "http://127.0.0.1:43123/pca/pair/callback",
    callbackStateHash: hash("s"),
    expiresAt: later,
    createdAt: now,
  });
  await repository.authorizePairingSession({
    sessionIdHash: hash("7"),
    authorizationCodeHash: hash("8"),
    workspaceId,
    ownerUserId,
    callbackStateHash: hash("s"),
    expiresAt: later,
    now,
  });
  await assert.rejects(
    repository.consumeAuthorizationCode({
      sessionIdHash: hash("7"),
      authorizationCodeHash: hash("8"),
      codeChallenge: "challenge-duplicate-key",
      deviceId: "01984444-7444-8444-8444-444444444444",
      accessTokenHash: hash("a"),
      refreshTokenHash: hash("b"),
      accessExpiresAt: later,
      refreshExpiresAt: later,
      now,
    }),
    (error) => error instanceof ControlRepositoryError && error.code === "CONFLICT",
  );

  await repository.createPairingSession({
    sessionIdHash: hash("0"),
    devicePublicKeyHash: hash("f"),
    codeChallenge: "challenge-duplicate-credential",
    callbackUri: "http://127.0.0.1:43123/pca/pair/callback",
    callbackStateHash: hash("s"),
    expiresAt: later,
    createdAt: now,
  });
  await repository.authorizePairingSession({
    sessionIdHash: hash("0"),
    authorizationCodeHash: hash("e"),
    workspaceId,
    ownerUserId,
    callbackStateHash: hash("s"),
    expiresAt: later,
    now,
  });
  const secondDevice = {
    sessionIdHash: hash("0"),
    authorizationCodeHash: hash("e"),
    codeChallenge: "challenge-duplicate-credential",
    deviceId: "01985555-7555-8555-8555-555555555555",
    accessExpiresAt: later,
    refreshExpiresAt: later,
    now,
  };
  await assert.rejects(
    repository.consumeAuthorizationCode({
      ...secondDevice,
      accessTokenHash: hash("5"),
      refreshTokenHash: hash("c"),
    }),
    (error) => error instanceof ControlRepositoryError && error.code === "CONFLICT",
  );
  await assert.rejects(
    repository.consumeAuthorizationCode({
      ...secondDevice,
      accessTokenHash: hash("d"),
      refreshTokenHash: hash("6"),
    }),
    (error) => error instanceof ControlRepositoryError && error.code === "CONFLICT",
  );

  const rotation = {
    workspaceId,
    deviceId: "01981111-7111-8111-8111-111111111111",
    currentRefreshTokenHash: hash("6"),
    accessExpiresAt: later,
    refreshExpiresAt: later,
    now,
  };
  await assert.rejects(
    repository.rotateDeviceCredentials({
      ...rotation,
      newAccessTokenHash: hash("5"),
      newRefreshTokenHash: hash("c"),
    }),
    (error) => error instanceof ControlRepositoryError && error.code === "CONFLICT",
  );
  await assert.rejects(
    repository.rotateDeviceCredentials({
      ...rotation,
      newAccessTokenHash: hash("d"),
      newRefreshTokenHash: hash("6"),
    }),
    (error) => error instanceof ControlRepositoryError && error.code === "CONFLICT",
  );
});
