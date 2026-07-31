import assert from "node:assert/strict";
import test from "node:test";

import {
  ControlRepositoryError,
  MemoryControlRepository,
} from "../src/repository.js";

const hash = (character: string): string => character.repeat(64);
const now = new Date("2026-07-31T12:00:00.000Z");
const later = new Date("2026-07-31T12:05:00.000Z");

async function pairDevice(repository: MemoryControlRepository): Promise<void> {
  await repository.createPairingSession({
    sessionIdHash: hash("1"),
    devicePublicKeyHash: hash("2"),
    codeChallenge: "challenge-a",
    callbackUri: "http://127.0.0.1:43123/pca/pair/callback",
    expiresAt: later,
    createdAt: now,
  });
  await repository.authorizePairingSession({
    sessionIdHash: hash("1"),
    authorizationCodeHash: hash("3"),
    workspaceId: "01982222-7222-8222-8222-222222222222",
    ownerUserId: "01983333-7333-8333-8333-333333333333",
    callbackStateHash: hash("4"),
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
  const repository = new MemoryControlRepository();
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
  const repository = new MemoryControlRepository();
  const session = {
    sessionIdHash: hash("9"),
    devicePublicKeyHash: hash("8"),
    codeChallenge: "challenge-expired",
    callbackUri: "http://127.0.0.1:43123/pca/pair/callback",
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
      workspaceId: "01982222-7222-8222-8222-222222222222",
      ownerUserId: "01983333-7333-8333-8333-333333333333",
      callbackStateHash: hash("6"),
      expiresAt: later,
      now,
    }),
    (error) =>
      error instanceof ControlRepositoryError && error.code === "PAIRING_EXPIRED",
  );
});

test("wrong PKCE challenge does not consume the authorization code", async () => {
  const repository = new MemoryControlRepository();
  await repository.createPairingSession({
    sessionIdHash: hash("a"),
    devicePublicKeyHash: hash("b"),
    codeChallenge: "expected-challenge",
    callbackUri: "http://127.0.0.1:43123/pca/pair/callback",
    expiresAt: later,
    createdAt: now,
  });
  await repository.authorizePairingSession({
    sessionIdHash: hash("a"),
    authorizationCodeHash: hash("c"),
    workspaceId: "01982222-7222-8222-8222-222222222222",
    ownerUserId: "01983333-7333-8333-8333-333333333333",
    callbackStateHash: hash("d"),
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
  const repository = new MemoryControlRepository();
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
    actorUserId: "01983333-7333-8333-8333-333333333333",
    workspaceId: "01982222-7222-8222-8222-222222222222",
    deviceId: "01981111-7111-8111-8111-111111111111",
    config: { networkEnabled: true, wechatEnabled: false },
    now,
  });
  assert.equal(revision, 1);
  const snapshot = await repository.loadControlSnapshot(
    "01981111-7111-8111-8111-111111111111",
    "01982222-7222-8222-8222-222222222222",
  );
  assert.equal(snapshot.configuration_revision, 1);
  assert.deepEqual(snapshot.collectors.network, { enabled: true });
  assert.deepEqual(snapshot.collectors["communication.wechat"], {
    enabled: false,
    direction: "outgoing",
    message_type: "text",
    sync_mode: "full",
  });
});

test("credential rotation revokes the prior refresh hash", async () => {
  const repository = new MemoryControlRepository();
  await pairDevice(repository);
  const rotation = {
    workspaceId: "01982222-7222-8222-8222-222222222222",
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
  const repository = new MemoryControlRepository();
  await pairDevice(repository);
  const heartbeat = {
    heartbeatId: "01986666-7666-8666-8666-666666666666",
    workspaceId: "01982222-7222-8222-8222-222222222222",
    deviceId: "01981111-7111-8111-8111-111111111111",
    receivedAt: now,
    agentVersion: "0.3.0",
    presence: "online" as const,
    outboxDepth: 0,
  };
  await repository.recordHeartbeat(heartbeat);
  await assert.rejects(
    repository.recordHeartbeat(heartbeat),
    (error) => error instanceof ControlRepositoryError && error.code === "CONFLICT",
  );
});
