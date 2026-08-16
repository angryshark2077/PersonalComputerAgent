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
  PHOTOS_UPLOAD_FAILED: "Photo uploads failed and will retry from the local spool.",
  SCREEN_CAPTURE_FAILED: "The screenshot collector could not capture the display.",
  SCREEN_CAPTURE_PERMISSION_REQUIRED: "Screen Recording permission is missing.",
  SCREEN_UPLOAD_FAILED: "Screenshot uploads failed and will retry from the local spool.",
  SCREEN_UPLOAD_TIMEOUT: "Screenshot uploads exceeded their bounded transfer window and will retry.",
  COMMUNICATION_SOURCE_IDENTITY_CONFLICT: "A communication source key conflicts with different immutable local message content; the conflicting record was skipped.",
  COMMUNICATION_LOCAL_DATABASE_FAILED: "The communication collector could not persist data to the local database and will retry.",
  COMMUNICATION_LOCAL_SPOOL_UNAVAILABLE: "The communication collector could not access its private local media spool and will retry.",
  COMMUNICATION_INVALID_RECORD: "The communication collector rejected an invalid local record and continued with later records.",
  COMMUNICATION_MEDIA_UPLOAD_FAILED: "Communication media uploads failed and will retry from the local spool.",
  MEDIA_CYCLE_TIMEOUT: "The media synchronization cycle exceeded its bounded window and will restart from the local spool.",
};

export function collectorHealthPresentation(
  health: DashboardCollectorHealth | undefined,
  nowMs = Date.now(),
  devicePresence?: "online" | "stale" | "offline" | "sleeping",
): { label: string; reason: string | null; alert: boolean } {
  if (health === undefined) {
    if (devicePresence === "offline" || devicePresence === "sleeping") {
      return {
        label: "Waiting for first Agent report",
        reason: `No collector health report was received before the Agent went ${devicePresence}.`,
        alert: false,
      };
    }
    return { label: "Waiting for first Agent report", reason: "No collector health report has been received.", alert: true };
  }
  if (health.error_code !== null) {
    const explanation = failureReasons[health.error_code] ?? "The collector reported a failure.";
    return { label: statusLabel(health.status), reason: `${explanation} (${health.error_code})`, alert: true };
  }
  if (nowMs - Date.parse(health.reported_at) > REPORT_STALE_AFTER_MS) {
    if (devicePresence === "offline" || devicePresence === "sleeping") {
      const unhealthy = ["permission_required", "degraded", "unsupported", "error"].includes(health.status);
      return {
        label: `Last reported: ${statusLabel(health.status)}`,
        reason: `The Agent is ${devicePresence}; this cached collector status has not been refreshed.`,
        alert: unhealthy,
      };
    }
    return {
      label: "Health report overdue",
      reason: "The connected Agent has missed two expected 30-minute collector health reports.",
      alert: true,
    };
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
