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
      direction: "outgoing" as const,
      message_type: "text" as const,
      sync_mode: "full" as const,
    },
  },
});

test("device configuration exposes no expanded WeChat scope", () => {
  const page = renderDeviceConfiguration(snapshotWithWechatEnabled());

  assert.match(page, /Outgoing text only/);
  assert.equal(page.includes("Incoming messages"), false);
  assert.match(page, /90-day retention/);
});

test("device configuration shows only the approved Network collection detail", () => {
  const page = renderDeviceConfiguration(snapshotWithWechatEnabled());

  assert.match(page, /SSID, BSSID and local IP/);
  assert.equal(page.includes("Gateway"), false);
  assert.equal(page.includes("Traffic"), false);
});
