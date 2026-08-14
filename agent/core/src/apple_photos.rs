use std::{path::Path, sync::Arc, time::Duration};

use ::time::{format_description::well_known::Rfc3339, Duration as TimeDuration, OffsetDateTime};
use pca_bridge_client::{PhotoAssetRecord, ScreenCaptureCommandHandle};
use pca_db_local::DbActorHandle;
use pca_domain::{EventCommit, EventEnvelope, Sensitivity};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tokio::{io::AsyncReadExt, sync::watch, time};
use uuid::Uuid;

use crate::cloud_control::{
    persist_aux_collector_state, persist_photo_manifest, persist_photo_marker,
    photo_asset_is_handled, photo_spool_root, AppliedControl, PendingPhoto, PhotoMarker,
};

const POLL_INTERVAL: Duration = Duration::from_mins(1);
const FULL_RESCAN_INTERVAL: Duration = Duration::from_mins(30);
const COLLECTION_DEADLINE: Duration = Duration::from_mins(30);
const STATE_PERSIST_DEADLINE: Duration = Duration::from_secs(10);
const LOOKBACK_DAYS: i64 = 60;
const PAGE_SIZE: u8 = 50;
const MAX_UPLOAD_BYTES: u64 = 500 * 1024 * 1024;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct PhotoCursor {
    created_at: Option<String>,
    local_identifier: Option<String>,
}

struct CollectionResult {
    event_observed: bool,
    cursor: PhotoCursor,
}

pub(crate) async fn run(
    database: Arc<DbActorHandle>,
    bridge: ScreenCaptureCommandHandle,
    workspace_id: String,
    device_id: String,
    mut controls: watch::Receiver<Option<AppliedControl>>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut interval = time::interval(POLL_INTERVAL);
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut cursor = PhotoCursor::default();
    let mut last_full_scan = None;
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let control = controls.borrow().clone();
                let revision = control.as_ref().map_or(0, |value| value.configuration_revision);
                let enabled = control.as_ref().is_some_and(|value| value.photos_library_enabled);
                let result = if enabled {
                    let now = time::Instant::now();
                    let full_scan = full_scan_due(last_full_scan, now);
                    let start_cursor = if full_scan {
                        PhotoCursor::default()
                    } else {
                        cursor.clone()
                    };
                    if let Ok(result) = time::timeout(
                        COLLECTION_DEADLINE,
                        collect_once(
                            &database,
                            &bridge,
                            &workspace_id,
                            &device_id,
                            start_cursor,
                        ),
                    ).await {
                        result.map(|collected| {
                            cursor = collected.cursor;
                            if full_scan {
                                last_full_scan = Some(now);
                            }
                            collected.event_observed
                        }).map_err(|()| "PHOTOS_COLLECTION_FAILED")
                    } else {
                        let _ = time::timeout(
                            STATE_PERSIST_DEADLINE,
                            persist_aux_collector_state(
                                &database,
                                "photos.library",
                                true,
                                revision,
                                false,
                                Some("PHOTOS_COLLECTION_TIMEOUT"),
                            ),
                        ).await;
                        return;
                    }
                } else {
                    Ok(false)
                };
                if !matches!(time::timeout(
                    STATE_PERSIST_DEADLINE,
                    persist_aux_collector_state(
                        &database,
                        "photos.library",
                        enabled,
                        revision,
                        result.as_ref().copied().unwrap_or(false),
                        result.err(),
                    ),
                ).await, Ok(Ok(()))) {
                    return;
                }
            }
            changed = controls.changed() => {
                if changed.is_err() { return; }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow_and_update() { return; }
            }
        }
    }
}

