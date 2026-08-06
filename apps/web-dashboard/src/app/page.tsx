"use client";

import Link from "next/link";
import { useEffect, useState } from "react";

import { DashboardApiError, cloudApiOrigin, getDevices, getWorkspaces, type DashboardWorkspace } from "../lib/api";
import { getBrowserSession, redirectToSignIn } from "../lib/auth";
import { DashboardShell } from "../components/dashboard-shell";

interface DashboardHome {
  workspace: DashboardWorkspace | null;
  devices: Awaited<ReturnType<typeof getDevices>>;
}

export default function HomePage() {
  const [dashboard, setDashboard] = useState<DashboardHome | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const origin = cloudApiOrigin();
    void (async () => {
      if ((await getBrowserSession(window.fetch, origin)) === null) {
        redirectToSignIn();
        return;
      }
      try {
        const [workspaces, devices] = await Promise.all([
          getWorkspaces(window.fetch, origin),
          getDevices(window.fetch, origin),
        ]);
        setDashboard({ workspace: workspaces[0] ?? null, devices });
      } catch (cause) {
        setError(messageFor(cause));
      }
    })();
  }, []);

  return (
    <DashboardShell>
      {error !== null ? <p role="alert">{error}</p> : null}
      {dashboard === null ? <p className="status-note">Loading Owner Dashboard…</p> : (
        <>
          <section className="page-heading">
            <p className="workspace-name">{dashboard.workspace === null ? "No Owner Workspace" : dashboard.workspace.name}</p>
            <h1>Devices</h1>
            <p>Manage the Macs connected to your Personal Computer Agent workspace.</p>
          </section>
          <section className="dashboard-panel" aria-labelledby="devices-heading">
            <div className="panel-header">
              <h2 id="devices-heading">Connected devices</h2>
              <p className="panel-count">{dashboard.devices.length} total</p>
            </div>
            {dashboard.devices.length === 0 ? <p className="empty-state">No paired devices yet.</p> : (
              <ul className="device-list">
                {dashboard.devices.map((device) => (
                  <li key={device.device_id}>
                    <Link href={`/devices/${encodeURIComponent(device.device_id)}`}>
                      Device {device.device_id}{device.revoked ? " (revoked)" : ""}
                    </Link>
                    <p className="status-note">{deviceStatusLabel(device)}</p>
                  </li>
                ))}
              </ul>
            )}
          </section>
        </>
      )}
    </DashboardShell>
  );
}

function deviceStatusLabel(device: Awaited<ReturnType<typeof getDevices>>[number]): string {
  if (device.revoked) return "Revoked";
  if (device.status === null) return "Agent has not checked in yet";
  return `${device.status.presence[0]?.toUpperCase()}${device.status.presence.slice(1)} · Agent ${device.status.agent_version} · Last check-in ${new Date(device.status.observed_at).toLocaleString()}`;
}

function messageFor(cause: unknown): string {
  return cause instanceof DashboardApiError ? cause.message : "Unable to load the Owner Dashboard.";
}
