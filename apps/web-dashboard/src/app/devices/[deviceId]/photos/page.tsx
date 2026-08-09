"use client";

import Link from "next/link";
import { useParams } from "next/navigation";
import { useEffect, useState } from "react";

import { DashboardShell } from "../../../../components/dashboard-shell";
import {
  DashboardApiError,
  cloudApiOrigin,
  getPhotoReadUrl,
  getPhotos,
  type DashboardPhoto,
  type DashboardPhotoPage,
} from "../../../../lib/api";
import { getBrowserSession, redirectToSignIn } from "../../../../lib/auth";

interface PhotoWithUrl extends DashboardPhoto { url: string }

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

export default function PhotosPage() {
  const { deviceId } = useParams<{ deviceId: string }>();
  const [photos, setPhotos] = useState<PhotoWithUrl[] | null>(null);
  const [pagination, setPagination] = useState<DashboardPhotoPage["pagination"] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function loadPage(requestedPage: number): Promise<void> {
    setBusy(true);
    setError(null);
    try {
      const origin = cloudApiOrigin();
      const page = await getPhotos(window.fetch, origin, deviceId, undefined, requestedPage);
      setPhotos(await Promise.all(page.photos.map(async (record) => ({
        ...record,
        url: await getPhotoReadUrl(window.fetch, origin, deviceId, record.photo_id),
      }))));
      setPagination(page.pagination);
    } catch (cause) {
      setError(cause instanceof DashboardApiError ? cause.message : "Unable to load photos.");
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
        <h1>Photos</h1>
        <p>Original photos and videos are retained permanently in private R2. Read links expire after five minutes.</p>
      </section>
      <button className="primary-button" type="button" disabled={busy} onClick={() => void loadPage(1)}>
        {busy ? "Refreshing…" : "Refresh photos"}
      </button>
      {error === null ? null : <p role="alert">{error}</p>}
      {photos === null ? <p className="status-note">Loading photos…</p> : null}
      {photos?.length === 0 ? <p className="status-note">No completed photos or videos.</p> : null}
      {photos !== null && photos.length > 0 ? (
        <div className="screenshot-grid">
          {photos.map((photo) => (
            <figure className="dashboard-panel screenshot-card" key={photo.photo_id}>
              {photo.media_type === "video" ? (
                <video src={photo.url} controls preload="none" />
              ) : (
                <a href={photo.url} target="_blank" rel="noreferrer">
                  <img src={photo.url} alt={photo.original_filename} loading="lazy" />
                </a>
              )}
              <figcaption>
                <strong>{photo.original_filename}</strong>
                <span>{new Date(photo.captured_at).toLocaleString()} · {photo.pixel_width}×{photo.pixel_height}</span>
                <span>{photo.album_names.length === 0 ? "No named album" : photo.album_names.join(", ")}</span>
                <span>{(photo.size_bytes / 1024 / 1024).toFixed(1)} MB</span>
                <a href={photo.url} target="_blank" rel="noreferrer">Open original</a>
              </figcaption>
            </figure>
          ))}
        </div>
      ) : null}
      {pagination !== null && pagination.total_pages > 1 ? (
        <nav aria-label="Photo pages">
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
