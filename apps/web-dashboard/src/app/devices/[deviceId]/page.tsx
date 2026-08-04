"use client";

import Link from "next/link";
import { useParams } from "next/navigation";
import { useEffect, useState } from "react";

import {
  DashboardApiError,
  cloudApiOrigin,
  createNetworkLocation,
  deleteNetworkLocation,
  getCollectorAudit,
  getDevice,
  getNetworkLocations,
  getSystemMetrics,
  revokeDevice,
  requestLocalMediaCleanup,
  updateCollectorConfig,
  type CollectorConfig,
  type CollectorConfigAudit,
  type DashboardDevice,
  type DashboardNetworkLocation,
  type DashboardSystemMetric,
} from "../../../lib/api";
import { getBrowserSession, redirectToSignIn } from "../../../lib/auth";
import { DashboardShell } from "../../../components/dashboard-shell";
import { summarizeSystemMetrics } from "../../../lib/system-metrics";

interface DeviceScreen {
  device: DashboardDevice;
  audit: CollectorConfigAudit[];
  metrics: DashboardSystemMetric[];
  locations: DashboardNetworkLocation[];
}

export default function DevicePage() {
  const params = useParams<{ deviceId: string }>();
  const deviceId = params.deviceId;
  const [screen, setScreen] = useState<DeviceScreen | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [locationName, setLocationName] = useState("");

  async function refresh(): Promise<void> {
    const origin = cloudApiOrigin();
    const [device, audit, metrics, locations] = await Promise.all([
      getDevice(window.fetch, origin, deviceId),
      getCollectorAudit(window.fetch, origin, deviceId),
      getSystemMetrics(window.fetch, origin, deviceId),
      getNetworkLocations(window.fetch, origin),
    ]);
    setScreen({ device, audit, metrics, locations });
  }

  useEffect(() => {
    void (async () => {
      if ((await getBrowserSession(window.fetch, cloudApiOrigin())) === null) {
        redirectToSignIn();
        return;
      }
      try {
        await refresh();
      } catch (cause) {
        setError(messageFor(cause));
      }
    })();
  }, [deviceId]);

  useEffect(() => {
    if (screen?.device.local_media_cleanup?.status !== "queued") return;
    const timer = window.setTimeout(() => {
      void refresh().catch((cause: unknown) => setError(messageFor(cause)));
    }, 3000);
    return () => window.clearTimeout(timer);
  }, [screen?.device.local_media_cleanup?.request_id, screen?.device.local_media_cleanup?.status]);

  async function save(config: CollectorConfig): Promise<void> {
    setBusy(true);
    setError(null);
    try {
      await updateCollectorConfig(window.fetch, cloudApiOrigin(), deviceId, config);
      await refresh();
    } catch (cause) {
      setError(messageFor(cause));
    } finally {
      setBusy(false);
    }
  }

  async function revoke(): Promise<void> {
    setBusy(true);
    setError(null);
    try {
      await revokeDevice(window.fetch, cloudApiOrigin(), deviceId);
      await refresh();
    } catch (cause) {
      setError(messageFor(cause));
    } finally {
      setBusy(false);
    }
  }

  async function cleanupLocalMedia(): Promise<void> {
    if (!window.confirm("Delete all local media that has completed its Cloud upload? Cloud copies and message history will remain.")) return;
    setBusy(true);
    setError(null);
    try {
      await requestLocalMediaCleanup(window.fetch, cloudApiOrigin(), deviceId);
      await refresh();
    } catch (cause) {
      setError(messageFor(cause));
    } finally {
      setBusy(false);
    }
  }

  async function saveCurrentLocation(): Promise<void> {
    const network = screen?.device.status?.network;
    const name = locationName.trim();
    if (network === null || network === undefined || name.length === 0) return;
    setBusy(true);
    setError(null);
    try {
      await createNetworkLocation(window.fetch, cloudApiOrigin(), {
        name,
        match_ssid: network.ssid,
        match_bssid: network.bssid,
        country: network.ip_location?.country ?? null,
        region: network.ip_location?.region ?? null,
        city: network.ip_location?.city ?? null,
      });
      setLocationName("");
      await refresh();
    } catch (cause) {
      setError(messageFor(cause));
    } finally {
      setBusy(false);
    }
  }

  async function removeLocation(locationId: string): Promise<void> {
    setBusy(true);
    setError(null);
    try {
      await deleteNetworkLocation(window.fetch, cloudApiOrigin(), locationId);
      await refresh();
    } catch (cause) {
      setError(messageFor(cause));
    } finally {
      setBusy(false);
    }
  }

  if (error !== null && screen === null) return <DashboardShell><p role="alert">{error}</p></DashboardShell>;
  if (screen === null) return <DashboardShell><p className="status-note">Loading device…</p></DashboardShell>;

  const disabled = busy || screen.device.revoked;
  const metrics = summarizeSystemMetrics(screen.metrics);
  return (
    <DashboardShell>
      <Link className="back-link" href="/">Back to devices</Link>
      <section className="page-heading">
        <p className="workspace-name">Configuration revision {screen.device.configuration_revision}</p>
        <h1>Device</h1>
        <p>Review collection permissions and device access.</p>
      </section>
      <Link className="primary-link" href={`/devices/${encodeURIComponent(deviceId)}/chats`}>View chats</Link>
      {error !== null ? <p role="alert">{error}</p> : null}
      <CollectorScopeCard
          name="Network"
          detail="SSID, BSSID and local IP"
          enabled={screen.device.collectors.network.enabled}
          disabled={disabled}
          onToggle={() => void save({
            ...screen.device.collectors,
            network: { enabled: !screen.device.collectors.network.enabled },
          })}
        />
        <CollectorScopeCard
          name="WeChat messages"
          detail="Incoming and outgoing text, audio, images and video; direct chats and groups up to 15 members; 180-day retention"
          enabled={screen.device.collectors["communication.wechat"].enabled}
          disabled={disabled}
          onToggle={() => void save({
            ...screen.device.collectors,
            "communication.wechat": {
              ...screen.device.collectors["communication.wechat"],
            enabled: !screen.device.collectors["communication.wechat"].enabled,
            },
          })}
        />
        <section className="dashboard-panel collector-card" aria-labelledby="network-status-heading">
          <h2 id="network-status-heading">Network and location</h2>
          {screen.device.status?.network === null || screen.device.status?.network === undefined ? (
            <p>Waiting for an enabled Network Collector observation.</p>
          ) : (
            <dl className="system-metrics">
              <div><dt>Interface</dt><dd>{screen.device.status.network.interface_type}</dd></div>
              <div><dt>Wi-Fi / SSID</dt><dd>{screen.device.status.network.ssid ?? "Unavailable (Location permission required)"}</dd></div>
              <div><dt>BSSID</dt><dd>{screen.device.status.network.bssid ?? "Unavailable"}</dd></div>
              <div><dt>Local IP</dt><dd>{screen.device.status.network.local_ipv4 ?? screen.device.status.network.local_ipv6 ?? "Unavailable"}</dd></div>
              <div><dt>Public IP</dt><dd>{screen.device.status.network.public_ip ?? "Unavailable"}</dd></div>
              <div><dt>Matched location</dt><dd>{networkLocationLabel(screen.device.status.network)}</dd></div>
            </dl>
          )}
          <label>
            Location name
            <input
              value={locationName}
              maxLength={100}
              placeholder="Home or Office"
              onChange={(event) => setLocationName(event.target.value)}
            />
          </label>
          <button
            className="primary-button"
            type="button"
            disabled={disabled
              || locationName.trim().length === 0
              || (screen.device.status?.network?.ssid === null && screen.device.status?.network?.bssid === null)}
            onClick={() => void saveCurrentLocation()}
          >
            Save current network to location library
          </button>
          {screen.locations.length === 0 ? <p>No saved locations.</p> : (
            <ul>
              {screen.locations.map((location) => (
                <li key={location.location_id}>
                  {location.name} — {location.match_ssid ?? location.match_bssid}
                  <button type="button" disabled={disabled} onClick={() => void removeLocation(location.location_id)}>Delete</button>
                </li>
              ))}
            </ul>
          )}
        </section>
        <div className="device-actions">
          <button className="danger-button" type="button" disabled={disabled} onClick={() => void revoke()}>
            {screen.device.revoked ? "Device revoked" : "Revoke device"}
          </button>
        </div>
        <section className="dashboard-panel collector-card" aria-labelledby="metrics-heading">
          <h2 id="metrics-heading">System metrics</h2>
          {metrics.cpu === null && metrics.memory === null && metrics.disk === null ? (
            <p>Waiting for the first system sample.</p>
          ) : (
            <dl className="system-metrics">
              <div><dt>CPU</dt><dd>{metrics.cpu ?? "Unavailable"}</dd></div>
              <div><dt>Memory</dt><dd>{metrics.memory ?? "Unavailable"}</dd></div>
              <div><dt>Disk</dt><dd>{metrics.disk ?? "Unavailable"}</dd></div>
            </dl>
          )}
        </section>
        <section className="dashboard-panel collector-card" aria-labelledby="local-media-heading">
          <h2 id="local-media-heading">Local media storage</h2>
          {screen.device.status === null ? <p>Waiting for the next Agent heartbeat.</p> : (
            <dl className="system-metrics">
              <div>
                <dt>Disk available</dt>
                <dd>{metrics.disk ?? "Unavailable"}</dd>
              </div>
              <div>
                <dt>Completed media</dt>
                <dd>{formatBytes(screen.device.status.local_media.completed_bytes)} across {screen.device.status.local_media.completed_file_count} files</dd>
              </div>
              <div>
                <dt>Protected media</dt>
                <dd>{formatBytes(screen.device.status.local_media.protected_bytes)} across {screen.device.status.local_media.protected_file_count} pending or failed files</dd>
              </div>
            </dl>
          )}
          <p>{cleanupSummary(screen.device.local_media_cleanup)}</p>
          <button
            className="danger-button"
            type="button"
            disabled={disabled
              || screen.device.status === null
              || screen.device.status.local_media.completed_file_count === 0
              || screen.device.local_media_cleanup?.status === "queued"}
            onClick={() => void cleanupLocalMedia()}
          >
            {screen.device.local_media_cleanup?.status === "queued" ? "Cleanup queued" : "Delete completed local media"}
          </button>
          <p>Cloud media and Dashboard message history are retained.</p>
        </section>
        <section className="dashboard-panel collector-card" aria-labelledby="audit-heading">
          <h2 id="audit-heading">Configuration audit</h2>
          {screen.audit.length === 0 ? <p>No configuration changes.</p> : (
            <ol>
              {screen.audit.map((entry) => (
                <li key={`${entry.configuration_revision}-${entry.created_at}`}>
                  Revision {entry.configuration_revision} by {entry.actor_user_id} at {entry.created_at}
                </li>
              ))}
            </ol>
          )}
        </section>
    </DashboardShell>
  );
}

