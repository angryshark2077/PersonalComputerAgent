import assert from "node:assert/strict";
import test from "node:test";

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
    "communication.wechat": {
      enabled: true,
      directions: ["incoming", "outgoing"] as ["incoming", "outgoing"],
      message_types: ["text", "audio", "image", "video"] as ["text", "audio", "image", "video"],
      conversation_scope: "direct_and_group_at_most_fifteen_members" as const,
      max_group_members: 15 as const,
      sync_mode: "full" as const,
      retention_days: 180 as const,
    },
  },
});

test("device configuration exposes the approved WeChat scope", () => {
  const page = renderDeviceConfiguration(snapshotWithWechatEnabled());

  assert.match(page, /Incoming and outgoing text, audio, images, video and files/);
  assert.match(page, /groups up to 15 members/);
  assert.match(page, /180-day retention/);
});

test("device configuration shows only the approved Network collection detail", () => {
  const page = renderDeviceConfiguration(snapshotWithWechatEnabled());

  assert.match(page, /SSID, BSSID and local IP/);
  assert.equal(page.includes("Gateway"), false);
  assert.equal(page.includes("Traffic"), false);
});
