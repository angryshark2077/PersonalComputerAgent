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
  type DashboardScreenshotPage,
} from "../../../../lib/api";
import { getBrowserSession, redirectToSignIn } from "../../../../lib/auth";

interface ScreenshotWithUrl extends DashboardScreenshot {
  url: string;
}

function pageNumbers(totalPages: number, currentPage: number): Array<number | null> {
  if (totalPages <= 7) return Array.from({ length: totalPages }, (_, index) => index + 1);
  const pages: Array<number | null> = [1];
  const firstMiddle = Math.max(2, currentPage - 1);
  const lastMiddle = Math.min(totalPages - 1, currentPage + 1);
  if (firstMiddle > 2) pages.push(null);
  for (let page = firstMiddle; page <= lastMiddle; page += 1) pages.push(page);
  if (lastMiddle < totalPages - 1) pages.push(null);
  pages.push(totalPages);
  return pages;
}

export default function ScreenshotsPage() {
  const { deviceId } = useParams<{ deviceId: string }>();
  const [screenshots, setScreenshots] = useState<ScreenshotWithUrl[] | null>(null);
  const [pagination, setPagination] = useState<DashboardScreenshotPage["pagination"] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function loadPage(requestedPage: number): Promise<void> {
    setBusy(true);
    setError(null);
    try {
      const origin = cloudApiOrigin();
      const page = await getScreenshots(window.fetch, origin, deviceId, undefined, requestedPage);
      setScreenshots(await Promise.all(page.screenshots.map(async (record) => ({
        ...record,
        url: await getScreenshotReadUrl(window.fetch, origin, deviceId, record.screenshot_id),
      }))));
      setPagination(page.pagination);
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
      await loadPage(1);
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
      <button className="primary-button" type="button" disabled={busy} onClick={() => void loadPage(1)}>
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
      {pagination !== null && pagination.total_pages > 1 ? (
        <nav aria-label="Screenshot pages">
          <button type="button" disabled={busy || pagination.page === 1} onClick={() => void loadPage(pagination.page - 1)}>
            Previous
          </button>
          {pageNumbers(pagination.total_pages, pagination.page).map((page, index) => page === null ? (
            <span key={`ellipsis-${index}`} aria-hidden="true">…</span>
          ) : (
            <button
              key={page}
              type="button"
              disabled={busy || page === pagination.page}
              aria-current={page === pagination.page ? "page" : undefined}
              onClick={() => void loadPage(page)}
            >
              {page}
            </button>
          ))}
          <button type="button" disabled={busy || pagination.page === pagination.total_pages} onClick={() => void loadPage(pagination.page + 1)}>
            Next
          </button>
        </nav>
      ) : null}
    </DashboardShell>
  );
}