function CollectorScopeCard({
  name,
  detail,
  enabled,
  disabled,
  onToggle,
}: {
  name: string;
  detail: string;
  enabled: boolean;
  disabled: boolean;
  onToggle: () => void;
}) {
  return (
    <section className="dashboard-panel collector-card" aria-label={name}>
      <h2>{name}</h2>
      <p>{detail}</p>
      <p className="collector-status">Current status: <strong>{enabled ? "Enabled" : "Disabled"}</strong></p>
      <button className="primary-button" type="button" aria-pressed={enabled} disabled={disabled} onClick={onToggle}>
        {enabled ? `Disable ${name}` : `Enable ${name}`}
      </button>
    </section>
  );
}

function messageFor(cause: unknown): string {
  return cause instanceof DashboardApiError ? cause.message : "Unable to update device configuration.";
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KiB", "MiB", "GiB", "TiB"];
  let value = bytes / 1024;
  let unit = units[0] ?? "KiB";
  for (const candidate of units.slice(1)) {
    if (value < 1024) break;
    value /= 1024;
    unit = candidate;
  }
  return `${value.toFixed(1)} ${unit}`;
}

function cleanupSummary(cleanup: DashboardDevice["local_media_cleanup"]): string {
  if (cleanup === null) return "No manual cleanup has been requested.";
  if (cleanup.status === "queued") return `Cleanup requested at ${new Date(cleanup.requested_at).toLocaleString()}.`;
  if (cleanup.status === "failed") return `Cleanup failed: ${cleanup.error_code ?? "unknown error"}.`;
  return `Last cleanup removed ${cleanup.deleted_file_count ?? 0} files and freed ${formatBytes(cleanup.freed_bytes ?? 0)}.`;
}

function networkLocationLabel(network: NonNullable<NonNullable<DashboardDevice["status"]>["network"]>): string {
  if (network.matched_location !== null) return network.matched_location.name;
  const parts = [network.ip_location?.city, network.ip_location?.region, network.ip_location?.country]
    .filter((value): value is string => value !== null && value !== undefined);
  return parts.length > 0 ? `${parts.join(", ")} (IP estimate)` : "Unavailable";
}