async fn collect_once(
    database: &DbActorHandle,
    bridge: &ScreenCaptureCommandHandle,
    workspace_id: &str,
    device_id: &str,
    mut cursor: PhotoCursor,
) -> Result<CollectionResult, ()> {
    if bridge.photo_authorization().await.map_err(|_| ())? != "available" {
        return Err(());
    }
    let cutoff = (OffsetDateTime::now_utc() - TimeDuration::days(LOOKBACK_DAYS))
        .format(&Rfc3339)
        .map_err(|_| ())?;
    let mut event_observed = false;
    loop {
        let (status, assets) = bridge
            .list_photo_assets(
                cursor.created_at.clone(),
                cursor.local_identifier.clone(),
                cutoff.clone(),
                PAGE_SIZE,
            )
            .await
            .map_err(|_| ())?;
        if status != "available" {
            return Err(());
        }
        if assets.is_empty() {
            return Ok(CollectionResult {
                event_observed,
                cursor,
            });
        }
        let count = assets.len();
        for asset in assets {
            cursor.created_at = Some(asset.created_at.clone());
            cursor.local_identifier = Some(asset.local_identifier.clone());
            event_observed |= queue_asset(database, bridge, workspace_id, device_id, asset).await?;
        }
        if count < usize::from(PAGE_SIZE) {
            return Ok(CollectionResult {
                event_observed,
                cursor,
            });
        }
    }
}

fn full_scan_due(last_full_scan: Option<time::Instant>, now: time::Instant) -> bool {
    last_full_scan.is_none_or(|last| {
        now.checked_duration_since(last)
            .is_some_and(|elapsed| elapsed >= FULL_RESCAN_INTERVAL)
    })
}

async fn queue_asset(
    database: &DbActorHandle,
    bridge: &ScreenCaptureCommandHandle,
    workspace_id: &str,
    device_id: &str,
    asset: PhotoAssetRecord,
) -> Result<bool, ()> {
    let photo_id = stable_uuid(workspace_id, device_id, "photo", &asset.local_identifier);
    let event_id = stable_uuid(workspace_id, device_id, "event", &asset.local_identifier);
    let spool_root = photo_spool_root().map_err(|_| ())?;
    if photo_asset_is_handled(&photo_id).await.map_err(|_| ())? {
        return Ok(false);
    }
    let file_uuid = Uuid::parse_str(&photo_id).map_err(|_| ())?;
    let exported = bridge
        .export_photo_asset(asset.local_identifier.clone(), file_uuid)
        .await
        .map_err(|_| ())?
        .ok_or(())?;
    validate_export_path(&exported, &spool_root, &photo_id)?;
    let exported_size = tokio::fs::metadata(&exported).await.map_err(|_| ())?.len();
    if exceeds_upload_limit(exported_size) {
        tokio::fs::remove_file(&exported).await.map_err(|_| ())?;
        persist_photo_marker(&photo_id, PhotoMarker::Oversized)
            .await
            .map_err(|_| ())?;
        return Ok(false);
    }
    let (sha256, size_bytes) = hash_file(&exported).await?;
    if size_bytes == 0 {
        return Err(());
    }
    if size_bytes > MAX_UPLOAD_BYTES {
        tokio::fs::remove_file(&exported).await.map_err(|_| ())?;
        persist_photo_marker(&photo_id, PhotoMarker::Oversized)
            .await
            .map_err(|_| ())?;
        return Ok(false);
    }
    let payload = serde_json::json!({
        "asset_id": asset.local_identifier,
        "captured_at": asset.created_at,
        "media_type": asset.media_type,
        "original_filename": asset.original_filename,
        "mime_type": asset.mime_type,
        "pixel_width": asset.pixel_width,
        "pixel_height": asset.pixel_height,
        "duration_seconds": asset.duration_seconds,
        "album_names": asset.album_names,
    });
    let event = EventEnvelope {
        event_id: event_id.clone(),
        workspace_id: workspace_id.to_owned(),
        device_id: device_id.to_owned(),
        event_type: "photos.asset_recorded".to_owned(),
        source: "photos.library".to_owned(),
        schema_version: 1,
        occurred_at: asset.created_at.clone(),
        created_at: asset.created_at.clone(),
        sensitivity: Sensitivity::High,
        payload: object(&payload)?,
        attachment_refs: Vec::new(),
        idempotency_key: Some(format!("photos:asset:{}", asset.local_identifier)),
    };
    database
        .commit_events(&EventCommit::try_new(vec![event], None).map_err(|_| ())?)
        .await
        .map_err(|_| ())?;
    persist_photo_manifest(&PendingPhoto {
        photo_id: photo_id.clone(),
        event_id,
        asset_id: asset.local_identifier,
        captured_at: asset.created_at,
        media_type: asset.media_type,
        original_filename: asset.original_filename,
        mime_type: asset.mime_type,
        pixel_width: asset.pixel_width,
        pixel_height: asset.pixel_height,
        duration_seconds: asset.duration_seconds,
        album_names: asset.album_names,
        sha256,
        size_bytes,
        media_file_name: Some(photo_id),
        completed: false,
    })
    .await
    .map_err(|_| ())?;
    Ok(true)
}

