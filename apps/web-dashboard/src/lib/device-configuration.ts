import type { DashboardDevice } from "./api";

export function renderDeviceConfiguration(device: DashboardDevice): string {
  return [
    `<section aria-label="Network"><h2>Network</h2><p>SSID, BSSID, local IP and precise device location</p><p>${enabledLabel(device.collectors.network.enabled)}</p></section>`,
    `<section aria-label="Screenshots"><h2>Screenshots</h2><p>Active display only; scheduled and activity-triggered capture; lock screen and excluded applications are skipped</p><p>${enabledLabel(device.collectors["screen.capture"].enabled)}</p></section>`,
    `<section aria-label="WeChat messages"><h2>WeChat messages</h2><p>Incoming and outgoing text, audio, images, video and files; direct chats and groups up to 15 members; permanent Cloud retention</p><p>${enabledLabel(device.collectors["communication.wechat"].enabled)}</p></section>`,
    `<section aria-label="Messages"><h2>Messages</h2><p>All iMessage and SMS conversations; text only; initial 7-day history and future messages</p><p>${enabledLabel(device.collectors["communication.messages"].enabled)}</p></section>`,
    `<section aria-label="Photos"><h2>Photos</h2><p>Original photos and videos with capture time and album names; initial 60-day history and future items; permanent Cloud retention</p><p>${enabledLabel(device.collectors["photos.library"].enabled)}</p></section>`,
  ].join("");
}

function enabledLabel(enabled: boolean): string {
  return enabled ? "Enabled" : "Disabled";
}
