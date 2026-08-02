import type { DashboardDevice } from "./api";

export function renderDeviceConfiguration(device: DashboardDevice): string {
  return [
    `<section aria-label="Network"><h2>Network</h2><p>SSID, BSSID and local IP</p><p>${enabledLabel(device.collectors.network.enabled)}</p></section>`,
    `<section aria-label="WeChat messages"><h2>WeChat messages</h2><p>Incoming and outgoing text, audio, images and video; direct chats and groups up to 8 members; 180-day retention</p><p>${enabledLabel(device.collectors["communication.wechat"].enabled)}</p></section>`,
  ].join("");
}

function enabledLabel(enabled: boolean): string {
  return enabled ? "Enabled" : "Disabled";
}
