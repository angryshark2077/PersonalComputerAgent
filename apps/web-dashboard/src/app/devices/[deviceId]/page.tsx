"use client";

import { useParams } from "next/navigation";
import { useEffect, useState } from "react";

import {
  DashboardApiError,
  cloudApiOrigin,
  getCollectorAudit,
  getDevice,
  revokeDevice,
  updateCollectorConfig,
  type CollectorConfig,
  type CollectorConfigAudit,
  type DashboardDevice,
} from "../../../lib/api";
import { getBrowserSession, redirectToSignIn } from "../../../lib/auth";

interface DeviceScreen {
  device: DashboardDevice;
  audit: CollectorConfigAudit[];
}

export default function DevicePage() {
  const params = useParams<{ deviceId: string }>();
  const deviceId = params.deviceId;
  const [screen, setScreen] = useState<DeviceScreen | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function refresh(): Promise<void> {
    const origin = cloudApiOrigin();
    const [device, audit] = await Promise.all([
      getDevice(window.fetch, origin, deviceId),
      getCollectorAudit(window.fetch, origin, deviceId),
    ]);
    setScreen({ device, audit });
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

  if (error !== null && screen === null) return <main><p role="alert">{error}</p></main>;
  if (screen === null) return <main><p>Loading device…</p></main>;

  const disabled = busy || screen.device.revoked;
  return (
    <main>
      <h1>Device</h1>
      <p>Configuration revision: {screen.device.configuration_revision}</p>
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
        name="WeChat outbound text"
        detail="Outgoing text only; 90-day retention"
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
      <button type="button" disabled={disabled} onClick={() => void revoke()}>
        {screen.device.revoked ? "Device revoked" : "Revoke device"}
      </button>
      <h2>Configuration audit</h2>
      {screen.audit.length === 0 ? <p>No configuration changes.</p> : (
        <ol>
          {screen.audit.map((entry) => (
            <li key={`${entry.configuration_revision}-${entry.created_at}`}>
              Revision {entry.configuration_revision} by {entry.actor_user_id} at {entry.created_at}
            </li>
          ))}
        </ol>
      )}
    </main>
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
    <section aria-label={name}>
      <h2>{name}</h2>
      <p>{detail}</p>
      <button type="button" aria-pressed={enabled} disabled={disabled} onClick={onToggle}>
        {enabled ? "Enabled" : "Disabled"}
      </button>
    </section>
  );
}

function messageFor(cause: unknown): string {
  return cause instanceof DashboardApiError ? cause.message : "Unable to update device configuration.";
}
