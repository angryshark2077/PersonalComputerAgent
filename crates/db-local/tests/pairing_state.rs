use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use pca_db_local::{AppliedCollectorControl, DbActorHandle, PairingState};
use rusqlite::Connection;

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TempDirectory(PathBuf);

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn database_path() -> (TempDirectory, PathBuf) {
    let identifier = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "pca-db-local-pairing-{}-{identifier}",
        std::process::id()
    ));
    std::fs::create_dir(&directory).expect("temporary directory");
    let path = directory.join("agent.sqlite3");
    (TempDirectory(directory), path)
}

fn device_id() -> &'static str {
    "01981111-7111-8111-8111-111111111111"
}

fn workspace_id() -> &'static str {
    "01982222-7222-8222-8222-222222222222"
}

fn cloud_api_origin() -> &'static str {
    "https://pca-cloud-api-production.up.railway.app"
}

#[tokio::test]
async fn pairing_state_is_absent_until_validated_credentials_are_saved() {
    let (_directory, path) = database_path();
    let db = DbActorHandle::open(&path, "0.3.0")
        .await
        .expect("open database");

    assert_eq!(db.load_pairing_state().await.expect("load state"), None);
    assert!(db.save_control_revision(1).await.is_err());
    assert_eq!(
        db.health().await.expect("database health").schema_version,
        18
    );
}

#[tokio::test]
async fn pairing_state_never_persists_token_material() {
    let (_directory, path) = database_path();
    let db = DbActorHandle::open(&path, "0.3.0")
        .await
        .expect("open database");
    db.save_pairing_state(&PairingState::paired(
        device_id(),
        workspace_id(),
        "keychain://pca/device/current",
        7,
        cloud_api_origin(),
    ))
    .await
    .expect("save state");

    let state = db
        .load_pairing_state()
        .await
        .expect("load state")
        .expect("saved state");
    assert_eq!(state.device_id, device_id());
    assert_eq!(state.workspace_id, workspace_id());
    assert_eq!(state.credential_ref, "keychain://pca/device/current");
    assert_eq!(state.credential_generation, 7);
    assert_eq!(state.cloud_api_origin, cloud_api_origin());
    assert_eq!(state.applied_control_revision, 0);
    assert!(!state.manually_unpaired);

    db.shutdown().await.expect("close database");
    let connection = Connection::open(&path).expect("inspect database");
    let columns = connection
        .prepare("PRAGMA table_info(pairing_state)")
        .expect("prepare column query")
        .query_map([], |row| row.get::<_, String>(1))
        .expect("query columns")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("read columns");
    assert_eq!(
        columns,
        vec![
            "singleton_id",
            "device_id",
            "workspace_id",
            "credential_ref",
            "credential_generation",
            "applied_control_revision",
            "paired_at_ms",
            "cloud_api_origin",
            "manually_unpaired",
        ]
    );
    let database_bytes = std::fs::read(path).expect("read database bytes");
    assert!(!String::from_utf8_lossy(&database_bytes).contains("refresh_token"));
}

#[tokio::test]
async fn control_revision_is_monotonic_and_manual_unpair_preserves_the_record() {
    let (_directory, path) = database_path();
    let db = DbActorHandle::open(&path, "0.3.0")
        .await
        .expect("open database");
    db.save_pairing_state(&PairingState::paired(
        device_id(),
        workspace_id(),
        "keychain://pca/device/current",
        2,
        cloud_api_origin(),
    ))
    .await
    .expect("save state");

    db.save_control_revision(9)
        .await
        .expect("save newer revision");
    db.save_control_revision(4)
        .await
        .expect("ignore stale revision");
    assert_eq!(
        db.load_pairing_state()
            .await
            .expect("load state")
            .expect("saved state")
            .applied_control_revision,
        9
    );

    let applied = AppliedCollectorControl {
        device_id: device_id().to_owned(),
        workspace_id: workspace_id().to_owned(),
        configuration_revision: 10,
        communication_wechat_enabled: true,
        screen_capture_enabled: true,
        screen_capture_scheduled_enabled: true,
        screen_capture_interval_seconds: 300,
        screen_capture_activity_enabled: true,
        screen_capture_activity_min_interval_seconds: 30,
        screen_capture_excluded_bundle_ids: vec!["com.example.private".to_owned()],
        updated_at_ms: 1_755_000_000_000,
    };
    db.save_applied_collector_control(&applied)
        .await
        .expect("save applied Collector control");
    assert_eq!(
        db.load_applied_collector_control()
            .await
            .expect("load applied Collector control"),
        Some(applied)
    );

    db.mark_pairing_manually_unpaired_and_disable_sensitive_collectors()
        .await
        .expect("mark manual unpair");
    let state = db
        .load_pairing_state()
        .await
        .expect("load manually unpaired state")
        .expect("pairing record remains");
    assert!(state.manually_unpaired);
    assert_eq!(state.applied_control_revision, 10);
    assert_eq!(
        db.load_applied_collector_control()
            .await
            .expect("load cleared control"),
        None
    );
}

