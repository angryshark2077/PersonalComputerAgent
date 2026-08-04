import assert from "node:assert/strict";
import test from "node:test";

import { MemoryControlRepository } from "@pca/db-cloud/src/repository.js";
import { validateContract } from "@pca/contracts/src/validate.js";

import { createApp, type OwnerPrincipal } from "../index.js";
import { pkceChallenge } from "../pairing.js";

const owner: OwnerPrincipal = {
  userId: "01983333-7333-8333-8333-333333333333",
  workspaceId: "01982222-7222-8222-8222-222222222222",
};

async function pairedApi() {
  const repository = new MemoryControlRepository([
    { workspaceId: owner.workspaceId, userId: owner.userId },
  ]);
  const api = createApp({ repository, ownerAuthenticator: async () => owner });
  const start = await api.request("/v1/device-pairing/sessions", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      device_public_key: "device-public-key-control",
      code_challenge: pkceChallenge("verifier-control"),
      callback_uri: "http://127.0.0.1:43123/pca/pair/callback",
      callback_state: "1234567890123456789012345678901234567890123",
    }),
  });
  const { session_id: sessionId } = (await start.json()) as { session_id: string };
  const authorized = await api.request(
    `/v1/device-pairing/sessions/${sessionId}/authorize`,
    {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        callback_state: "1234567890123456789012345678901234567890123",
      }),
    },
  );
  const code = new URL(authorized.headers.get("location") ?? "").searchParams.get("code");
  const exchange = await api.request("/v1/device-pairing/exchange", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      session_id: sessionId,
      authorization_code: code,
      code_verifier: "verifier-control",
    }),
  });
  const credentials = (await exchange.json()) as {
    device_id: string;
    device_access_token: string;
    refresh_token: string;
  };
  return { api, credentials, repository };
}

test("owner config is scoped, strict, and reaches device control", async () => {
  const { api, credentials } = await pairedApi();
  const config = await api.request(
    `/v1/devices/${credentials.device_id}/collector-config`,
    {
      method: "PUT",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        network: { enabled: true },
        "communication.wechat": {
          enabled: true,
          directions: ["incoming", "outgoing"],
          message_types: ["text", "audio", "image", "video"],
          conversation_scope: "direct_and_group_at_most_fifteen_members",
          max_group_members: 15,
          sync_mode: "full",
          retention_days: 180,
        },
      }),
    },
  );
  assert.equal(config.status, 200);
  assert.deepEqual(await config.json(), { configuration_revision: 1 });

  const control = await api.request("/v1/agent/control", {
    method: "POST",
    headers: {
      authorization: `Bearer ${credentials.device_access_token}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({
      heartbeat_id: "01985555-7555-8555-8555-555555555555",
      agent_version: "0.1.0",
      presence: "online",
      outbox_depth: 0,
      local_media: {
        completed_file_count: 2,
        completed_bytes: 1024,
        protected_file_count: 1,
        protected_bytes: 512,
      },
      cleanup_result: null,
    }),
  });
  assert.equal(control.status, 200);
  const result = (await control.json()) as {
    snapshot: { configuration_revision: number; collectors: { network: { enabled: boolean } } };
    server_time: string;
  };
  assert.equal(result.snapshot.configuration_revision, 1);
  assert.equal(result.snapshot.collectors.network.enabled, true);
  assert.notEqual(Number.isNaN(Date.parse(result.server_time)), true);

  const badScope = await api.request(
    `/v1/devices/${credentials.device_id}/collector-config`,
    {
      method: "PUT",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ network: { enabled: true }, extra: true }),
    },
  );
  assert.equal(badScope.status, 400);
});

test("refresh rotates credentials and a revoked device is rejected", async () => {
  const { api, credentials } = await pairedApi();
  const refresh = await api.request("/v1/devices/token/refresh", {
    method: "POST",
    headers: { authorization: `Bearer ${credentials.refresh_token}` },
  });
  assert.equal(refresh.status, 200);
  const rotated = (await refresh.json()) as {
    device_access_token: string;
    refresh_token: string;
  };
  const replay = await api.request("/v1/devices/token/refresh", {
    method: "POST",
    headers: { authorization: `Bearer ${credentials.refresh_token}` },
  });
  assert.equal(replay.status, 401);

  const revoked = await api.request(`/v1/devices/${credentials.device_id}/revoke`, {
    method: "POST",
  });
  assert.equal(revoked.status, 204);
  const control = await api.request("/v1/agent/control", {
    method: "POST",
    headers: {
      authorization: `Bearer ${rotated.device_access_token}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({
      heartbeat_id: "01985555-7555-8555-8555-555555555555",
      agent_version: "0.1.0",
      presence: "online",
      outbox_depth: 0,
      local_media: {
        completed_file_count: 0,
        completed_bytes: 0,
        protected_file_count: 0,
        protected_bytes: 0,
      },
      cleanup_result: null,
      network: null,
    }),
  });
  assert.equal(control.status, 401);
});

