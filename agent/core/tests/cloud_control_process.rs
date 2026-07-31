use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use pca_agentd::cloud_control::{CloudControlRuntime, ControlClient, ControlError, ControlFuture};
use pca_db_local::{DbActorHandle, PairingState};
use pca_keychain::{CredentialError, CredentialStore, DeviceCredential, DEVICE_CREDENTIAL_ACCOUNT, DEVICE_CREDENTIAL_SERVICE};
use tempfile::TempDir;

#[derive(Default)]
struct MemoryStore(Mutex<BTreeMap<(String, String), Vec<u8>>>);

impl CredentialStore for MemoryStore {
    fn load(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, CredentialError> {
        Ok(self.0.lock().map_err(|_| CredentialError::Unavailable)?.get(&(service.to_owned(), account.to_owned())).cloned())
    }

    fn store(&self, service: &str, account: &str, value: &[u8]) -> Result<(), CredentialError> {
        self.0.lock().map_err(|_| CredentialError::Unavailable)?.insert((service.to_owned(), account.to_owned()), value.to_vec());
        Ok(())
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), CredentialError> {
        self.0.lock().map_err(|_| CredentialError::Unavailable)?.remove(&(service.to_owned(), account.to_owned()));
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
        "synthetic-access", "synthetic-refresh",
    ).unwrap().with_metadata(1, 1_700_000_000_000, 1_800_000_000_000);
    pca_agentd::cloud_control::LoadedDeviceCredentials::new(credential, store)
}

async fn db() -> (TempDir, Arc<DbActorHandle>) {
    let temp = TempDir::new().unwrap();
    let database = Arc::new(DbActorHandle::open(&temp.path().join("agent.sqlite"), "test").await.unwrap());
    (temp, database)
}

#[tokio::test(start_paused = true)]
async fn revocation_clears_pairing_and_disables_sensitive_collectors() {
    let (_temp, database) = db().await;
    let store = Arc::new(MemoryStore::default());
    let credentials = credentials(Arc::clone(&store));
    database.save_pairing_state(&PairingState::paired(
        credentials.credential().device_id(), credentials.credential().workspace_id(),
        "keychain://pca/device/current", 1,
    )).await.unwrap();
    store.store(DEVICE_CREDENTIAL_SERVICE, DEVICE_CREDENTIAL_ACCOUNT, &credentials.credential().encode().unwrap()).unwrap();

    let runtime = CloudControlRuntime::start(Arc::clone(&database), credentials, Arc::new(RevokedClient)).await.unwrap();
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(30)).await;
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }

    assert!(database.load_pairing_state().await.unwrap().is_none());
    assert!(runtime.is_unpaired().await);
    assert_eq!(runtime.applied_revision().await, None);
    assert!(store.load(DEVICE_CREDENTIAL_SERVICE, DEVICE_CREDENTIAL_ACCOUNT).unwrap().is_none());
    runtime.shutdown().await.unwrap();
    let states = database.load_collector_states().await.unwrap();
    assert_eq!(
        states
            .iter()
            .map(|state| (state.collector_key.as_str(), state.status))
            .collect::<Vec<_>>(),
        vec![
            ("communication.wechat", pca_domain::CollectorStatus::Disabled),
            ("network", pca_domain::CollectorStatus::Disabled),
        ]
    );
    match Arc::try_unwrap(database) {
        Ok(database) => database.shutdown().await.unwrap(),
        Err(_) => panic!("runtime released database after shutdown"),
    }
}
