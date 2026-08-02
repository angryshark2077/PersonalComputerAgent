use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use pca_agentd::cloud_control::{
    apply_snapshot, AgentControlSnapshot, CloudControlOwner, CloudControlRuntime,
    CloudControlRuntimeError, ControlClient, ControlError, ControlFuture,
};
use pca_agentd::communication::{
    CommunicationAuthorization, CommunicationControl, CommunicationIdentity, CommunicationRuntime,
    CommunicationRuntimeError, UnavailableCommunicationProviderFactory,
};
use pca_db_local::{DbActorHandle, PairingState};
use pca_domain::{CollectorState, CollectorStatus, EventEnvelope, Sensitivity};
use pca_keychain::{
    CredentialError, CredentialStore, DeviceCredential, DEVICE_CREDENTIAL_ACCOUNT,
    DEVICE_CREDENTIAL_SERVICE,
};
use tempfile::TempDir;
use tokio::sync::{watch, Notify};

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

struct BlockingSyncClient {
    calls: AtomicUsize,
    snapshots: Mutex<VecDeque<Result<AgentControlSnapshot, ControlError>>>,
    sync_entered: Notify,
    sync_release: Notify,
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
            self.sync_entered.notify_waiters();
            self.sync_release.notified().await;
            Err(ControlError::Transient)
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

#[tokio::test(start_paused = true)]
async fn persisted_enabled_revision_is_restored_even_when_published_before_subscription() {
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
    let client = Arc::new(SequenceClient {
        calls: AtomicUsize::new(0),
        snapshots: Mutex::new(VecDeque::from([Ok(exact_snapshot(7, true))])),
    });
    let runtime = CloudControlRuntime::start(
        Arc::clone(&database),
        loaded,
        Arc::clone(&client) as Arc<dyn ControlClient>,
    )
    .await
    .unwrap();

    wait_for_calls(&client.calls, 1).await;
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    let controls = runtime.communication_controls();
    let restored = controls
        .borrow()
        .as_ref()
        .copied()
        .expect("equal persisted revision is published for restart hydration");
    assert_eq!(restored.configuration_revision, 7);
    assert!(restored.communication_wechat_enabled);

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
async fn valid_disable_gates_communication_before_blocked_system_sync() {
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

    database
        .append_event_with_outbox(&system_event("system-before-disable"))
        .await
        .unwrap();
    let sync_entered = client.sync_entered.notified();
    tokio::pin!(sync_entered);
    tokio::time::advance(Duration::from_secs(30)).await;
    sync_entered.as_mut().await;

    assert!(controls.borrow().as_ref().is_some_and(|control| {
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

async fn shutdown_database(database: Arc<DbActorHandle>) {
    match Arc::try_unwrap(database) {
        Ok(database) => database.shutdown().await.unwrap(),
        Err(database) => {
            drop(database);
            panic!("runtime released database before test cleanup");
        }
    }
}