test("Owner queues one completed-media cleanup and the Agent acknowledges it", async () => {
  const { api, credentials } = await pairedApi();
  const queued = await api.request(
    `/v1/devices/${credentials.device_id}/communication/local-media/cleanup`,
    { method: "POST" },
  );
  assert.equal(queued.status, 202);
  const queuedBody = await queued.json() as { cleanup: { request_id: string; status: string } };
  assert.equal(queuedBody.cleanup.status, "queued");

  const duplicate = await api.request(
    `/v1/devices/${credentials.device_id}/communication/local-media/cleanup`,
    { method: "POST" },
  );
  assert.equal(duplicate.status, 409);

  const dispatch = await api.request("/v1/agent/control", {
    method: "POST",
    headers: {
      authorization: `Bearer ${credentials.device_access_token}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({
      heartbeat_id: "01985555-7555-8555-8555-555555555557",
      agent_version: "0.1.70",
      presence: "online",
      outbox_depth: 0,
      local_media: {
        completed_file_count: 4,
        completed_bytes: 4096,
        protected_file_count: 1,
        protected_bytes: 256,
      },
      cleanup_result: null,
      network: null,
    }),
  });
  assert.equal(dispatch.status, 200);
  const dispatchBody = await dispatch.json() as { snapshot: { local_media_cleanup: { request_id: string } | null } };
  assert.equal(dispatchBody.snapshot.local_media_cleanup?.request_id, queuedBody.cleanup.request_id);

  const acknowledgement = await api.request("/v1/agent/control", {
    method: "POST",
    headers: {
      authorization: `Bearer ${credentials.device_access_token}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({
      heartbeat_id: "01985555-7555-8555-8555-555555555558",
      agent_version: "0.1.70",
      presence: "online",
      outbox_depth: 0,
      local_media: {
        completed_file_count: 0,
        completed_bytes: 0,
        protected_file_count: 1,
        protected_bytes: 256,
      },
      cleanup_result: {
        request_id: queuedBody.cleanup.request_id,
        status: "succeeded",
        deleted_file_count: 4,
        freed_bytes: 4096,
        error_code: null,
      },
      network: null,
    }),
  });
  assert.equal(acknowledgement.status, 200);
  const acknowledgementBody = await acknowledgement.json() as { snapshot: { local_media_cleanup: null } };
  assert.equal(acknowledgementBody.snapshot.local_media_cleanup, null);

  const detail = await api.request(`/v1/devices/${credentials.device_id}`);
  const detailBody = await detail.json() as {
    status: { local_media: { completed_bytes: number; protected_bytes: number } };
    local_media_cleanup: { status: string; freed_bytes: number };
  };
  assert.equal(detailBody.status.local_media.completed_bytes, 0);
  assert.equal(detailBody.status.local_media.protected_bytes, 256);
  assert.equal(detailBody.local_media_cleanup.status, "succeeded");
  assert.equal(detailBody.local_media_cleanup.freed_bytes, 4096);
});

test("network heartbeat is IP-enriched and matched against the Owner location library", async () => {
  const paired = await pairedApi();
  const api = createApp({
    repository: paired.repository,
    ownerAuthenticator: async () => owner,
    clientAddress: () => "203.0.113.25",
    geoEnricher: {
      locate: async (publicIp) => {
        assert.equal(publicIp, "203.0.113.25");
        return { country: "SG", region: "Singapore", city: "Singapore", accuracy: "ip_city" };
      },
    },
  });
  const created = await api.request("/v1/network-locations", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      name: "Home",
      match_ssid: "Jacob WiFi",
      match_bssid: "aa:bb:cc:dd:ee:ff",
      country: "SG",
      region: "Singapore",
      city: "Singapore",
    }),
  });
  assert.equal(created.status, 201);

  const heartbeat = await api.request("/v1/agent/control", {
    method: "POST",
    headers: {
      authorization: `Bearer ${paired.credentials.device_access_token}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({
      heartbeat_id: "01985555-7555-8555-8555-555555555559",
      agent_version: "0.1.70",
      presence: "online",
      outbox_depth: 0,
      local_media: {
        completed_file_count: 0,
        completed_bytes: 0,
        protected_file_count: 0,
        protected_bytes: 0,
      },
      cleanup_result: null,
      network: {
        interface_type: "wifi",
        wifi_identity_available: true,
        ssid: "Jacob WiFi",
        bssid: "AA:BB:CC:DD:EE:FF",
        local_ipv4: "192.168.71.120",
        local_ipv6: null,
      },
    }),
  });
  assert.equal(heartbeat.status, 200);

  const detail = await api.request(`/v1/devices/${paired.credentials.device_id}`);
  const body = await detail.json() as {
    status: { network: { public_ip: string; matched_location: { name: string }; ip_location: { city: string } } };
  };
  assert.equal(body.status.network.public_ip, "203.0.113.25");
  assert.equal(body.status.network.matched_location.name, "Home");
  assert.equal(body.status.network.ip_location.city, "Singapore");
});

test("Owner reads only its device control state and configuration audit", async () => {
  const { api, credentials, repository } = await pairedApi();
  const config = {
    network: { enabled: true },
    "communication.wechat": {
      enabled: true,
      directions: ["incoming", "outgoing"],
      message_types: ["text", "audio", "image", "video"],
      conversation_scope: "direct_and_group_at_most_fifteen_members",
      max_group_members: 15,
      sync_mode: "full",
      retention_days: 180,
    },
  };
  assert.equal(
    (
      await api.request(`/v1/devices/${credentials.device_id}/collector-config`, {
        method: "PUT",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(config),
      })
    ).status,
    200,
  );
  assert.equal(
    (
      await api.request("/v1/agent/control", {
        method: "POST",
        headers: {
          authorization: `Bearer ${credentials.device_access_token}`,
          "content-type": "application/json",
        },
        body: JSON.stringify({
          heartbeat_id: "01985555-7555-8555-8555-555555555556",
          agent_version: "0.1.0",
          presence: "online",
          outbox_depth: 2,
          local_media: {
            completed_file_count: 3,
            completed_bytes: 2048,
            protected_file_count: 0,
            protected_bytes: 0,
          },
          cleanup_result: null,
          network: null,
        }),
      })
    ).status,
    200,
  );

  const workspaces = await api.request("/v1/workspaces");
  assert.equal(workspaces.status, 200);
  const workspaceBody = await workspaces.json();
  assert.equal(validateContract("dashboard-control", workspaceBody).valid, true);
  assert.deepEqual(workspaceBody, {
    workspaces: [{ workspace_id: owner.workspaceId, name: "Personal Computer Agent" }],
  });

  const devices = await api.request("/v1/devices");
  assert.equal(devices.status, 200);
  const deviceListBody = await devices.json();
  assert.equal(validateContract("dashboard-control", deviceListBody).valid, true);
  const listed = deviceListBody as {
    devices: Array<{ device_id: string; status: { presence: string } | null }>;
  };
  assert.deepEqual(listed.devices.map((device) => device.device_id), [credentials.device_id]);
  assert.equal(listed.devices[0]?.status?.presence, "online");

  const detail = await api.request(`/v1/devices/${credentials.device_id}`);
  assert.equal(detail.status, 200);
  const deviceBody = await detail.json();
  assert.equal(validateContract("dashboard-control", deviceBody).valid, true);
  const snapshot = deviceBody as {
    collectors: { network: { enabled: boolean } };
    configuration_revision: number;
    status: { outbox_depth: number } | null;
  };
  assert.equal(snapshot.configuration_revision, 1);
  assert.equal(snapshot.collectors.network.enabled, true);
  assert.equal(snapshot.status?.outbox_depth, 2);
  assert.equal(JSON.stringify(snapshot).includes("token"), false);

  const audit = await api.request(`/v1/devices/${credentials.device_id}/collector-config/audit`);
  assert.equal(audit.status, 200);
  const auditBody = await audit.json();
  assert.equal(validateContract("dashboard-control", auditBody).valid, true);
  assert.deepEqual(auditBody, {
    audit: [
      {
        actor_user_id: owner.userId,
        configuration_revision: 1,
        old_config: { network: { enabled: false }, "communication.wechat": { ...config["communication.wechat"], enabled: false } },
        new_config: config,
        created_at: (await repository.listCollectorConfigAudit(credentials.device_id, owner.workspaceId, owner.userId))[0]?.createdAt.toISOString(),
      },
    ],
  });

  const otherWorkspace = createApp({
    repository,
    ownerAuthenticator: async () => ({
      userId: "01987777-7777-8777-8777-777777777777",
      workspaceId: "01989999-7999-8999-8999-999999999999",
    }),
  });
  assert.equal((await otherWorkspace.request(`/v1/devices/${credentials.device_id}`)).status, 403);
});

test("owner endpoints cannot cross Workspace boundaries", async () => {
  const { api, credentials, repository } = await pairedApi();
  const otherWorkspace = createApp({
    repository,
    ownerAuthenticator: async () => ({
      userId: "01987777-7777-8777-8777-777777777777",
      workspaceId: "01989999-7999-8999-8999-999999999999",
    }),
  });
  const result = await otherWorkspace.request(
    `/v1/devices/${credentials.device_id}/collector-config`,
    {
      method: "PUT",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        network: { enabled: true },
        "communication.wechat": {
          enabled: false,
          directions: ["incoming", "outgoing"],
          message_types: ["text", "audio", "image", "video"],
          conversation_scope: "direct_and_group_at_most_fifteen_members",
          max_group_members: 15,
          sync_mode: "full",
          retention_days: 180,
        },
      }),
    },
  );
  assert.equal(result.status, 403);
  assert.equal((await result.json() as { error: { error_code: string } }).error.error_code, "WORKSPACE_FORBIDDEN");
  assert.notEqual(api, otherWorkspace);
});
