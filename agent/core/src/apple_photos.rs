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
    persist_photo_manifest, photo_spool_root, AppliedControl, PendingPhoto,
};

const POLL_INTERVAL: Duration = Duration::from_mins(1);
const LOOKBACK_DAYS: i64 = 60;
const PAGE_SIZE: u8 = 50;
const MAX_UPLOAD_BYTES: u64 = 500 * 1024 * 1024;

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
    loop {
        tokio::select! {
            _ = interval.tick() => {
                if controls.borrow().as_ref().is_some_and(|value| value.photos_library_enabled) {
                    let _ = collect_once(&database, &bridge, &workspace_id, &device_id).await;
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
) -> Result<(), ()> {
    if bridge.photo_authorization().await.map_err(|_| ())? != "available" {
        return Err(());
    }
    let cutoff = (OffsetDateTime::now_utc() - TimeDuration::days(LOOKBACK_DAYS))
        .format(&Rfc3339)
        .map_err(|_| ())?;
    let mut after_created_at = None;
    let mut after_local_identifier = None;
    loop {
        let (status, assets) = bridge
            .list_photo_assets(
                after_created_at.clone(),
                after_local_identifier.clone(),
                cutoff.clone(),
                PAGE_SIZE,
            )
            .await
            .map_err(|_| ())?;
        if status != "available" {
            return Err(());
        }
        if assets.is_empty() {
            return Ok(());
        }
        let count = assets.len();
        for asset in assets {
            after_created_at = Some(asset.created_at.clone());
            after_local_identifier = Some(asset.local_identifier.clone());
            let _ = queue_asset(database, bridge, workspace_id, device_id, asset).await;
        }
        if count < usize::from(PAGE_SIZE) {
            return Ok(());
        }
    }
}

async fn queue_asset(
    database: &DbActorHandle,
    bridge: &ScreenCaptureCommandHandle,
    workspace_id: &str,
    device_id: &str,
    asset: PhotoAssetRecord,
) -> Result<(), ()> {
    let photo_id = stable_uuid(workspace_id, device_id, "photo", &asset.local_identifier);
    let event_id = stable_uuid(workspace_id, device_id, "event", &asset.local_identifier);
    let spool_root = photo_spool_root().map_err(|_| ())?;
    let manifest_path = spool_root.join(format!("{photo_id}.json"));
    if tokio::fs::try_exists(&manifest_path)
        .await
        .map_err(|_| ())?
    {
        return Ok(());
    }
    let file_uuid = Uuid::parse_str(&photo_id).map_err(|_| ())?;
    let exported = bridge
        .export_photo_asset(asset.local_identifier.clone(), file_uuid)
        .await
        .map_err(|_| ())?
        .ok_or(())?;
    validate_export_path(&exported, &spool_root, &photo_id)?;
    let (sha256, size_bytes) = hash_file(&exported).await?;
    if size_bytes == 0 {
        return Err(());
    }
    if size_bytes > MAX_UPLOAD_BYTES {
        let _ = tokio::fs::remove_file(&exported).await;
        return Ok(());
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
    .map_err(|_| ())
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
    use super::{stable_uuid, LOOKBACK_DAYS};

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
}
