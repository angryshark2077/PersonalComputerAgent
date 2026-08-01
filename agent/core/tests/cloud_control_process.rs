use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

use pca_agentd::cloud_control::{
    CloudControlRuntime, CloudControlRuntimeError, ControlClient, ControlError, ControlFuture,
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