#[tokio::test]
async fn applied_control_rejects_a_different_identity_and_stale_replacement() {
    let (_directory, path) = database_path();
    let db = DbActorHandle::open(&path, "0.3.0")
        .await
        .expect("open database");
    db.save_pairing_state(&PairingState::paired(
        device_id(),
        workspace_id(),
        "keychain://pca/device/current",
        2,
        cloud_api_origin(),
    ))
    .await
    .expect("save state");
    let mut control = AppliedCollectorControl {
        device_id: device_id().to_owned(),
        workspace_id: workspace_id().to_owned(),
        configuration_revision: 8,
        communication_wechat_enabled: true,
        screen_capture_enabled: true,
        screen_capture_scheduled_enabled: true,
        screen_capture_interval_seconds: 300,
        screen_capture_activity_enabled: true,
        screen_capture_activity_min_interval_seconds: 30,
        screen_capture_excluded_bundle_ids: Vec::new(),
        updated_at_ms: 8,
    };
    db.save_applied_collector_control(&control)
        .await
        .expect("save current control");
    control.configuration_revision = 7;
    control.communication_wechat_enabled = false;
    control.updated_at_ms = 9;
    db.save_applied_collector_control(&control)
        .await
        .expect("ignore stale control");
    let stored = db
        .load_applied_collector_control()
        .await
        .expect("load current control")
        .expect("saved control");
    assert_eq!(stored.configuration_revision, 8);
    assert!(stored.communication_wechat_enabled);

    control.workspace_id = "01983333-7333-8333-8333-333333333333".to_owned();
    control.configuration_revision = 9;
    assert!(db.save_applied_collector_control(&control).await.is_err());

    db.save_pairing_state(&PairingState::paired(
        "01984444-7444-8444-8444-444444444444",
        workspace_id(),
        "keychain://pca/device/current",
        1,
        cloud_api_origin(),
    ))
    .await
    .expect("replace pairing identity");
    assert_eq!(
        db.load_applied_collector_control()
            .await
            .expect("load replaced pairing control"),
        None
    );
}

#[tokio::test]
async fn pairing_state_rejects_non_uuid_identifiers_and_non_keychain_references() {
    let (_directory, path) = database_path();
    let db = DbActorHandle::open(&path, "0.3.0")
        .await
        .expect("open database");

    let invalid_id = PairingState::paired(
        "zzzzzzzz-zzzz-zzzz-zzzz-zzzzzzzzzzzz",
        workspace_id(),
        "keychain://pca/device/current",
        1,
        cloud_api_origin(),
    );
    assert!(db.save_pairing_state(&invalid_id).await.is_err());

    let misplaced_hyphen = PairingState::paired(
        "-1981111-7111-8111-8111-111111111111",
        workspace_id(),
        "keychain://pca/device/current",
        1,
        cloud_api_origin(),
    );
    assert!(db.save_pairing_state(&misplaced_hyphen).await.is_err());

    let inline_credential = PairingState::paired(
        device_id(),
        workspace_id(),
        "secret-body",
        1,
        cloud_api_origin(),
    );
    assert!(db.save_pairing_state(&inline_credential).await.is_err());
}
