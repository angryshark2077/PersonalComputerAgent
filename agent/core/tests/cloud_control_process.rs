use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use pca_agentd::cloud_control::{
    apply_snapshot, AgentControlSnapshot, CloudControlOwner, CloudControlRuntime,
    CloudControlRuntimeError, ControlClient, ControlError, ControlFuture, MediaControlFuture,
    MediaUploadFailure, MediaUploadFailureStage,
};
use pca_agentd::communication::{
    CommunicationAuthorization, CommunicationControl, CommunicationIdentity, CommunicationRuntime,
    CommunicationRuntimeError, UnavailableCommunicationProviderFactory,
};
use pca_bridge_client::screen_capture_command_channel;
use pca_db_local::{AppliedCollectorControl, DbActorHandle, PairingState};
use pca_domain::{CollectorState, CollectorStatus, EventEnvelope, Sensitivity};
use pca_keychain::{
    load_device_credential, CredentialError, CredentialStore, DeviceCredential,
    DEVICE_CREDENTIAL_ACCOUNT, DEVICE_CREDENTIAL_SERVICE,
};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::sync::{watch, Notify};

#[derive(Default)]
struct MemoryStore {
    values: Mutex<BTreeMap<(String, String), Vec<u8>>>,
    fail_delete: AtomicBool,
    delete_attempts: AtomicUsize,
    load_attempts: AtomicUsize,
    load_failures_remaining: AtomicUsize,
    store_attempts: AtomicUsize,
    store_failures_remaining: AtomicUsize,
}

impl CredentialStore for MemoryStore {
    fn load(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, CredentialError> {
        self.load_attempts.fetch_add(1, Ordering::SeqCst);
        if self
            .load_failures_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err(CredentialError::Unavailable);
        }
        Ok(self
            .values
            .lock()
            .map_err(|_| CredentialError::Unavailable)?
            .get(&(service.to_owned(), account.to_owned()))
            .cloned())
    }

    fn store(&self, service: &str, account: &str, value: &[u8]) -> Result<(), CredentialError> {
        self.store_attempts.fetch_add(1, Ordering::SeqCst);
        if self
            .store_failures_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err(CredentialError::Unavailable);
        }
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

struct NetworkStateClient {
    calls: AtomicUsize,
    network_enabled: AtomicBool,
    snapshot: AgentControlSnapshot,
}

struct BlockingSyncClient {
    calls: AtomicUsize,
    snapshots: Mutex<VecDeque<Result<AgentControlSnapshot, ControlError>>>,
    sync_entered: Notify,
    sync_release: Notify,
}

struct BlockingMediaClient {
    calls: AtomicUsize,
    media_entered: Notify,
    media_release: Notify,
    snapshot: AgentControlSnapshot,
}

#[derive(Default)]
struct WorkerConcurrency {
    active: AtomicUsize,
    maximum: AtomicUsize,
}

struct BlockingOwnerClient {
    calls: AtomicUsize,
    release: Notify,
    concurrency: Arc<WorkerConcurrency>,
    snapshot: AgentControlSnapshot,
}

struct ActiveWorkerCall(Arc<WorkerConcurrency>);

impl Drop for ActiveWorkerCall {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

impl ControlClient for BlockingOwnerClient {
    fn refresh<'a>(&'a self, _: &'a DeviceCredential) -> ControlFuture<'a, DeviceCredential> {
        Box::pin(async { Err(ControlError::Contract) })
    }

    fn heartbeat_and_control<'a>(
        &'a self,
        _: &'a DeviceCredential,
        _: u64,
    ) -> ControlFuture<'a, AgentControlSnapshot> {
        Box::pin(async move {
            let active = self.concurrency.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.concurrency.maximum.fetch_max(active, Ordering::SeqCst);
            self.calls.fetch_add(1, Ordering::SeqCst);
            let _active = ActiveWorkerCall(Arc::clone(&self.concurrency));
            self.release.notified().await;
            Ok(self.snapshot.clone())
        })
    }
}

#[derive(Default)]
struct TransientCountingClient {
    calls: AtomicUsize,
}

#[derive(Default)]
struct InvalidCredentialClient {
    calls: AtomicUsize,
    refresh_calls: AtomicUsize,
}

impl ControlClient for InvalidCredentialClient {
    fn refresh<'a>(&'a self, _: &'a DeviceCredential) -> ControlFuture<'a, DeviceCredential> {
        self.refresh_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(ControlError::InvalidCredential) })
    }

    fn heartbeat_and_control<'a>(
        &'a self,
        _: &'a DeviceCredential,
        _: u64,
    ) -> ControlFuture<'a, AgentControlSnapshot> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(ControlError::InvalidCredential) })
    }
}

struct RefreshingClient {
    calls: AtomicUsize,
    refresh_calls: AtomicUsize,
    refreshed: DeviceCredential,
    snapshot: AgentControlSnapshot,
}

struct PanicOnceClient {
    calls: AtomicUsize,
    panic_once: AtomicBool,
    snapshot: AgentControlSnapshot,
}

impl ControlClient for TransientCountingClient {
    fn refresh<'a>(&'a self, _: &'a DeviceCredential) -> ControlFuture<'a, DeviceCredential> {
        Box::pin(async { Err(ControlError::Contract) })
    }

    fn heartbeat_and_control<'a>(
        &'a self,
        _: &'a DeviceCredential,
        _: u64,
    ) -> ControlFuture<'a, AgentControlSnapshot> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(ControlError::Transient) })
    }
}

impl ControlClient for RefreshingClient {
    fn refresh<'a>(&'a self, _: &'a DeviceCredential) -> ControlFuture<'a, DeviceCredential> {
        self.refresh_calls.fetch_add(1, Ordering::SeqCst);
        let refreshed = self.refreshed.clone();
        Box::pin(async move { Ok(refreshed) })
    }

    fn heartbeat_and_control<'a>(
        &'a self,
        _: &'a DeviceCredential,
        _: u64,
    ) -> ControlFuture<'a, AgentControlSnapshot> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let snapshot = self.snapshot.clone();
        Box::pin(async move {
            if call == 0 {
                Err(ControlError::InvalidCredential)
            } else {
                Ok(snapshot)
            }
        })
    }
}

impl ControlClient for PanicOnceClient {
    fn refresh<'a>(&'a self, _: &'a DeviceCredential) -> ControlFuture<'a, DeviceCredential> {
        Box::pin(async { Err(ControlError::Contract) })
    }

    fn heartbeat_and_control<'a>(
        &'a self,
        _: &'a DeviceCredential,
        _: u64,
    ) -> ControlFuture<'a, AgentControlSnapshot> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let should_panic = self.panic_once.swap(false, Ordering::SeqCst);
        let snapshot = self.snapshot.clone();
        Box::pin(async move {
            assert!(!should_panic, "injected Cloud worker panic");
            Ok(snapshot)
        })
    }
}

impl ControlClient for BlockingSyncClient {
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

