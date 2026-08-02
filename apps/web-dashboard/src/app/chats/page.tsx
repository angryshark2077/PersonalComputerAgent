"use client";

import Link from "next/link";
import { useEffect, useState } from "react";

import { DashboardShell } from "../../components/dashboard-shell";
import { DashboardApiError, cloudApiOrigin, getDevices } from "../../lib/api";
import { getBrowserSession, redirectToSignIn } from "../../lib/auth";

type DashboardDeviceSummary = Awaited<ReturnType<typeof getDevices>>[number];

export default function ChatsPage() {
  const [devices, setDevices] = useState<DashboardDeviceSummary[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const origin = cloudApiOrigin();
    void (async () => {
      if ((await getBrowserSession(window.fetch, origin)) === null) {
        redirectToSignIn();
        return;
      }
      try {
        setDevices((await getDevices(window.fetch, origin)).filter((device) => !device.revoked));
      } catch (cause) {
        setError(messageFor(cause));
      }
    })();
  }, []);

  return (
    <DashboardShell>
      <section className="page-heading">
        <p className="workspace-name">WeChat collection</p>
        <h1>Chats</h1>
        <p>Select the Mac whose WeChat account you want to inspect.</p>
      </section>
      {error !== null ? <p role="alert">{error}</p> : null}
      {devices === null ? <p className="status-note">Loading devices…</p> : (
        <section className="dashboard-panel" aria-labelledby="chat-devices-heading">
          <div className="panel-header">
            <h2 id="chat-devices-heading">Devices</h2>
            <p className="panel-count">{devices.length} total</p>
          </div>
          {devices.length === 0 ? <p className="empty-state">No paired devices.</p> : (
            <ul className="device-list">
              {devices.map((device) => (
                <li key={device.device_id}>
                  <Link href={`/devices/${encodeURIComponent(device.device_id)}/chats`}>
                    Device {device.device_id}
                  </Link>
                </li>
              ))}
            </ul>
          )}
        </section>
      )}
    </DashboardShell>
  );
}

function messageFor(cause: unknown): string {
  return cause instanceof DashboardApiError ? cause.message : "Unable to load chat devices.";
}
