use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

use pca_agentd::cloud_control::{
    apply_snapshot, AgentControlSnapshot, CloudControlRuntime, CloudControlRuntimeError,
    ControlClient, ControlError, ControlFuture,
};
use pca_db_local::{DbActorHandle, PairingState};
use pca_domain::{CollectorState, CollectorStatus};
use pca_keychain::{
    CredentialError, CredentialStore, DeviceCredential, DEVICE_CREDENTIAL_ACCOUNT,
    DEVICE_CREDENTIAL_SERVICE,
};
use tempfile::TempDir;
use tokio::sync::watch;

#[derive(Default)]
struct MemoryStore {
    values: Mutex<BTreeMap<(String, String), Vec<u8>>>,
    fail_delete: AtomicBool,
    delete_attempts: AtomicUsize,
}

impl CredentialStore for MemoryStore {
    fn load(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, CredentialError> {
        Ok(self
            .values
            .lock()
            .map_err(|_| CredentialError::Unavailable)?
            .get(&(service.to_owned(), account.to_owned()))
            .cloned())
    }

    fn store(&self, service: &str, account: &str, value: &[u8]) -> Result<(), CredentialError> {
        self.values
            .lock()
            .map_err(|_| CredentialError::Unavailable)?
            .insert((service.to_owned(), account.to_owned()), value.to_vec());
        Ok(())
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), CredentialError> {
        self.delete_attempts.fetch_add(1, Ordering::Relaxed);
        if self.fail_delete.load(Ordering::Relaxed) {
            return Err(CredentialError::OperationFailed);
        }
        self.values
            .lock()
            .map_err(|_| CredentialError::Unavailable)?
            .remove(&(service.to_owned(), account.to_owned()));
        Ok(())
    }
}

struct RevokedClient;

impl ControlClient for RevokedClient {
    fn refresh<'a>(&'a self, _: &'a DeviceCredential) -> ControlFuture<'a, DeviceCredential> {
        Box::pin(async { Err(ControlError::Revoked) })
    }

    fn heartbeat_and_control<'a>(
        &'a self,
        _: &'a DeviceCredential,
        _: u64,
    ) -> ControlFuture<'a, pca_agentd::cloud_control::AgentControlSnapshot> {
        Box::pin(async { Err(ControlError::Revoked) })
    }
}

struct SequenceClient {
    calls: AtomicUsize,
    snapshots: Mutex<VecDeque<Result<AgentControlSnapshot, ControlError>>>,
}

impl ControlClient for SequenceClient {
    fn refresh<'a>(&'a self, _: &'a DeviceCredential) -> ControlFuture<'a, DeviceCredential> {
        Box::pin(async { Err(ControlError::Contract) })
    }

    fn heartbeat_and_control<'a>(
        &'a self,
        _: &'a DeviceCredential,
        _: u64,
    ) -> ControlFuture<'a, AgentControlSnapshot> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let result = self
            .snapshots
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Err(ControlError::Transient));
        Box::pin(async move { result })
    }
}

fn credentials(store: Arc<MemoryStore>) -> pca_agentd::cloud_control::LoadedDeviceCredentials {
    let credential = DeviceCredential::new(
        "11111111-1111-4111-8111-111111111111".to_owned(),
        "22222222-2222-4222-8222-222222222222".to_owned(),
        "synthetic-access",
        "synthetic-refresh",
    )
    .unwrap()
    .with_metadata(1, 1_700_000_000_000, 1_800_000_000_000);
    pca_agentd::cloud_control::LoadedDeviceCredentials::new(credential, store)
}

async fn db() -> (TempDir, Arc<DbActorHandle>) {
    let temp = TempDir::new().unwrap();
    let database = Arc::new(
        DbActorHandle::open(&temp.path().join("agent.sqlite"), "test")
            .await
            .unwrap(),
    );
    (temp, database)
}

