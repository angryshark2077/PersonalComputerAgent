"use client";

import Link from "next/link";
import { useEffect, useState } from "react";

import { DashboardApiError, cloudApiOrigin, getDevices, getWorkspaces, type DashboardWorkspace } from "../lib/api";
import { getBrowserSession, redirectToSignIn } from "../lib/auth";

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

  if (error !== null) return <main><p role="alert">{error}</p></main>;
  if (dashboard === null) return <main><p>Loading Owner Dashboard…</p></main>;

  return (
    <main>
      <h1>Personal Computer Agent</h1>
      {dashboard.workspace === null ? <p>No Owner Workspace is available.</p> : <p>Workspace: {dashboard.workspace.name}</p>}
      <h2>Devices</h2>
      {dashboard.devices.length === 0 ? <p>No paired devices.</p> : (
        <ul>
          {dashboard.devices.map((device) => (
            <li key={device.device_id}>
              <Link href={`/devices/${encodeURIComponent(device.device_id)}`}>Device {device.device_id}</Link>
              {device.revoked ? " (revoked)" : ""}
            </li>
          ))}
        </ul>
      )}
    </main>
  );
}

function messageFor(cause: unknown): string {
  return cause instanceof DashboardApiError ? cause.message : "Unable to load the Owner Dashboard.";
}