fn validate_export_path(path: &Path, root: &Path, photo_id: &str) -> Result<(), ()> {
    if path.parent() != Some(root)
        || path.file_name().and_then(|value| value.to_str()) != Some(photo_id)
    {
        return Err(());
    }
    Ok(())
}

async fn hash_file(path: &Path) -> Result<(String, u64), ()> {
    let mut file = tokio::fs::File::open(path).await.map_err(|_| ())?;
    let mut digest = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let count = file.read(&mut buffer).await.map_err(|_| ())?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
        size = size
            .checked_add(u64::try_from(count).map_err(|_| ())?)
            .ok_or(())?;
    }
    Ok((format!("{:x}", digest.finalize()), size))
}

fn exceeds_upload_limit(size_bytes: u64) -> bool {
    size_bytes > MAX_UPLOAD_BYTES
}

fn stable_uuid(workspace_id: &str, device_id: &str, kind: &str, key: &str) -> String {
    let digest = Sha256::digest(format!("{workspace_id}\0{device_id}\0{kind}\0{key}").as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes).hyphenated().to_string()
}

fn object(value: &Value) -> Result<Map<String, Value>, ()> {
    value.as_object().cloned().ok_or(())
}

#[cfg(test)]
mod tests {
    use super::{
        exceeds_upload_limit, full_scan_due, stable_uuid, FULL_RESCAN_INTERVAL, LOOKBACK_DAYS,
        MAX_UPLOAD_BYTES,
    };

    #[test]
    fn photo_library_initial_history_is_sixty_days() {
        assert_eq!(LOOKBACK_DAYS, 60);
    }

    #[test]
    fn photo_identity_is_stable_and_kind_separated() {
        let photo = stable_uuid("workspace", "device", "photo", "asset");
        assert_eq!(photo, stable_uuid("workspace", "device", "photo", "asset"));
        assert_ne!(photo, stable_uuid("workspace", "device", "event", "asset"));
        assert!(uuid::Uuid::parse_str(&photo).is_ok());
    }

    #[test]
    fn photo_upload_limit_allows_exactly_five_hundred_mebibytes() {
        assert_eq!(MAX_UPLOAD_BYTES, 500 * 1024 * 1024);
        assert!(!exceeds_upload_limit(MAX_UPLOAD_BYTES));
        assert!(exceeds_upload_limit(MAX_UPLOAD_BYTES + 1));
    }

    #[test]
    fn photo_library_only_reconciles_the_full_window_every_thirty_minutes() {
        let now = tokio::time::Instant::now();
        assert!(full_scan_due(None, now));
        assert!(!full_scan_due(
            Some(now),
            now + FULL_RESCAN_INTERVAL - std::time::Duration::from_secs(1)
        ));
        assert!(full_scan_due(Some(now), now + FULL_RESCAN_INTERVAL));
    }
}
