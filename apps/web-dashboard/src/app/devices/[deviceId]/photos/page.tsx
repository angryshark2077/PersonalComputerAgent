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
} from "../../../../lib/api";
import { getBrowserSession, redirectToSignIn } from "../../../../lib/auth";

interface PhotoWithUrl extends DashboardPhoto { url: string }

export default function PhotosPage() {
  const { deviceId } = useParams<{ deviceId: string }>();
  const [photos, setPhotos] = useState<PhotoWithUrl[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function refresh(): Promise<void> {
    setBusy(true);
    setError(null);
    try {
      const origin = cloudApiOrigin();
      const records = await getPhotos(window.fetch, origin, deviceId);
      setPhotos(await Promise.all(records.map(async (record) => ({
        ...record,
        url: await getPhotoReadUrl(window.fetch, origin, deviceId, record.photo_id),
      }))));
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
      await refresh();
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
      <button className="primary-button" type="button" disabled={busy} onClick={() => void refresh()}>
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
                <video src={photo.url} controls preload="metadata" />
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
    </DashboardShell>
  );
}