    fn sync_system_events<'a>(
        &'a self,
        _: &'a DeviceCredential,
        _: &'a [EventEnvelope],
    ) -> ControlFuture<'a, pca_agentd::cloud_control::SyncEventsResponse> {
        Box::pin(async move {
            self.sync_entered.notify_one();
            self.sync_release.notified().await;
            Err(ControlError::Transient)
        })
    }
}

impl ControlClient for BlockingMediaClient {
    fn refresh<'a>(&'a self, _: &'a DeviceCredential) -> ControlFuture<'a, DeviceCredential> {
        Box::pin(async { Err(ControlError::Contract) })
    }

    fn heartbeat_and_control<'a>(
        &'a self,
        _: &'a DeviceCredential,
        _: u64,
    ) -> ControlFuture<'a, AgentControlSnapshot> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let snapshot = self.snapshot.clone();
        Box::pin(async move { Ok(snapshot) })
    }

    fn sync_communication_attachment<'a>(
        &'a self,
        _: &'a DeviceCredential,
        _: &'a pca_db_local::PendingCommunicationAttachment,
    ) -> MediaControlFuture<'a> {
        Box::pin(async move {
            self.media_entered.notify_one();
            self.media_release.notified().await;
            Err(MediaUploadFailure::new(
                MediaUploadFailureStage::Client,
                ControlError::Transient,
            ))
        })
    }
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

impl ControlClient for NetworkStateClient {
    fn set_network_enabled(&self, enabled: bool) {
        self.network_enabled.store(enabled, Ordering::SeqCst);
    }

    fn refresh<'a>(&'a self, _: &'a DeviceCredential) -> ControlFuture<'a, DeviceCredential> {
        Box::pin(async { Err(ControlError::Contract) })
    }

    fn heartbeat_and_control<'a>(
        &'a self,
        _: &'a DeviceCredential,
        _: u64,
    ) -> ControlFuture<'a, AgentControlSnapshot> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let snapshot = self.snapshot.clone();
        Box::pin(async move { Ok(snapshot) })
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
    for collector_key in ["network", "communication.wechat", "screen.capture"] {
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
            ("screen.capture", CollectorStatus::Disabled),
        ]
    );
}

async fn await_unpaired(
    runtime: &pca_agentd::cloud_control::CloudControlHandle,
    pairing_state: &mut watch::Receiver<bool>,
) {
    while *pairing_state.borrow_and_update() {
        pairing_state
            .changed()
            .await
            .expect("Cloud control retains the pairing-state sender");
    }
    assert!(runtime.is_unpaired().await);
}

fn prevent_virtual_time_auto_advance() -> tokio::task::JoinHandle<()> {
    tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    })
}

