import type { DashboardDevice } from "./api";

export function renderDeviceConfiguration(device: DashboardDevice): string {
  return [
    `<section aria-label="Network"><h2>Network</h2><p>SSID, BSSID and local IP</p><p>${enabledLabel(device.collectors.network.enabled)}</p></section>`,
    `<section aria-label="WeChat outbound text"><h2>WeChat outbound text</h2><p>Outgoing text only; 90-day retention</p><p>${enabledLabel(device.collectors["communication.wechat"].enabled)}</p></section>`,
  ].join("");
}

function enabledLabel(enabled: boolean): string {
  return enabled ? "Enabled" : "Disabled";
}
