use std::{collections::BTreeMap, sync::Mutex};

use pca_keychain::{
    delete_device_credential, load_device_credential, store_device_credential, CredentialError,
    CredentialStore, DeviceCredential, DEVICE_CREDENTIAL_ACCOUNT, DEVICE_CREDENTIAL_SERVICE,
};

#[derive(Default)]
struct MemoryStore(Mutex<BTreeMap<(String, String), Vec<u8>>>);

impl CredentialStore for MemoryStore {
    fn load(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, CredentialError> {
        Ok(self
            .0
            .lock()
            .map_err(|_| CredentialError::Unavailable)?
            .get(&(service.to_owned(), account.to_owned()))
            .cloned())
    }

    fn store(&self, service: &str, account: &str, secret: &[u8]) -> Result<(), CredentialError> {
        self.0
            .lock()
            .map_err(|_| CredentialError::Unavailable)?
            .insert((service.to_owned(), account.to_owned()), secret.to_vec());
        Ok(())
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), CredentialError> {
        self.0
            .lock()
            .map_err(|_| CredentialError::Unavailable)?
            .remove(&(service.to_owned(), account.to_owned()));
        Ok(())
    }
}

fn device_id() -> String {
    "11111111-1111-4111-8111-111111111111".to_owned()
}

fn workspace_id() -> String {
    "22222222-2222-4222-8222-222222222222".to_owned()
}

#[test]
fn device_credentials_reject_bridge_identity_and_missing_refresh_secret() {
    assert!(DeviceCredential::decode(b"not-json").is_err());
    assert!(DeviceCredential::new(device_id(), workspace_id(), "", "refresh").is_err());
    assert!(DeviceCredential::new(device_id(), workspace_id(), "access", "").is_err());
}

#[test]
fn device_credentials_use_the_versioned_identity_and_round_trip_without_bridge_overlap() {
    let store = MemoryStore::default();
    let credential = DeviceCredential::new(device_id(), workspace_id(), "access", "refresh")
        .expect("valid device credential")
        .with_metadata(7, 1_700_000_000_000, 1_800_000_000_000);

    store_device_credential(&store, &credential).expect("store device credential");

    assert!(store
        .load("com.pca.bridge", "shared-secret-v1")
        .expect("read bridge identity")
        .is_none());
    assert_eq!(
        store
            .load(DEVICE_CREDENTIAL_SERVICE, DEVICE_CREDENTIAL_ACCOUNT)
            .expect("read device identity")
            .expect("device record"),
        credential.encode().expect("encoded credential")
    );
    assert_eq!(load_device_credential(&store), Ok(Some(credential)));

    delete_device_credential(&store).expect("delete device credential");
    assert_eq!(load_device_credential(&store), Ok(None));
}

#[test]
fn device_credential_errors_do_not_include_secret_material() {
    let error =
        DeviceCredential::new(device_id(), workspace_id(), "access-secret", "").unwrap_err();
    assert!(!error.to_string().contains("access-secret"));
    assert!(!format!("{error:?}").contains("access-secret"));
}

#[test]
fn device_credential_debug_redacts_both_bearer_tokens() {
    let credential = DeviceCredential::new(
        device_id(),
        workspace_id(),
        "access-must-not-appear",
        "refresh-must-not-appear",
    )
    .expect("valid device credential");
    let debug = format!("{credential:?}");
    assert!(!debug.contains("access-must-not-appear"));
    assert!(!debug.contains("refresh-must-not-appear"));
    assert!(debug.contains("redacted"));
}