#[tokio::test(start_paused = true)]
async fn owner_revocation_marks_manual_unpair_and_disables_sensitive_collectors() {
    let time_guard = prevent_virtual_time_auto_advance();
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
    await_unpaired(&runtime, &mut pairing_state_receiver).await;

    assert!(database
        .load_pairing_state()
        .await
        .unwrap()
        .is_some_and(|state| state.manually_unpaired));
    assert!(runtime.is_unpaired().await);
    assert_eq!(runtime.applied_revision().await, None);
    assert!(store
        .load(DEVICE_CREDENTIAL_SERVICE, DEVICE_CREDENTIAL_ACCOUNT)
        .unwrap()
        .is_none());
    runtime.shutdown().await.unwrap();
    time_guard.abort();
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
async fn invalid_credentials_never_change_the_manual_pairing_decision() {
    let time_guard = prevent_virtual_time_auto_advance();
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
    let client = Arc::new(InvalidCredentialClient::default());
    let runtime = CloudControlRuntime::start(
        Arc::clone(&database),
        credentials,
        Arc::clone(&client) as Arc<dyn ControlClient>,
    )
    .await
    .unwrap();

    wait_for_calls(&client.calls, 1).await;
    for _ in 0..100 {
        if client.refresh_calls.load(Ordering::SeqCst) >= 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(client.refresh_calls.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(30)).await;
    wait_for_calls(&client.calls, 2).await;

    assert!(client.calls.load(Ordering::SeqCst) >= 2);
    for _ in 0..100 {
        if client.refresh_calls.load(Ordering::SeqCst) >= 2 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(client.refresh_calls.load(Ordering::SeqCst) >= 2);
    assert!(!runtime.is_unpaired().await);
    assert!(database
        .load_pairing_state()
        .await
        .unwrap()
        .is_some_and(|state| state.is_paired()));
    assert!(store
        .load(DEVICE_CREDENTIAL_SERVICE, DEVICE_CREDENTIAL_ACCOUNT)
        .unwrap()
        .is_some());
    assert!(database
        .load_collector_states()
        .await
        .unwrap()
        .iter()
        .all(|state| state.status == CollectorStatus::Running));

    runtime.shutdown().await.unwrap();
    time_guard.abort();
    match Arc::try_unwrap(database) {
        Ok(database) => database.shutdown().await.unwrap(),
        Err(error) => {
            drop(error);
            panic!("runtime released database after invalid credential retry");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn revocation_notifies_the_agent_runtime_after_local_cleanup() {
    let time_guard = prevent_virtual_time_auto_advance();
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
    await_unpaired(&runtime, &mut pairing_state_receiver).await;

    assert!(!*pairing_state_receiver.borrow_and_update());
    assert!(database
        .load_pairing_state()
        .await
        .unwrap()
        .is_some_and(|state| state.manually_unpaired));
    assert_sensitive_disabled(&database).await;
    runtime.shutdown().await.unwrap();
    time_guard.abort();
    match Arc::try_unwrap(database) {
        Ok(database) => database.shutdown().await.unwrap(),
        Err(error) => {
            drop(error);
            panic!("runtime released database after shutdown");
        }
    }
}

#[tokio::test]
async fn corrupt_startup_credential_preserves_pairing_and_sensitive_collectors() {
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

    let (pairing_state_sender, mut pairing_state_receiver) = watch::channel(false);
    let runtime = CloudControlRuntime::start_from_keychain_with_pairing_state(
        Arc::clone(&database),
        store,
        Arc::new(RevokedClient),
        pairing_state_sender,
    )
    .await
    .unwrap();

    assert!(runtime.is_none());
    assert!(*pairing_state_receiver.borrow_and_update());
    assert!(database
        .load_pairing_state()
        .await
        .unwrap()
        .is_some_and(|state| state.is_paired()));
    assert!(database
        .load_collector_states()
        .await
        .unwrap()
        .iter()
        .all(|state| state.status == CollectorStatus::Running));
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
    let time_guard = prevent_virtual_time_auto_advance();
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

    let (pairing_state_sender, mut pairing_state_receiver) = watch::channel(false);
    let runtime = CloudControlRuntime::start_with_pairing_state(
        Arc::clone(&database),
        credentials,
        Arc::new(RevokedClient),
        pairing_state_sender,
    )
    .await
    .unwrap();
    await_unpaired(&runtime, &mut pairing_state_receiver).await;

    assert!(runtime.is_unpaired().await);
    assert!(database
        .load_pairing_state()
        .await
        .unwrap()
        .is_some_and(|state| state.manually_unpaired));
    assert_sensitive_disabled(&database).await;
    assert_eq!(store.delete_attempts.load(Ordering::Relaxed), 1);
    assert!(matches!(
        runtime.shutdown().await,
        Err(CloudControlRuntimeError::Keychain(
            CredentialError::OperationFailed
        ))
    ));
    time_guard.abort();
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
            "screen.capture": {
                "enabled": false,
                "scheduled_enabled": true,
                "interval_seconds": 300,
                "activity_enabled": true,
                "activity_min_interval_seconds": 30,
                "excluded_bundle_ids": []
            },
            "communication.wechat": {
                "enabled": true,
                "directions": ["incoming", "outgoing"],
                "message_types": ["text", "audio", "image", "video"],
                "conversation_scope": "direct_and_group_at_most_fifteen_members",
                "max_group_members": 15,
                "sync_mode": "full",
                "retention_days": 180
            },
            "communication.messages": { "enabled": false, "directions": ["incoming", "outgoing"], "message_types": ["text"], "conversation_scope": "all", "initial_lookback_days": 7, "sync_mode": "full", "attachments_enabled": false, "attachment_retention_days": 7 },
            "photos.library": { "enabled": false, "media_types": ["image", "video"], "include_originals": true, "include_album_names": true, "initial_lookback_days": 60, "cloud_retention": "permanent" }
        }
    });
    let snapshot: AgentControlSnapshot =
        serde_json::from_value(exact.clone()).expect("exact v2 control parses");
    let applied = apply_snapshot(6, &snapshot)
        .expect("exact v2 scope validates")
        .expect("new revision applies");
    assert_eq!(applied.configuration_revision, 7);
    assert!(applied.communication_wechat_enabled);
    assert_eq!(applied.screen_capture.interval_seconds, 300);
    assert!(apply_snapshot(7, &snapshot).unwrap().is_none());

    let mut rollout_compatible = exact.clone();
    rollout_compatible["collectors"]["photos.library"]["initial_lookback_days"] =
        serde_json::json!(7);
    let rollout_snapshot: AgentControlSnapshot = serde_json::from_value(rollout_compatible)
        .expect("legacy Photos lookback remains rollout-compatible");
    assert!(apply_snapshot(6, &rollout_snapshot).is_ok());

    let mut invalid_photos = exact.clone();
    invalid_photos["collectors"]["photos.library"]["initial_lookback_days"] = serde_json::json!(61);
    let invalid_photos_snapshot: AgentControlSnapshot =
        serde_json::from_value(invalid_photos).expect("shape remains parseable");
    assert!(apply_snapshot(6, &invalid_photos_snapshot).is_err());

    let mut legacy = exact.clone();
    legacy["collectors"]
        .as_object_mut()
        .unwrap()
        .remove("communication.messages");
    legacy["collectors"]
        .as_object_mut()
        .unwrap()
        .remove("photos.library");
    let legacy_snapshot: AgentControlSnapshot =
        serde_json::from_value(legacy).expect("legacy Cloud control remains rollout-compatible");
    let legacy_applied = apply_snapshot(6, &legacy_snapshot)
        .expect("legacy Cloud scope validates with disabled additions")
        .expect("legacy revision applies");
    assert!(!legacy_applied.communication_messages_enabled);
    assert!(!legacy_applied.photos_library_enabled);

    for (field, invalid) in [
        ("retention_days", serde_json::json!(7)),
        ("max_group_members", serde_json::json!(16)),
        ("sync_mode", serde_json::json!("metadata_only")),
        ("directions", serde_json::json!(["outgoing"])),
        ("message_types", serde_json::json!(["text"])),
    ] {
        let mut malformed = exact.clone();
        malformed["collectors"]["communication.wechat"][field] = invalid;
        assert!(serde_json::from_value::<AgentControlSnapshot>(malformed).is_err());
    }

    for (field, invalid) in [
        ("interval_seconds", serde_json::json!(59)),
        ("activity_min_interval_seconds", serde_json::json!(3601)),
        ("excluded_bundle_ids", serde_json::json!(["invalid/bundle"])),
    ] {
        let mut malformed = exact.clone();
        malformed["collectors"]["screen.capture"][field] = invalid;
        let parsed = serde_json::from_value::<AgentControlSnapshot>(malformed).unwrap();
        assert!(apply_snapshot(6, &parsed).is_err());
    }
}

#[tokio::test(start_paused = true)]
async fn communication_revision_notifications_are_monotonic_and_invalid_control_keeps_last_good() {
    let _time_guard = prevent_virtual_time_auto_advance();
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
        .clone()
        .expect("valid revision is notified");
    assert_eq!(first.configuration_revision, 1);
    assert!(first.communication_wechat_enabled);
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }

    tokio::time::advance(std::time::Duration::from_secs(30)).await;
    wait_for_calls(&client.calls, 2).await;
    assert!(!controls.has_changed().unwrap());
    assert_eq!(controls.borrow().as_ref(), Some(&first));
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }

    for _ in 0..3 {
        if client.calls.load(Ordering::SeqCst) >= 3 {
            break;
        }
        tokio::time::advance(std::time::Duration::from_secs(30)).await;
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
    }
    wait_for_calls(&client.calls, 3).await;
    assert!(!controls.has_changed().unwrap());
    assert_eq!(controls.borrow().as_ref(), Some(&first));

    runtime.shutdown().await.unwrap();
    match Arc::try_unwrap(database) {
        Ok(database) => database.shutdown().await.unwrap(),
        Err(error) => {
            drop(error);
            panic!("control runtime released database after shutdown");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn same_enabled_revision_remains_active_across_a_contract_failure() {
    let _time_guard = prevent_virtual_time_auto_advance();
    let (_temp, database) = db().await;
    let store = Arc::new(MemoryStore::default());
    let loaded = credentials(Arc::clone(&store));
    let enabled = exact_snapshot(5, true);
    let client = Arc::new(SequenceClient {
        calls: AtomicUsize::new(0),
        snapshots: Mutex::new(VecDeque::from([
            Ok(enabled.clone()),
            Err(ControlError::Contract),
            Ok(enabled),
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

    controls.changed().await.unwrap();
    assert!(controls.borrow_and_update().is_some());
    tokio::time::advance(Duration::from_secs(30)).await;
    wait_for_calls(&client.calls, 2).await;
    assert!(!controls.has_changed().unwrap());
    assert!(controls.borrow().is_some());

    for _ in 0..10 {
        if client.calls.load(Ordering::SeqCst) >= 3 {
            break;
        }
        tokio::time::advance(Duration::from_secs(30)).await;
        tokio::task::yield_now().await;
    }
    wait_for_calls(&client.calls, 3).await;
    let restored = controls
        .borrow()
        .clone()
        .expect("same enabled revision stays active");
    assert_eq!(restored.configuration_revision, 5);
    assert!(restored.communication_wechat_enabled);

    runtime.shutdown().await.unwrap();
    shutdown_database(database).await;
}

#[tokio::test(start_paused = true)]
async fn same_revision_reasserts_network_enabled_after_in_memory_state_drift() {
    let _time_guard = prevent_virtual_time_auto_advance();
    let (_temp, database) = db().await;
    let store = Arc::new(MemoryStore::default());
    let loaded = credentials(Arc::clone(&store));
    let mut snapshot = exact_snapshot(5, false);
    snapshot.collectors.network.enabled = true;
    let client = Arc::new(NetworkStateClient {
        calls: AtomicUsize::new(0),
        network_enabled: AtomicBool::new(false),
        snapshot,
    });
    let runtime = CloudControlRuntime::start(
        Arc::clone(&database),
        loaded,
        Arc::clone(&client) as Arc<dyn ControlClient>,
    )
    .await
    .unwrap();

    wait_for_calls(&client.calls, 1).await;
    wait_for_enabled(&client.network_enabled).await;
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    client.network_enabled.store(false, Ordering::SeqCst);

    for _ in 0..3 {
        if client.calls.load(Ordering::SeqCst) >= 2 {
            break;
        }
        tokio::time::advance(Duration::from_secs(30)).await;
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
    }
    wait_for_calls(&client.calls, 2).await;
    wait_for_enabled(&client.network_enabled).await;

    runtime.shutdown().await.unwrap();
    shutdown_database(database).await;
}

#[tokio::test(start_paused = true)]
async fn persisted_enabled_revision_is_restored_even_when_published_before_subscription() {
    let _time_guard = prevent_virtual_time_auto_advance();
    let (_temp, database) = db().await;
    let store = Arc::new(MemoryStore::default());
    let loaded = credentials(Arc::clone(&store));
    database
        .save_pairing_state(&PairingState::paired(
            loaded.credential().device_id(),
            loaded.credential().workspace_id(),
            "keychain://pca/device/current",
            7,
            "https://pca-cloud-api-production.up.railway.app",
        ))
        .await
        .unwrap();
    database.save_control_revision(7).await.unwrap();
    database
        .upsert_collector_state(&CollectorState {
            collector_key: "network".to_owned(),
            collector_version: "old".to_owned(),
            status: CollectorStatus::Disabled,
            desired_config_revision: 0,
            applied_config_revision: 0,
            last_event_at_ms: None,
            last_health_at_ms: None,
            last_error_code: None,
            created_at_ms: 1,
            updated_at_ms: 1,
        })
        .await
        .unwrap();
    let mut snapshot = exact_snapshot(7, true);
    snapshot.collectors.network.enabled = true;
    let client = Arc::new(SequenceClient {
        calls: AtomicUsize::new(0),
        snapshots: Mutex::new(VecDeque::from([Ok(snapshot)])),
    });
    let runtime = CloudControlRuntime::start(
        Arc::clone(&database),
        loaded,
        Arc::clone(&client) as Arc<dyn ControlClient>,
    )
    .await
    .unwrap();

    wait_for_calls(&client.calls, 1).await;
    let mut published = false;
    for _ in 0..10_000 {
        if runtime.communication_controls().borrow().is_some() {
            published = true;
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(published, "persisted revision was not published");
    let controls = runtime.communication_controls();
    let restored = controls
        .borrow()
        .as_ref()
        .cloned()
        .expect("equal persisted revision is published for restart hydration");
    assert_eq!(restored.configuration_revision, 7);
    assert!(restored.communication_wechat_enabled);
    let network = database
        .load_collector_states()
        .await
        .unwrap()
        .into_iter()
        .find(|state| state.collector_key == "network")
        .expect("network state is persisted");
    assert_eq!(network.status, CollectorStatus::Running);
    assert_eq!(network.desired_config_revision, 7);
    assert_eq!(network.applied_config_revision, 7);

    runtime.shutdown().await.unwrap();
    match Arc::try_unwrap(database) {
        Ok(database) => database.shutdown().await.unwrap(),
        Err(error) => {
            drop(error);
            panic!("control runtime released database after shutdown");
        }
    }
}

#[tokio::test]
async fn owner_restores_local_wechat_and_screenshot_control_without_cloud_credentials() {
    let (_temp, database) = db().await;
    let store = Arc::new(MemoryStore::default());
    let loaded = credentials(store);
    database
        .save_pairing_state(&PairingState::paired(
            loaded.credential().device_id(),
            loaded.credential().workspace_id(),
            "keychain://pca/device/current",
            7,
            "https://pca-cloud-api-production.up.railway.app",
        ))
        .await
        .unwrap();
    database
        .save_applied_collector_control(&AppliedCollectorControl {
            device_id: loaded.credential().device_id().to_owned(),
            workspace_id: loaded.credential().workspace_id().to_owned(),
            configuration_revision: 7,
            communication_wechat_enabled: true,
            screen_capture_enabled: true,
            screen_capture_scheduled_enabled: true,
            screen_capture_interval_seconds: 600,
            screen_capture_activity_enabled: false,
            screen_capture_activity_min_interval_seconds: 45,
            screen_capture_excluded_bundle_ids: vec!["com.example.private".to_owned()],
            updated_at_ms: 7,
        })
        .await
        .unwrap();

    let (pairing_state_sender, _) = watch::channel(true);
    let (owner, _commands) = CloudControlOwner::start(
        Arc::clone(&database),
        pairing_state_sender,
        CommunicationAuthorization::new(),
    );
    let mut controls = owner.communication_controls();
    if controls.borrow().is_none() {
        controls.changed().await.unwrap();
    }
    let restored = controls.borrow_and_update().clone().unwrap();
    assert_eq!(restored.configuration_revision, 7);
    assert!(restored.communication_wechat_enabled);
    assert!(restored.screen_capture.enabled);
    assert_eq!(restored.screen_capture.interval_seconds, 600);
    assert!(!restored.screen_capture.activity_enabled);
    assert_eq!(
        restored.screen_capture.excluded_bundle_ids,
        vec!["com.example.private"]
    );
    assert!(!restored.network_enabled);
    assert!(!restored.communication_messages_enabled);
    assert!(!restored.photos_library_enabled);
    assert_eq!(restored.screenshot_request_id, None);

    owner.shutdown().await.unwrap();
    shutdown_database(database).await;
}

#[tokio::test(start_paused = true)]
async fn persisted_screenshot_schedule_runs_without_a_cloud_worker() {
    let _time_guard = prevent_virtual_time_auto_advance();
    let (_temp, database) = db().await;
    let store = Arc::new(MemoryStore::default());
    let loaded = credentials(store);
    database
        .save_pairing_state(&PairingState::paired(
            loaded.credential().device_id(),
            loaded.credential().workspace_id(),
            "keychain://pca/device/current",
            7,
            "https://pca-cloud-api-production.up.railway.app",
        ))
        .await
        .unwrap();
    database
        .save_applied_collector_control(&AppliedCollectorControl {
            device_id: loaded.credential().device_id().to_owned(),
            workspace_id: loaded.credential().workspace_id().to_owned(),
            configuration_revision: 7,
            communication_wechat_enabled: true,
            screen_capture_enabled: true,
            screen_capture_scheduled_enabled: true,
            screen_capture_interval_seconds: 60,
            screen_capture_activity_enabled: false,
            screen_capture_activity_min_interval_seconds: 30,
            screen_capture_excluded_bundle_ids: Vec::new(),
            updated_at_ms: 7,
        })
        .await
        .unwrap();
    let (screen_capture, receiver) = screen_capture_command_channel();
    drop(receiver);
    let (pairing_state_sender, _) = watch::channel(true);
    let (owner, _commands) = CloudControlOwner::start_with_screen_capture(
        Arc::clone(&database),
        pairing_state_sender,
        CommunicationAuthorization::new(),
        screen_capture,
    );
    let mut controls = owner.communication_controls();
    if controls.borrow().is_none() {
        controls.changed().await.unwrap();
    }
    let startup_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if database
            .load_collector_states()
            .await
            .unwrap()
            .iter()
            .any(|state| state.collector_key == "screen.capture")
        {
            break;
        }
        assert!(
            Instant::now() < startup_deadline,
            "local screenshot loop did not initialize"
        );
        tokio::task::yield_now().await;
    }

    tokio::time::advance(Duration::from_secs(65)).await;
    let deadline = Instant::now() + Duration::from_secs(5);
    let state = loop {
        if let Some(state) = database
            .load_collector_states()
            .await
            .unwrap()
            .into_iter()
            .find(|state| state.collector_key == "screen.capture")
            .filter(|state| state.last_error_code.as_deref() == Some("SCREEN_CAPTURE_FAILED"))
        {
            break state;
        }
        assert!(
            Instant::now() < deadline,
            "local screenshot loop did not run"
        );
        tokio::task::yield_now().await;
    };
    assert_eq!(state.status, CollectorStatus::Degraded);
    assert_eq!(
        state.last_error_code.as_deref(),
        Some("SCREEN_CAPTURE_FAILED")
    );

    owner.shutdown().await.unwrap();
    shutdown_database(database).await;
}

#[tokio::test(start_paused = true)]
async fn valid_disable_applies_while_system_sync_is_blocked() {
    let _time_guard = prevent_virtual_time_auto_advance();
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
    database
        .append_event_with_outbox(&system_event("system-before-disable"))
        .await
        .unwrap();
    let client = Arc::new(BlockingSyncClient {
        calls: AtomicUsize::new(0),
        snapshots: Mutex::new(VecDeque::from([
            Ok(exact_snapshot(1, true)),
            Ok(exact_snapshot(2, false)),
        ])),
        sync_entered: Notify::new(),
        sync_release: Notify::new(),
    });
    let runtime = CloudControlRuntime::start(
        Arc::clone(&database),
        loaded,
        Arc::clone(&client) as Arc<dyn ControlClient>,
    )
    .await
    .unwrap();
    let mut controls = runtime.communication_controls();
    controls.changed().await.unwrap();
    assert!(controls
        .borrow_and_update()
        .as_ref()
        .is_some_and(|control| control.communication_wechat_enabled));

    let sync_entered = client.sync_entered.notified();
    tokio::pin!(sync_entered);
    sync_entered.as_mut().await;
    tokio::time::advance(Duration::from_secs(30)).await;
    wait_for_calls(&client.calls, 2).await;
    controls.changed().await.unwrap();

    assert!(controls
        .borrow_and_update()
        .as_ref()
        .is_some_and(|control| {
            control.configuration_revision == 2 && !control.communication_wechat_enabled
        }));

    client.sync_release.notify_waiters();
    runtime.shutdown().await.unwrap();
    match Arc::try_unwrap(database) {
        Ok(database) => database.shutdown().await.unwrap(),
        Err(error) => {
            drop(error);
            panic!("control runtime released database after shutdown");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn blocked_communication_upload_degrades_only_its_collectors_without_stopping_control() {
    let time_guard = prevent_virtual_time_auto_advance();
    let (temp, database) = db().await;
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
    seed_media_collectors(&database).await;

    let body = b"blocked media fixture";
    let sha256 = format!("{:x}", Sha256::digest(body));
    std::fs::write(temp.path().join("communication-spool").join(&sha256), body).unwrap();
    Connection::open(temp.path().join("agent.sqlite"))
        .unwrap()
        .execute_batch(&format!(
            "INSERT INTO events_local VALUES (
                'media-event', '22222222-2222-4222-8222-222222222222',
                '11111111-1111-4111-8111-111111111111',
                'communication.message_recorded', 'communication.wechat', 1, 1, 1,
                'high', '{{}}', '[]', NULL
             );
             INSERT INTO sync_outbox VALUES ('outbox-media', 'media-event', 'acked', 1);
             INSERT INTO communication_conversations VALUES (
                'account-1', 'conversation-1', 'direct', NULL, 1, 1
             );
             INSERT INTO communication_messages VALUES (
                1, 'media-event', 'account-1', 'conversation-1', 1, 'source-1',
                'incoming', 'image', 1, NULL, 1, 1
             );
             INSERT INTO attachment_spool (
                attachment_id, local_message_id, kind, sha256, size_bytes, mime_type,
                spool_relative_path, transfer_state, created_at_ms, completed_at_ms
             ) VALUES (
                'attachment-1', 1, 'image', '{sha256}', {size}, 'image/jpeg',
                '{sha256}', 'pending', 1, NULL
             );",
            size = body.len(),
        ))
        .unwrap();

    let client = Arc::new(BlockingMediaClient {
        calls: AtomicUsize::new(0),
        media_entered: Notify::new(),
        media_release: Notify::new(),
        snapshot: exact_snapshot(1, true),
    });
    let media_entered = client.media_entered.notified();
    tokio::pin!(media_entered);
    let runtime = CloudControlRuntime::start(
        Arc::clone(&database),
        loaded,
        Arc::clone(&client) as Arc<dyn ControlClient>,
    )
    .await
    .unwrap();
    let mut controls = runtime.communication_controls();

    media_entered.as_mut().await;
    wait_for_calls(&client.calls, 1).await;
    if controls.borrow().is_none() {
        controls.changed().await.unwrap();
    }
    assert!(controls.borrow_and_update().is_some());
    tokio::time::advance(Duration::from_secs(30)).await;
    wait_for_calls(&client.calls, 2).await;

    for _ in 0..89 {
        tokio::time::advance(Duration::from_secs(30)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }
    let calls_after_timeout = client.calls.load(Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(30)).await;
    wait_for_calls(&client.calls, calls_after_timeout + 1).await;
    for _ in 0..1_000 {
        if communication_media_cycle_timed_out(database.as_ref()).await {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_communication_media_cycle_timeout(&database).await;
    assert!(
        !runtime.is_finished(),
        "heartbeats must continue after a media cycle timeout"
    );

    runtime.shutdown().await.unwrap();
    time_guard.abort();
    match Arc::try_unwrap(database) {
        Ok(database) => database.shutdown().await.unwrap(),
        Err(error) => {
            drop(error);
            panic!("control and media workers released database after shutdown");
        }
    }
}

async fn seed_media_collectors(database: &DbActorHandle) {
    for collector_key in [
        "communication.wechat",
        "communication.messages",
        "photos.library",
    ] {
        database
            .upsert_collector_state(&CollectorState {
                collector_key: collector_key.to_owned(),
                collector_version: "test".to_owned(),
                status: CollectorStatus::Running,
                desired_config_revision: 1,
                applied_config_revision: 1,
                last_event_at_ms: None,
                last_health_at_ms: Some(1),
                last_error_code: None,
                created_at_ms: 1,
                updated_at_ms: 1,
            })
            .await
            .unwrap();
    }
}

async fn assert_communication_media_cycle_timeout(database: &DbActorHandle) {
    let states = database.load_collector_states().await.unwrap();
    for collector_key in ["communication.wechat", "communication.messages"] {
        let state = states
            .iter()
            .find(|state| state.collector_key == collector_key)
            .unwrap();
        assert_eq!(
            state.status,
            CollectorStatus::Degraded,
            "{collector_key} must remain degraded after media cycle timeout"
        );
        assert_eq!(
            state.last_error_code.as_deref(),
            Some("MEDIA_CYCLE_TIMEOUT"),
            "{collector_key} must retain the media cycle timeout error"
        );
    }
    let photos = states
        .iter()
        .find(|state| state.collector_key == "photos.library")
        .unwrap();
    assert_eq!(photos.status, CollectorStatus::Running);
    assert_eq!(photos.last_error_code, None);
}

async fn communication_media_cycle_timed_out(database: &DbActorHandle) -> bool {
    let states = database.load_collector_states().await.unwrap();
    ["communication.wechat", "communication.messages"]
        .into_iter()
        .all(|collector_key| {
            states.iter().any(|state| {
                state.collector_key == collector_key
                    && state.status == CollectorStatus::Degraded
                    && state.last_error_code.as_deref() == Some("MEDIA_CYCLE_TIMEOUT")
            })
        })
}

#[tokio::test(start_paused = true)]
async fn refreshed_credential_persistence_retries_without_stopping_cloud_control() {
    let _time_guard = prevent_virtual_time_auto_advance();
    let (_temp, database) = db().await;
    let store = Arc::new(MemoryStore::default());
    let loaded = credentials(Arc::clone(&store));
    store
        .store(
            DEVICE_CREDENTIAL_SERVICE,
            DEVICE_CREDENTIAL_ACCOUNT,
            &loaded.credential().encode().unwrap(),
        )
        .unwrap();
    let initial_store_attempts = store.store_attempts.load(Ordering::SeqCst);
    store.store_failures_remaining.store(2, Ordering::SeqCst);
    let refreshed = DeviceCredential::new(
        loaded.credential().device_id().to_owned(),
        loaded.credential().workspace_id().to_owned(),
        "refreshed-access",
        "refreshed-refresh",
    )
    .unwrap()
    .with_metadata(2, 1_800_000_000_000, 1_900_000_000_000);
    let client = Arc::new(RefreshingClient {
        calls: AtomicUsize::new(0),
        refresh_calls: AtomicUsize::new(0),
        refreshed,
        snapshot: exact_snapshot(1, true),
    });

    let runtime = CloudControlRuntime::start(
        Arc::clone(&database),
        loaded,
        Arc::clone(&client) as Arc<dyn ControlClient>,
    )
    .await
    .unwrap();
    wait_for_calls(&client.calls, 1).await;
    wait_for_calls(&store.store_attempts, initial_store_attempts + 1).await;
    tokio::time::advance(Duration::from_secs(2)).await;
    wait_for_calls(&store.store_attempts, initial_store_attempts + 2).await;
    tokio::time::advance(Duration::from_secs(2)).await;
    wait_for_calls(&store.store_attempts, initial_store_attempts + 3).await;
    wait_for_calls(&client.calls, 2).await;

    assert_eq!(client.refresh_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        store.store_attempts.load(Ordering::SeqCst),
        initial_store_attempts + 3
    );
    assert_eq!(
        load_device_credential(store.as_ref())
            .unwrap()
            .unwrap()
            .credential_generation(),
        2
    );

    runtime.shutdown().await.unwrap();
    shutdown_database(database).await;
}

#[tokio::test(start_paused = true)]
async fn permanently_unwritable_refreshed_credential_stops_cloud_control_after_deadline() {
    let time_guard = prevent_virtual_time_auto_advance();
    let (_temp, database) = db().await;
    let store = Arc::new(MemoryStore::default());
    let loaded = credentials(Arc::clone(&store));
    store
        .store(
            DEVICE_CREDENTIAL_SERVICE,
            DEVICE_CREDENTIAL_ACCOUNT,
            &loaded.credential().encode().unwrap(),
        )
        .unwrap();
    let initial_store_attempts = store.store_attempts.load(Ordering::SeqCst);
    store
        .store_failures_remaining
        .store(usize::MAX, Ordering::SeqCst);
    let refreshed = DeviceCredential::new(
        loaded.credential().device_id().to_owned(),
        loaded.credential().workspace_id().to_owned(),
        "refreshed-access",
        "refreshed-refresh",
    )
    .unwrap()
    .with_metadata(2, 1_800_000_000_000, 1_900_000_000_000);
    let client = Arc::new(RefreshingClient {
        calls: AtomicUsize::new(0),
        refresh_calls: AtomicUsize::new(0),
        refreshed,
        snapshot: exact_snapshot(1, true),
    });

    let runtime = CloudControlRuntime::start(
        Arc::clone(&database),
        loaded,
        Arc::clone(&client) as Arc<dyn ControlClient>,
    )
    .await
    .unwrap();
    wait_for_calls(&client.calls, 1).await;
    wait_for_calls(&store.store_attempts, initial_store_attempts + 1).await;
    tokio::time::advance(Duration::from_mins(5)).await;
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }

    assert!(matches!(
        runtime.shutdown().await,
        Err(CloudControlRuntimeError::WorkerStopped)
    ));
    time_guard.abort();
    shutdown_database(database).await;
}

#[tokio::test]
async fn cloud_owner_propagates_a_finished_worker_failure_for_agent_restart() {
    let (_temp, database) = db().await;
    let store = Arc::new(MemoryStore::default());
    let loaded = credentials(Arc::clone(&store));
    store
        .store(
            DEVICE_CREDENTIAL_SERVICE,
            DEVICE_CREDENTIAL_ACCOUNT,
            &loaded.credential().encode().unwrap(),
        )
        .unwrap();
    let client = Arc::new(PanicOnceClient {
        calls: AtomicUsize::new(0),
        panic_once: AtomicBool::new(true),
        snapshot: exact_snapshot(1, true),
    });
    let (pairing_state_sender, _) = watch::channel(false);
    let (owner, commands) = CloudControlOwner::start(
        Arc::clone(&database),
        pairing_state_sender,
        CommunicationAuthorization::new(),
    );

    assert!(commands
        .replace_from_keychain(
            Arc::clone(&store) as Arc<dyn CredentialStore>,
            Arc::clone(&client) as Arc<dyn ControlClient>,
        )
        .await
        .unwrap());
    wait_for_calls(&client.calls, 1).await;
    for _ in 0..200 {
        if owner.is_finished() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(owner.is_finished());
    assert!(matches!(
        commands
            .replace_from_keychain(
                Arc::clone(&store) as Arc<dyn CredentialStore>,
                Arc::clone(&client) as Arc<dyn ControlClient>,
            )
            .await,
        Err(CloudControlRuntimeError::WorkerStopped)
    ));

    assert!(matches!(
        owner.shutdown().await,
        Err(CloudControlRuntimeError::WorkerStopped)
    ));
    shutdown_database(database).await;
}

#[tokio::test]
async fn cloud_owner_retries_keychain_reconciliation_without_replacing_a_live_worker() {
    let (_temp, database) = db().await;
    let store = Arc::new(MemoryStore::default());
    let loaded = credentials(Arc::clone(&store));
    store
        .store(
            DEVICE_CREDENTIAL_SERVICE,
            DEVICE_CREDENTIAL_ACCOUNT,
            &loaded.credential().encode().unwrap(),
        )
        .unwrap();
    store.load_failures_remaining.store(1, Ordering::SeqCst);
    let concurrency = Arc::new(WorkerConcurrency::default());
    let client = Arc::new(BlockingOwnerClient {
        calls: AtomicUsize::new(0),
        release: Notify::new(),
        concurrency: Arc::clone(&concurrency),
        snapshot: exact_snapshot(1, true),
    });
    let (pairing_state_sender, _) = watch::channel(false);
    let (owner, commands) = CloudControlOwner::start(
        Arc::clone(&database),
        pairing_state_sender,
        CommunicationAuthorization::new(),
    );

    assert!(matches!(
        commands
            .replace_from_keychain(
                Arc::clone(&store) as Arc<dyn CredentialStore>,
                Arc::clone(&client) as Arc<dyn ControlClient>,
            )
            .await,
        Err(CloudControlRuntimeError::Keychain(
            CredentialError::Unavailable
        ))
    ));
    assert!(commands
        .replace_from_keychain(
            Arc::clone(&store) as Arc<dyn CredentialStore>,
            Arc::clone(&client) as Arc<dyn ControlClient>,
        )
        .await
        .unwrap());
    wait_for_calls(&client.calls, 1).await;
    assert!(commands
        .replace_from_keychain(
            Arc::clone(&store) as Arc<dyn CredentialStore>,
            Arc::clone(&client) as Arc<dyn ControlClient>,
        )
        .await
        .unwrap());
    assert_eq!(client.calls.load(Ordering::SeqCst), 1);
    assert_eq!(concurrency.maximum.load(Ordering::SeqCst), 1);

    client.release.notify_waiters();
    owner.shutdown().await.unwrap();
    shutdown_database(database).await;
}

#[tokio::test]
async fn cloud_owner_joins_startup_worker_before_pairing_replacement_starts() {
    let (_temp, database) = db().await;
    let store = Arc::new(MemoryStore::default());
    let loaded = credentials(Arc::clone(&store));
    let concurrency = Arc::new(WorkerConcurrency::default());
    let startup = Arc::new(BlockingOwnerClient {
        calls: AtomicUsize::new(0),
        release: Notify::new(),
        concurrency: Arc::clone(&concurrency),
        snapshot: exact_snapshot(1, true),
    });
    let pairing = Arc::new(BlockingOwnerClient {
        calls: AtomicUsize::new(0),
        release: Notify::new(),
        concurrency: Arc::clone(&concurrency),
        snapshot: exact_snapshot(2, true),
    });
    let (pairing_state_sender, _) = watch::channel(false);
    let (owner, commands) = CloudControlOwner::start(
        Arc::clone(&database),
        pairing_state_sender,
        CommunicationAuthorization::new(),
    );

    commands
        .replace_identity(
            loaded.clone(),
            Arc::clone(&startup) as Arc<dyn ControlClient>,
        )
        .await
        .unwrap();
    wait_for_calls(&startup.calls, 1).await;
    let replace = tokio::spawn({
        let commands = commands.clone();
        let loaded = loaded.clone();
        let pairing = Arc::clone(&pairing);
        async move {
            commands
                .replace_identity(loaded, pairing as Arc<dyn ControlClient>)
                .await
        }
    });
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert_eq!(pairing.calls.load(Ordering::SeqCst), 0);

    startup.release.notify_waiters();
    replace.await.unwrap().unwrap();
    wait_for_calls(&pairing.calls, 1).await;
    assert_eq!(concurrency.maximum.load(Ordering::SeqCst), 1);

    pairing.release.notify_waiters();
    owner.shutdown().await.unwrap();
    shutdown_database(database).await;
}

#[tokio::test]
async fn cloud_owner_repeated_pairing_replacement_never_overlaps_workers() {
    let (_temp, database) = db().await;
    let store = Arc::new(MemoryStore::default());
    let loaded = credentials(Arc::clone(&store));
    let concurrency = Arc::new(WorkerConcurrency::default());
    let clients = (1..=3)
        .map(|revision| {
            Arc::new(BlockingOwnerClient {
                calls: AtomicUsize::new(0),
                release: Notify::new(),
                concurrency: Arc::clone(&concurrency),
                snapshot: exact_snapshot(revision, true),
            })
        })
        .collect::<Vec<_>>();
    let (pairing_state_sender, _) = watch::channel(false);
    let (owner, commands) = CloudControlOwner::start(
        Arc::clone(&database),
        pairing_state_sender,
        CommunicationAuthorization::new(),
    );

    commands
        .replace_identity(
            loaded.clone(),
            Arc::clone(&clients[0]) as Arc<dyn ControlClient>,
        )
        .await
        .unwrap();
    wait_for_calls(&clients[0].calls, 1).await;
    for index in 1..clients.len() {
        let replace = tokio::spawn({
            let commands = commands.clone();
            let loaded = loaded.clone();
            let client = Arc::clone(&clients[index]);
            async move {
                commands
                    .replace_identity(loaded, client as Arc<dyn ControlClient>)
                    .await
            }
        });
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert_eq!(clients[index].calls.load(Ordering::SeqCst), 0);
        clients[index - 1].release.notify_waiters();
        replace.await.unwrap().unwrap();
        wait_for_calls(&clients[index].calls, 1).await;
        assert_eq!(concurrency.maximum.load(Ordering::SeqCst), 1);
    }

    clients[2].release.notify_waiters();
    owner.shutdown().await.unwrap();
    shutdown_database(database).await;
}

#[tokio::test]
async fn cloud_owner_stale_worker_cannot_publish_or_reauthorize_after_epoch_handoff() {
    let (temp, database) = db().await;
    let store = Arc::new(MemoryStore::default());
    let loaded = credentials(Arc::clone(&store));
    let authorization = CommunicationAuthorization::new();
    let communication = CommunicationRuntime::start_authorized(
        Arc::clone(&database),
        temp.path().join("agent.sqlite"),
        Arc::new(UnavailableCommunicationProviderFactory),
        authorization.clone(),
    )
    .await
    .unwrap();
    let concurrency = Arc::new(WorkerConcurrency::default());
    let old = Arc::new(BlockingOwnerClient {
        calls: AtomicUsize::new(0),
        release: Notify::new(),
        concurrency: Arc::clone(&concurrency),
        snapshot: exact_snapshot(1, true),
    });
    let replacement = Arc::new(BlockingOwnerClient {
        calls: AtomicUsize::new(0),
        release: Notify::new(),
        concurrency,
        snapshot: exact_snapshot(2, true),
    });
    let (pairing_state_sender, _) = watch::channel(false);
    let (owner, commands) =
        CloudControlOwner::start(Arc::clone(&database), pairing_state_sender, authorization);
    let mut controls = owner.communication_controls();
    commands
        .replace_identity(loaded.clone(), Arc::clone(&old) as Arc<dyn ControlClient>)
        .await
        .unwrap();
    wait_for_calls(&old.calls, 1).await;
    controls.borrow_and_update();
    let copied_old_control = CommunicationControl::paired(
        CommunicationIdentity::try_new(
            loaded.credential().workspace_id(),
            loaded.credential().device_id(),
        )
        .unwrap(),
        1,
        true,
    )
    .unwrap();

    let replace = tokio::spawn({
        let commands = commands.clone();
        let replacement = Arc::clone(&replacement);
        async move {
            commands
                .replace_identity(loaded, replacement as Arc<dyn ControlClient>)
                .await
        }
    });
    controls.changed().await.unwrap();
    assert!(controls.borrow_and_update().is_none());
    assert!(matches!(
        communication.apply_control(copied_old_control).await,
        Err(CommunicationRuntimeError::AuthorizationReadOnly)
    ));
    old.release.notify_waiters();
    replace.await.unwrap().unwrap();
    wait_for_calls(&replacement.calls, 1).await;
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert!(
        controls.borrow().is_none(),
        "stale worker publication escaped"
    );
    let communication_state = database
        .load_collector_states()
        .await
        .unwrap()
        .into_iter()
        .find(|state| state.collector_key == "communication.wechat")
        .unwrap();
    assert_eq!(communication_state.status, CollectorStatus::Disabled);

    replacement.release.notify_waiters();
    owner.shutdown().await.unwrap();
    communication.shutdown().await.unwrap();
    shutdown_database(database).await;
}

#[tokio::test(start_paused = true)]
async fn cloud_owner_closed_command_and_worker_shutdown_channels_terminate_without_busy_loop() {
    let _time_guard = prevent_virtual_time_auto_advance();
    let (_temp, database) = db().await;
    let store = Arc::new(MemoryStore::default());
    let loaded = credentials(Arc::clone(&store));
    let owner_client = Arc::new(TransientCountingClient::default());
    let (pairing_state_sender, _) = watch::channel(false);
    let (owner, commands) = CloudControlOwner::start(
        Arc::clone(&database),
        pairing_state_sender,
        CommunicationAuthorization::new(),
    );
    commands
        .replace_identity(
            loaded.clone(),
            Arc::clone(&owner_client) as Arc<dyn ControlClient>,
        )
        .await
        .unwrap();
    wait_for_calls(&owner_client.calls, 1).await;
    drop(commands);
    tokio::time::advance(Duration::from_hours(1)).await;
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert_eq!(owner_client.calls.load(Ordering::SeqCst), 1);
    owner.shutdown().await.unwrap();

    let worker_client = Arc::new(TransientCountingClient::default());
    let worker = CloudControlRuntime::start(
        Arc::clone(&database),
        loaded,
        Arc::clone(&worker_client) as Arc<dyn ControlClient>,
    )
    .await
    .unwrap();
    wait_for_calls(&worker_client.calls, 1).await;
    drop(worker);
    tokio::time::advance(Duration::from_hours(1)).await;
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert_eq!(worker_client.calls.load(Ordering::SeqCst), 1);
}

fn system_event(event_id: &str) -> EventEnvelope {
    EventEnvelope {
        event_id: event_id.to_owned(),
        workspace_id: "22222222-2222-4222-8222-222222222222".to_owned(),
        device_id: "11111111-1111-4111-8111-111111111111".to_owned(),
        event_type: "system.metric_sampled".to_owned(),
        source: "system".to_owned(),
        schema_version: 1,
        occurred_at: "2026-08-02T00:00:00Z".to_owned(),
        created_at: "2026-08-02T00:00:00Z".to_owned(),
        sensitivity: Sensitivity::Normal,
        payload: serde_json::Map::new(),
        attachment_refs: Vec::new(),
        idempotency_key: Some(format!("system:{event_id}")),
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
            "screen.capture": {
                "enabled": false,
                "scheduled_enabled": true,
                "interval_seconds": 300,
                "activity_enabled": true,
                "activity_min_interval_seconds": 30,
                "excluded_bundle_ids": []
            },
            "communication.wechat": {
                "enabled": enabled,
                "directions": ["incoming", "outgoing"],
                "message_types": ["text", "audio", "image", "video"],
                "conversation_scope": "direct_and_group_at_most_fifteen_members",
                "max_group_members": 15,
                "sync_mode": "full",
                "retention_days": 180
            },
            "communication.messages": { "enabled": false, "directions": ["incoming", "outgoing"], "message_types": ["text"], "conversation_scope": "all", "initial_lookback_days": 7, "sync_mode": "full", "attachments_enabled": false, "attachment_retention_days": 7 },
            "photos.library": { "enabled": false, "media_types": ["image", "video"], "include_originals": true, "include_album_names": true, "initial_lookback_days": 60, "cloud_retention": "permanent" }
        }
    }))
    .unwrap()
}

async fn wait_for_calls(calls: &AtomicUsize, expected: usize) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if calls.load(Ordering::SeqCst) >= expected {
            return;
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        tokio::task::yield_now().await;
    }
    panic!("control client did not reach {expected} calls");
}

async fn wait_for_enabled(enabled: &AtomicBool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if enabled.load(Ordering::SeqCst) {
            return;
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        tokio::task::yield_now().await;
    }
    panic!("an unchanged valid control snapshot did not repair Network state drift");
}

async fn shutdown_database(database: Arc<DbActorHandle>) {
    match Arc::try_unwrap(database) {
        Ok(database) => database.shutdown().await.unwrap(),
        Err(database) => {
            drop(database);
            panic!("runtime released database before test cleanup");
        }
    }
}
