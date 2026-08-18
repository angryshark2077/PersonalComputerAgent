import assert from "node:assert/strict";
import test from "node:test";

import { mergeDevice, normalizeDashboardDevice, type DashboardDevice } from "../src/lib/api.ts";
import { renderDeviceConfiguration } from "../src/lib/device-configuration.ts";

const snapshotWithWechatEnabled = () => ({
  device_id: "01985555-7555-8555-8555-555555555555",
  workspace_id: "01982222-7222-8222-2222-222222222222",
  platform: "macos" as const,
  paired_at: "2026-07-31T10:00:00.000Z",
  revoked: false,
  configuration_revision: 3,
  status: null,
  collectors: {
    network: { enabled: true },
    "screen.capture": {
      enabled: true,
      scheduled_enabled: true,
      interval_seconds: 300,
      activity_enabled: true,
      activity_min_interval_seconds: 30,
      excluded_bundle_ids: [],
    },
    "communication.wechat": {
      enabled: true,
      directions: ["incoming", "outgoing"] as ["incoming", "outgoing"],
      message_types: ["text", "audio", "image", "video"] as ["text", "audio", "image", "video"],
      conversation_scope: "direct_and_group_at_most_fifteen_members" as const,
      max_group_members: 15 as const,
      sync_mode: "full" as const,
      retention_days: 180 as const,
    },
    "communication.messages": {
      enabled: true,
      directions: ["incoming", "outgoing"] as ["incoming", "outgoing"],
      message_types: ["text"] as ["text"],
      conversation_scope: "all" as const,
      initial_lookback_days: 7 as const,
      sync_mode: "full" as const,
      attachments_enabled: false as const,
      attachment_retention_days: 7 as const,
    },
    "photos.library": {
      enabled: true,
      media_types: ["image", "video"] as ["image", "video"],
      include_originals: true as const,
      include_album_names: true as const,
      initial_lookback_days: 60 as const,
      cloud_retention: "permanent" as const,
    },
  },
});

test("device merge sends the selected active target without deleting history", async () => {
  let request: { url: string; init?: RequestInit } | null = null;
  await mergeDevice(async (url, init) => {
    request = { url: String(url), init };
    return new Response(JSON.stringify({ merged: true }), { status: 200, headers: { "content-type": "application/json" } });
  }, "https://cloud.example", "source-device", "target-device");
  assert.equal(request?.url, "https://cloud.example/v1/devices/source-device/merge");
  assert.equal(request?.init?.method, "POST");
  assert.deepEqual(JSON.parse(String(request?.init?.body)), { target_device_id: "target-device" });
});

test("device configuration exposes the approved WeChat scope", () => {
  const page = renderDeviceConfiguration(snapshotWithWechatEnabled());

  assert.match(page, /Incoming and outgoing text, audio, images, video and files/);
  assert.match(page, /groups up to 15 members/);
  assert.match(page, /permanent Cloud retention/);
});

test("device configuration shows only the approved Network collection detail", () => {
  const page = renderDeviceConfiguration(snapshotWithWechatEnabled());

  assert.match(page, /SSID, BSSID, local IP and precise device location/);
  assert.equal(page.includes("Gateway"), false);
  assert.equal(page.includes("Traffic"), false);
});

test("device configuration exposes active-display screenshot boundaries", () => {
  const page = renderDeviceConfiguration(snapshotWithWechatEnabled());

  assert.match(page, /Active display only/);
  assert.match(page, /lock screen and excluded applications are skipped/);
});

test("device page normalizes an old Cloud response without screenshot settings", () => {
  const current = snapshotWithWechatEnabled();
  const { "screen.capture": _screenCapture, ...legacyCollectors } = current.collectors;
  const legacy = { ...current, collectors: legacyCollectors } as unknown as DashboardDevice;

  const normalized = normalizeDashboardDevice(legacy);

  assert.deepEqual(normalized.collectors["screen.capture"], {
    enabled: false,
    scheduled_enabled: true,
    interval_seconds: 300,
    activity_enabled: true,
    activity_min_interval_seconds: 30,
    excluded_bundle_ids: [],
  });
});
