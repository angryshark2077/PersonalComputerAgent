import type { DashboardCollectorHealth } from "./api";

const REPORT_STALE_AFTER_MS = 65 * 60 * 1000;

const failureReasons: Record<string, string> = {
  NETWORK_OBSERVATION_UNAVAILABLE: "The Network collector is enabled but the Agent has no current network observation.",
  WECHAT_CAPABILITY_UNAVAILABLE: "WeChat key verification is unavailable.",
  WECHAT_KEY_REJECTED: "The stored WeChat key was rejected.",
  WECHAT_ACCOUNT_UNVERIFIED: "The stored key does not match the current WeChat account.",
  WECHAT_MULTIPLE_ACCOUNTS: "Multiple local WeChat accounts require selection.",
  WECHAT_DATABASE_UNAVAILABLE: "The WeChat database is unavailable.",
  WECHAT_PERMISSION_REQUIRED: "macOS permission to read WeChat data is missing.",
  WECHAT_STOP_FAILED: "The WeChat collector could not stop cleanly.",
  SYSTEM_SAMPLE_FAILED: "System metric sampling failed.",
  DISK_SPACE_LOW: "Local disk space is low.",
  MESSAGES_DATABASE_UNAVAILABLE: "The local Messages database is unavailable.",
  MESSAGES_COLLECTION_FAILED: "The Messages collector could not read or persist messages.",
  PHOTOS_COLLECTION_FAILED: "The Photos collector could not access or persist the photo library.",
  SCREEN_CAPTURE_FAILED: "The screenshot collector could not capture the display.",
  SCREEN_CAPTURE_PERMISSION_REQUIRED: "Screen Recording permission is missing.",
};

export function collectorHealthPresentation(
  health: DashboardCollectorHealth | undefined,
  nowMs = Date.now(),
): { label: string; reason: string | null; alert: boolean } {
  if (health === undefined) {
    return { label: "Waiting for first Agent report", reason: "No collector health report has been received.", alert: true };
  }
  if (nowMs - Date.parse(health.reported_at) > REPORT_STALE_AFTER_MS) {
    return {
      label: "Health report overdue",
      reason: "The Agent has not refreshed this collector within the expected 30-minute reporting schedule.",
      alert: true,
    };
  }
  if (health.error_code !== null) {
    const explanation = failureReasons[health.error_code] ?? "The collector reported a failure.";
    return { label: statusLabel(health.status), reason: `${explanation} (${health.error_code})`, alert: true };
  }
  const alert = ["permission_required", "degraded", "unsupported", "error"].includes(health.status);
  return {
    label: statusLabel(health.status),
    reason: alert ? "The collector is not healthy, but no error code was reported." : null,
    alert,
  };
}

function statusLabel(status: DashboardCollectorHealth["status"]): string {
  return status.split("_").map((part) => part[0]?.toUpperCase() + part.slice(1)).join(" ");
}
