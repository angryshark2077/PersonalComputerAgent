"use client";

import Link from "next/link";
import { useParams } from "next/navigation";
import { useEffect, useState } from "react";

import { DashboardShell } from "../../../../components/dashboard-shell";
import {
  DashboardApiError,
  cloudApiOrigin,
  getScreenshotReadUrl,
  getScreenshots,
  type DashboardScreenshot,
} from "../../../../lib/api";
import { getBrowserSession, redirectToSignIn } from "../../../../lib/auth";

interface ScreenshotWithUrl extends DashboardScreenshot {
  url: string;
}

export default function ScreenshotsPage() {
  const { deviceId } = useParams<{ deviceId: string }>();
  const [screenshots, setScreenshots] = useState<ScreenshotWithUrl[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function refresh(): Promise<void> {
    setBusy(true);
    setError(null);
    try {
      const origin = cloudApiOrigin();
      const records = await getScreenshots(window.fetch, origin, deviceId);
      setScreenshots(await Promise.all(records.map(async (record) => ({
        ...record,
        url: await getScreenshotReadUrl(window.fetch, origin, deviceId, record.screenshot_id),
      }))));
    } catch (cause) {
      setError(cause instanceof DashboardApiError ? cause.message : "Unable to load screenshots.");
    } finally {
      setBusy(false);
    }
  }

  useEffect(() => {
    void (async () => {
      if ((await getBrowserSession(window.fetch, cloudApiOrigin())) === null) {
        redirectToSignIn();
        return;
      }
      await refresh();
    })();
  }, [deviceId]);

  return (
    <DashboardShell>
      <Link className="back-link" href={`/devices/${encodeURIComponent(deviceId)}`}>Back to device</Link>
      <section className="page-heading">
        <p className="workspace-name">Private R2 media</p>
        <h1>Screenshots</h1>
        <p>Active-display captures are retained in private R2 for seven days. Read links expire after five minutes.</p>
      </section>
      <button className="primary-button" type="button" disabled={busy} onClick={() => void refresh()}>
        {busy ? "Refreshing…" : "Refresh screenshots"}
      </button>
      {error === null ? null : <p role="alert">{error}</p>}
      {screenshots === null ? <p className="status-note">Loading screenshots…</p> : null}
      {screenshots?.length === 0 ? <p className="status-note">No completed screenshots.</p> : null}
      {screenshots !== null && screenshots.length > 0 ? (
        <div className="screenshot-grid">
          {screenshots.map((screenshot) => (
            <figure className="dashboard-panel screenshot-card" key={screenshot.screenshot_id}>
              <a href={screenshot.url} target="_blank" rel="noreferrer">
                <img
                  src={screenshot.url}
                  alt={`Screenshot captured ${new Date(screenshot.captured_at).toLocaleString()}`}
                  loading="lazy"
                />
              </a>
              <figcaption>
                <strong>{new Date(screenshot.captured_at).toLocaleString()}</strong>
                <span>{screenshot.trigger} · {screenshot.pixel_width}×{screenshot.pixel_height}</span>
                <span>{screenshot.app_bundle_id ?? "Unknown application"}</span>
              </figcaption>
            </figure>
          ))}
        </div>
      ) : null}
    </DashboardShell>
  );
}