async fn save_sensitive_enabled(database: &DbActorHandle) {
    for collector_key in ["network", "communication.wechat"] {
        database
            .upsert_collector_state(&CollectorState {
                collector_key: collector_key.to_owned(),
                collector_version: "test".to_owned(),
                status: CollectorStatus::Running,
                desired_config_revision: 9,
                applied_config_revision: 9,
                last_event_at_ms: None,
                last_health_at_ms: None,
                last_error_code: None,
                created_at_ms: 1,
                updated_at_ms: 1,
            })
            .await
            .unwrap();
    }
}

async fn assert_sensitive_disabled(database: &DbActorHandle) {
    let states = database.load_collector_states().await.unwrap();
    assert_eq!(
        states
            .iter()
            .map(|state| (state.collector_key.as_str(), state.status))
            .collect::<Vec<_>>(),
        vec![
            ("communication.wechat", CollectorStatus::Disabled),
            ("network", CollectorStatus::Disabled),
        ]
    );
}

async fn await_unpaired(runtime: &pca_agentd::cloud_control::CloudControlHandle) {
    for _ in 0..100 {
        if runtime.is_unpaired().await {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("control runtime did not process revocation");
}

#[tokio::test(start_paused = true)]
async fn revocation_clears_pairing_and_disables_sensitive_collectors() {
    let (_temp, database) = db().await;
    let store = Arc::new(MemoryStore::default());
    let credentials = credentials(Arc::clone(&store));
    database
        .save_pairing_state(&PairingState::paired(
            credentials.credential().device_id(),
            credentials.credential().workspace_id(),
            "keychain://pca/device/current",
            1,
            "https://pca-cloud-api-production.up.railway.app",
        ))
        .await
        .unwrap();
    save_sensitive_enabled(&database).await;
    store
        .store(
            DEVICE_CREDENTIAL_SERVICE,
            DEVICE_CREDENTIAL_ACCOUNT,
            &credentials.credential().encode().unwrap(),
        )
        .unwrap();

    let runtime =
        CloudControlRuntime::start(Arc::clone(&database), credentials, Arc::new(RevokedClient))
            .await
            .unwrap();
    await_unpaired(&runtime).await;

    assert!(database.load_pairing_state().await.unwrap().is_none());
    assert!(runtime.is_unpaired().await);
    assert_eq!(runtime.applied_revision().await, None);
    assert!(store
        .load(DEVICE_CREDENTIAL_SERVICE, DEVICE_CREDENTIAL_ACCOUNT)
        .unwrap()
        .is_none());
    runtime.shutdown().await.unwrap();
    assert_sensitive_disabled(&database).await;
    match Arc::try_unwrap(database) {
        Ok(database) => database.shutdown().await.unwrap(),
        Err(error) => {
            drop(error);
            panic!("runtime released database after shutdown");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn revocation_notifies_the_agent_runtime_after_local_cleanup() {
    let (_temp, database) = db().await;
    let store = Arc::new(MemoryStore::default());
    let credentials = credentials(Arc::clone(&store));
    database
        .save_pairing_state(&PairingState::paired(
            credentials.credential().device_id(),
            credentials.credential().workspace_id(),
            "keychain://pca/device/current",
            1,
            "https://pca-cloud-api-production.up.railway.app",
        ))
        .await
        .unwrap();
    save_sensitive_enabled(&database).await;
    store
        .store(
            DEVICE_CREDENTIAL_SERVICE,
            DEVICE_CREDENTIAL_ACCOUNT,
            &credentials.credential().encode().unwrap(),
        )
        .unwrap();
    let (pairing_state_sender, mut pairing_state_receiver) = watch::channel(false);

    let runtime = CloudControlRuntime::start_with_pairing_state(
        Arc::clone(&database),
        credentials,
        Arc::new(RevokedClient),
        pairing_state_sender,
    )
    .await
    .unwrap();
    assert!(*pairing_state_receiver.borrow_and_update());
    await_unpaired(&runtime).await;
    pairing_state_receiver.changed().await.unwrap();

    assert!(!*pairing_state_receiver.borrow_and_update());
    assert!(database.load_pairing_state().await.unwrap().is_none());
    assert_sensitive_disabled(&database).await;
    runtime.shutdown().await.unwrap();
    match Arc::try_unwrap(database) {
        Ok(database) => database.shutdown().await.unwrap(),
        Err(error) => {
            drop(error);
            panic!("runtime released database after shutdown");
        }
    }
}

#[tokio::test]
async fn corrupt_startup_credential_clears_pairing_and_disables_sensitive_collectors() {
    let (_temp, database) = db().await;
    let store = Arc::new(MemoryStore::default());
    let credential = credentials(Arc::clone(&store));
    database
        .save_pairing_state(&PairingState::paired(
            credential.credential().device_id(),
            credential.credential().workspace_id(),
            "keychain://pca/device/current",
            1,
            "https://pca-cloud-api-production.up.railway.app",
        ))
        .await
        .unwrap();
    save_sensitive_enabled(&database).await;
    store
        .store(
            DEVICE_CREDENTIAL_SERVICE,
            DEVICE_CREDENTIAL_ACCOUNT,
            b"corrupt",
        )
        .unwrap();

    let runtime = CloudControlRuntime::start_from_keychain(
        Arc::clone(&database),
        store,
        Arc::new(RevokedClient),
    )
    .await
    .unwrap();

    assert!(runtime.is_none());
    assert!(database.load_pairing_state().await.unwrap().is_none());
    assert_sensitive_disabled(&database).await;
    match Arc::try_unwrap(database) {
        Ok(database) => database.shutdown().await.unwrap(),
        Err(error) => {
            drop(error);
            panic!("startup released database");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn failed_keychain_delete_still_disables_sensitive_collectors() {
    let (_temp, database) = db().await;
    let store = Arc::new(MemoryStore::default());
    let credentials = credentials(Arc::clone(&store));
    database
        .save_pairing_state(&PairingState::paired(
            credentials.credential().device_id(),
            credentials.credential().workspace_id(),
            "keychain://pca/device/current",
            1,
            "https://pca-cloud-api-production.up.railway.app",
        ))
        .await
        .unwrap();
    save_sensitive_enabled(&database).await;
    store
        .store(
            DEVICE_CREDENTIAL_SERVICE,
            DEVICE_CREDENTIAL_ACCOUNT,
            &credentials.credential().encode().unwrap(),
        )
        .unwrap();
    store.fail_delete.store(true, Ordering::Relaxed);

    let runtime =
        CloudControlRuntime::start(Arc::clone(&database), credentials, Arc::new(RevokedClient))
            .await
            .unwrap();
    await_unpaired(&runtime).await;

    assert!(runtime.is_unpaired().await);
    assert!(database.load_pairing_state().await.unwrap().is_none());
    assert_sensitive_disabled(&database).await;
    assert_eq!(store.delete_attempts.load(Ordering::Relaxed), 1);
    assert!(matches!(
        runtime.shutdown().await,
        Err(CloudControlRuntimeError::Keychain(
            CredentialError::OperationFailed
        ))
    ));
    match Arc::try_unwrap(database) {
        Ok(database) => database.shutdown().await.unwrap(),
        Err(error) => {
            drop(error);
            panic!("runtime released database after failed Keychain deletion");
        }
    }
}

#[test]
fn exact_v2_wechat_scope_is_required_before_a_revision_can_enable_collection() {
    let exact = serde_json::json!({
        "device_id": "11111111-1111-4111-8111-111111111111",
        "workspace_id": "22222222-2222-4222-8222-222222222222",
        "revoked": false,
        "configuration_revision": 7,
        "collectors": {
            "network": { "enabled": false },
            "communication.wechat": {
                "enabled": true,
                "directions": ["incoming", "outgoing"],
                "message_types": ["text", "audio", "image", "video"],
                "conversation_scope": "direct_and_group_at_most_eight_members",
                "max_group_members": 8,
                "sync_mode": "full",
                "retention_days": 180
            }
        }
    });
    let snapshot: AgentControlSnapshot =
        serde_json::from_value(exact.clone()).expect("exact v2 control parses");
    let applied = apply_snapshot(6, &snapshot)
        .expect("exact v2 scope validates")
        .expect("new revision applies");
    assert_eq!(applied.configuration_revision, 7);
    assert!(applied.communication_wechat_enabled);
    assert!(apply_snapshot(7, &snapshot).unwrap().is_none());

    for (field, invalid) in [
        ("retention_days", serde_json::json!(7)),
        ("max_group_members", serde_json::json!(9)),
        ("sync_mode", serde_json::json!("metadata_only")),
        ("directions", serde_json::json!(["outgoing"])),
        ("message_types", serde_json::json!(["text"])),
    ] {
        let mut malformed = exact.clone();
        malformed["collectors"]["communication.wechat"][field] = invalid;
        assert!(serde_json::from_value::<AgentControlSnapshot>(malformed).is_err());
    }
}

#[tokio::test(start_paused = true)]
async fn communication_revision_notifications_are_monotonic_and_invalid_control_fails_closed() {
    let (_temp, database) = db().await;
    let store = Arc::new(MemoryStore::default());
    let loaded = credentials(Arc::clone(&store));
    database
        .save_pairing_state(&PairingState::paired(
            loaded.credential().device_id(),
            loaded.credential().workspace_id(),
            "keychain://pca/device/current",
            1,
            "https://pca-cloud-api-production.up.railway.app",
        ))
        .await
        .unwrap();
    let enabled = exact_snapshot(1, true);
    let stale = exact_snapshot(1, false);
    let mut invalid_identity = exact_snapshot(2, true);
    invalid_identity.device_id = "33333333-3333-4333-8333-333333333333".to_owned();
    let client = Arc::new(SequenceClient {
        calls: AtomicUsize::new(0),
        snapshots: Mutex::new(VecDeque::from([
            Ok(enabled),
            Ok(stale),
            Ok(invalid_identity),
        ])),
    });
    let runtime = CloudControlRuntime::start(
        Arc::clone(&database),
        loaded,
        Arc::clone(&client) as Arc<dyn ControlClient>,
    )
    .await
    .unwrap();
    let mut controls = runtime.communication_controls();
    assert!(controls.borrow_and_update().is_none());

    controls.changed().await.unwrap();
    let first = controls
        .borrow_and_update()
        .expect("valid revision is notified");
    assert_eq!(first.configuration_revision, 1);
    assert!(first.communication_wechat_enabled);

    tokio::time::advance(std::time::Duration::from_secs(30)).await;
    wait_for_calls(&client.calls, 2).await;
    assert!(!controls.has_changed().unwrap());
    assert_eq!(controls.borrow().as_ref(), Some(&first));
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }

    tokio::time::advance(std::time::Duration::from_secs(30)).await;
    wait_for_calls(&client.calls, 3).await;
    controls.changed().await.unwrap();
    assert!(controls.borrow_and_update().is_none());

    runtime.shutdown().await.unwrap();
    match Arc::try_unwrap(database) {
        Ok(database) => database.shutdown().await.unwrap(),
        Err(error) => {
            drop(error);
            panic!("control runtime released database after shutdown");
        }
    }
}

fn exact_snapshot(revision: u64, enabled: bool) -> AgentControlSnapshot {
    serde_json::from_value(serde_json::json!({
        "device_id": "11111111-1111-4111-8111-111111111111",
        "workspace_id": "22222222-2222-4222-8222-222222222222",
        "revoked": false,
        "configuration_revision": revision,
        "collectors": {
            "network": { "enabled": false },
            "communication.wechat": {
                "enabled": enabled,
                "directions": ["incoming", "outgoing"],
                "message_types": ["text", "audio", "image", "video"],
                "conversation_scope": "direct_and_group_at_most_eight_members",
                "max_group_members": 8,
                "sync_mode": "full",
                "retention_days": 180
            }
        }
    }))
    .unwrap()
}

async fn wait_for_calls(calls: &AtomicUsize, expected: usize) {
    for _ in 0..100 {
        if calls.load(Ordering::SeqCst) >= expected {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("control client did not reach {expected} calls");
}
