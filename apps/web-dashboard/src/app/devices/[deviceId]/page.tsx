"use client";

import Link from "next/link";
import { useParams } from "next/navigation";
import { useEffect, useState, type ReactNode } from "react";

import {
  DashboardApiError,
  cloudApiOrigin,
  createNetworkLocation,
  deleteNetworkLocation,
  getCollectorAudit,
  getDevice,
  getNetworkLocations,
  getSystemMetrics,
  purgeDevice,
  revokeDevice,
  requestLocalMediaCleanup,
  requestScreenshot,
  updateCollectorConfig,
  type CollectorConfig,
  type CollectorConfigAudit,
  type DashboardCollectorHealth,
  type DashboardDevice,
  type DashboardNetworkLocation,
  type DashboardSystemMetric,
} from "../../../lib/api";
import { getBrowserSession, redirectToSignIn } from "../../../lib/auth";
import { DashboardShell } from "../../../components/dashboard-shell";
import { summarizeSystemMetrics } from "../../../lib/system-metrics";
import { collectorHealthPresentation } from "../../../lib/collector-health";

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
  const [screenDraft, setScreenDraft] = useState<CollectorConfig["screen.capture"] | null>(null);
  const [captureQueued, setCaptureQueued] = useState(false);

  async function refresh(): Promise<void> {
    const origin = cloudApiOrigin();
    const [device, audit, metrics, locations] = await Promise.all([
      getDevice(window.fetch, origin, deviceId),
      getCollectorAudit(window.fetch, origin, deviceId),
      getSystemMetrics(window.fetch, origin, deviceId),
      getNetworkLocations(window.fetch, origin),
    ]);
    setScreen({ device, audit, metrics, locations });
    setScreenDraft(device.collectors["screen.capture"]);
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

  async function purge(): Promise<void> {
    if (window.prompt(`Type the device ID to permanently delete this device and all Cloud media:\n${deviceId}`) !== deviceId) return;
    setBusy(true);
    setError(null);
    try {
      await purgeDevice(window.fetch, cloudApiOrigin(), deviceId);
      window.location.assign("/");
    } catch (cause) {
      setError(messageFor(cause));
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

  async function captureNow(): Promise<void> {
    setBusy(true);
    setError(null);
    try {
      await requestScreenshot(window.fetch, cloudApiOrigin(), deviceId);
      setCaptureQueued(true);
      window.setTimeout(() => setCaptureQueued(false), 35_000);
    } catch (cause) {
      setError(messageFor(cause));
    } finally {
      setBusy(false);
    }
  }

  async function saveScreenSettings(): Promise<void> {
    if (screenDraft === null || screen === null) return;
    await save({ ...screen.device.collectors, "screen.capture": screenDraft });
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
        country: null,
        region: null,
        city: null,
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
  const screenSettingsValid = screenDraft !== null
    && Number.isInteger(screenDraft.interval_seconds)
    && screenDraft.interval_seconds >= 60
    && screenDraft.interval_seconds <= 86400
    && Number.isInteger(screenDraft.activity_min_interval_seconds)
    && screenDraft.activity_min_interval_seconds >= 10
    && screenDraft.activity_min_interval_seconds <= 3600
    && screenDraft.excluded_bundle_ids.every((value) => /^[A-Za-z0-9.-]{1,255}$/.test(value));
  const metrics = summarizeSystemMetrics(screen.metrics);
  return (
    <DashboardShell>
      <Link className="back-link" href="/">Back to devices</Link>
      <section className="page-heading">
        <p className="workspace-name">Configuration revision {screen.device.configuration_revision}</p>
        <h1>Device</h1>
        <p>Review collection permissions and device access.</p>
      </section>
      <div className="device-links">
        <Link className="primary-link" href={`/devices/${encodeURIComponent(deviceId)}/chats`}>View WeChat</Link>
        <Link className="primary-link" href={`/devices/${encodeURIComponent(deviceId)}/messages`}>View Messages</Link>
        <Link className="primary-link" href={`/devices/${encodeURIComponent(deviceId)}/screenshots`}>View screenshots</Link>
        <Link className="primary-link" href={`/devices/${encodeURIComponent(deviceId)}/photos`}>View photos</Link>
      </div>
      {error !== null ? <p role="alert">{error}</p> : null}
      <CollectorHealthPanel
        health={screen.device.status?.collector_health ?? []}
        presence={screen.device.status?.presence}
        lastSuccessfulCheckIn={screen.device.status?.observed_at}
      />
      <CollectorScopeCard
          name="Network"
          detail="SSID, BSSID, local IP and precise device location"
          enabled={screen.device.collectors.network.enabled}
          disabled={disabled}
          onToggle={() => void save({
            ...screen.device.collectors,
            network: { enabled: !screen.device.collectors.network.enabled },
          })}
        />
        <CollectorScopeCard
          name="WeChat messages"
          detail="Incoming and outgoing text, audio, images and video; direct chats and groups up to 15 members; permanent Cloud retention"
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
        <CollectorScopeCard
          name="Messages"
          detail="All iMessage and SMS conversations; text only; initial 7-day history and future messages"
          enabled={screen.device.collectors["communication.messages"].enabled}
          disabled={disabled}
          onToggle={() => void save({
            ...screen.device.collectors,
            "communication.messages": {
              ...screen.device.collectors["communication.messages"],
              enabled: !screen.device.collectors["communication.messages"].enabled,
            },
          })}
        />
        <CollectorScopeCard
          name="Photos"
          detail="Original photos and videos with capture time and album names; initial 60-day history and future items; permanent Cloud retention"
          enabled={screen.device.collectors["photos.library"].enabled}
          disabled={disabled}
          onToggle={() => void save({
            ...screen.device.collectors,
            "photos.library": {
              ...screen.device.collectors["photos.library"],
              enabled: !screen.device.collectors["photos.library"].enabled,
            },
          })}
        />
        <section className="dashboard-panel collector-card" aria-labelledby="screenshots-heading">
          <h2 id="screenshots-heading">Screenshots</h2>
          <p>Capture the active display only. Locked/login screens and excluded applications are skipped.</p>
          {screenDraft === null ? null : (
            <div className="settings-grid">
              <label className="checkbox-label">
                <input
                  type="checkbox"
                  checked={screenDraft.enabled}
                  onChange={(event) => setScreenDraft({ ...screenDraft, enabled: event.target.checked })}
                />
                Enable screenshot collection
              </label>
              <label className="checkbox-label">
                <input
                  type="checkbox"
                  checked={screenDraft.scheduled_enabled}
                  onChange={(event) => setScreenDraft({ ...screenDraft, scheduled_enabled: event.target.checked })}
                />
                Scheduled screenshots
              </label>
              <label>
                Schedule interval (minutes)
                <input
                  type="number"
                  min={1}
                  max={1440}
                  value={screenDraft.interval_seconds / 60}
                  onChange={(event) => setScreenDraft({
                    ...screenDraft,
                    interval_seconds: Math.round(Number(event.target.value) * 60),
                  })}
                />
              </label>
              <label className="checkbox-label">
                <input
                  type="checkbox"
                  checked={screenDraft.activity_enabled}
                  onChange={(event) => setScreenDraft({ ...screenDraft, activity_enabled: event.target.checked })}
                />
                Activity-triggered screenshots
              </label>
              <label>
                Activity minimum interval (seconds)
                <input
                  type="number"
                  min={10}
                  max={3600}
                  value={screenDraft.activity_min_interval_seconds}
                  onChange={(event) => setScreenDraft({
                    ...screenDraft,
                    activity_min_interval_seconds: Math.round(Number(event.target.value)),
                  })}
                />
              </label>
              <label>
                Excluded app Bundle IDs (one per line)
                <textarea
                  rows={4}
                  value={screenDraft.excluded_bundle_ids.join("\n")}
                  placeholder="com.1password.1password"
                  onChange={(event) => setScreenDraft({
                    ...screenDraft,
                    excluded_bundle_ids: [...new Set(event.target.value.split("\n").map((value) => value.trim()).filter(Boolean))],
                  })}
                />
              </label>
              <div className="button-row">
                <button className="primary-button" type="button" disabled={disabled || !screenSettingsValid} onClick={() => void saveScreenSettings()}>
                  Save screenshot settings
                </button>
                <button
                  className="primary-button"
                  type="button"
                  disabled={disabled || captureQueued || !screenDraft.enabled || screen.device.status?.presence !== "online"}
                  onClick={() => void captureNow()}
                >
                  {captureQueued ? "Capture queued" : "Capture now"}
                </button>
              </div>
            </div>
          )}
          <p>Screen Recording permission is granted once during installation and remains tied to the signed helper.</p>
        </section>
        <section className="dashboard-panel collector-card" aria-labelledby="network-status-heading">
          <h2 id="network-status-heading">Network and location</h2>
          {screen.device.status?.network === null || screen.device.status?.network === undefined ? (
            <p>Waiting for an enabled Network Collector observation.</p>
          ) : (
            <>
              <h3>Current network and location</h3>
              <NetworkObservationDetails network={screen.device.status.network} />
              <p>Last observed {new Date(screen.device.status.observed_at).toLocaleString()}</p>
              <h3 id="network-history">Recent network and location changes</h3>
              {hasNetworkChanges(screen.device.status.network_history ?? []) ? (
                <ol>
                  {(screen.device.status.network_history ?? []).map((record) => (
                    <li key={`${record.observed_at}:${record.interface_type}:${record.bssid ?? record.observed_exit_ip ?? "none"}`}>
                      <p>Recorded {new Date(record.observed_at).toLocaleString()}</p>
                      <NetworkObservationDetails network={record} />
                    </li>
                  ))}
                </ol>
              ) : (
                <p>No network or location changes recorded yet.</p>
              )}
            </>
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
          <button className="danger-button" type="button" disabled={busy} onClick={() => void purge()}>
            Permanently delete device
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

const collectorHealthDefinitions = [
  ["system", "System metrics"],
  ["network", "Network"],
  ["communication.wechat", "WeChat messages"],
  ["communication.messages", "Messages"],
  ["photos.library", "Photos"],
  ["screen.capture", "Screenshots"],
] as const;

function CollectorHealthPanel({
  health,
  presence,
  lastSuccessfulCheckIn,
}: {
  health: DashboardCollectorHealth[];
  presence: "online" | "stale" | "offline" | "sleeping" | undefined;
  lastSuccessfulCheckIn: string | undefined;
}) {
  return (
    <section className="dashboard-panel collector-card" aria-labelledby="collector-health-heading">
      <h2 id="collector-health-heading">Collector health</h2>
      <p>The Agent checks and reports collector health every 30 minutes while it is running.</p>
      <p>
        Device connectivity: <strong>{presence === undefined ? "No successful check-in" : statusLabel(presence)}</strong>
        {lastSuccessfulCheckIn === undefined ? null : ` · Last successful check-in ${formatHealthTime(lastSuccessfulCheckIn)}`}
      </p>
      <div className="collector-health-list">
        {collectorHealthDefinitions.map(([key, name]) => {
          const record = health.find((candidate) => candidate.collector_key === key);
          const presentation = collectorHealthPresentation(record, Date.now(), presence);
          return (
            <article className={`collector-health-row${presentation.alert ? " is-alert" : ""}`} key={key}>
              <div>
                <h3>{name}</h3>
                <p className="collector-health-state">Actual status: <strong>{presentation.label}</strong></p>
                {presentation.reason === null ? null : <p role="alert">Failure: {presentation.reason}</p>}
              </div>
              <dl>
                <div><dt>Last successful check</dt><dd>{formatHealthTime(record?.last_health_at)}</dd></div>
                <div><dt>Last collected event</dt><dd>{formatHealthTime(record?.last_event_at)}</dd></div>
                <div><dt>Last Agent report</dt><dd>{formatHealthTime(record?.reported_at)}</dd></div>
              </dl>
            </article>
          );
        })}
      </div>
    </section>
  );
}

function statusLabel(status: string): string {
  return status.split("_").map((part) => part[0]?.toUpperCase() + part.slice(1)).join(" ");
}

function formatHealthTime(value: string | null | undefined): string {
  return value === null || value === undefined ? "Never" : new Date(value).toLocaleString();
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

function NetworkObservationDetails({
  network,
}: {
  network: NonNullable<NonNullable<DashboardDevice["status"]>["network"]>;
}) {
  return (
    <dl className="system-metrics">
      <div><dt>Interface</dt><dd>{network.interface_type}</dd></div>
      <div><dt>Wi-Fi / SSID</dt><dd>{network.ssid ?? "Unavailable (Location permission required)"}</dd></div>
      <div><dt>BSSID</dt><dd>{network.bssid ?? "Unavailable"}</dd></div>
      <div><dt>Local IP</dt><dd>{network.local_ipv4 ?? network.local_ipv6 ?? "Unavailable"}</dd></div>
      <div><dt>Observed exit IP</dt><dd>{network.observed_exit_ip ?? "Unavailable"}</dd></div>
      <div><dt>Exit IP estimate</dt><dd>{exitIpLocationLabel(network)}</dd></div>
      <div><dt>Device location</dt><dd>{deviceLocationLabel(network)}</dd></div>
      <div><dt>Saved location match</dt><dd>{networkLocationLabel(network)}</dd></div>
    </dl>
  );
}

function hasNetworkChanges(
  history: Array<NonNullable<NonNullable<DashboardDevice["status"]>["network"]> & { observed_at: string }>,
): boolean {
  return history.length > 1;
}

function networkLocationLabel(network: NonNullable<NonNullable<DashboardDevice["status"]>["network"]>): string {
  if (network.matched_location !== null) return network.matched_location.name;
  return "No saved match";
}

function exitIpLocationLabel(network: NonNullable<NonNullable<DashboardDevice["status"]>["network"]>): string {
  const parts = [network.exit_ip_location?.city, network.exit_ip_location?.region, network.exit_ip_location?.country]
    .filter((value): value is string => value !== null && value !== undefined);
  return parts.length > 0
    ? `${[...new Set(parts)].join(", ")} (low confidence; may be VPN/proxy)`
    : "Unavailable";
}

function deviceLocationLabel(network: NonNullable<NonNullable<DashboardDevice["status"]>["network"]>): ReactNode {
  const location = network.device_location;
  if (location === null) return "Unavailable";
  const coordinates = `${location.latitude.toFixed(6)}, ${location.longitude.toFixed(6)}`;
  return (
    <a
      href={`https://maps.google.com/?q=${encodeURIComponent(`${location.latitude},${location.longitude}`)}`}
      target="_blank"
      rel="noreferrer"
    >
      {coordinates} (±{Math.round(location.horizontal_accuracy_meters)} m)
    </a>
  );
}
