use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
};

use pca_keychain::{
    delete_bridge_shared_secret, load_bridge_shared_secret, store_bridge_shared_secret,
    CredentialError, CredentialStore, BRIDGE_SHARED_SECRET_LENGTH,
};

type CredentialMap = BTreeMap<(String, String), Vec<u8>>;
type CredentialMapGuard<'a> = std::sync::MutexGuard<'a, CredentialMap>;

#[derive(Default)]
struct InMemoryCredentialStore {
    entries: Mutex<CredentialMap>,
    unavailable: AtomicBool,
}

impl InMemoryCredentialStore {
    fn unavailable() -> Self {
        Self {
            entries: Mutex::new(BTreeMap::new()),
            unavailable: AtomicBool::new(true),
        }
    }

    fn entries(&self) -> Result<CredentialMapGuard<'_>, CredentialError> {
        if self.unavailable.load(Ordering::Relaxed) {
            return Err(CredentialError::Unavailable);
        }

        self.entries
            .lock()
            .map_err(|_| CredentialError::Unavailable)
    }
}

impl CredentialStore for InMemoryCredentialStore {
    fn load(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, CredentialError> {
        Ok(self
            .entries()?
            .get(&(service.to_owned(), account.to_owned()))
            .cloned())
    }

    fn store(&self, service: &str, account: &str, secret: &[u8]) -> Result<(), CredentialError> {
        self.entries()?
            .insert((service.to_owned(), account.to_owned()), secret.to_vec());
        Ok(())
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), CredentialError> {
        self.entries()?
            .remove(&(service.to_owned(), account.to_owned()));
        Ok(())
    }
}

fn secret(byte: u8) -> [u8; BRIDGE_SHARED_SECRET_LENGTH] {
    [byte; BRIDGE_SHARED_SECRET_LENGTH]
}

#[test]
fn generic_store_creates_overwrites_reads_and_deletes_credentials() {
    let store = InMemoryCredentialStore::default();

    assert_eq!(store.load("test.service", "test-account"), Ok(None));

    store
        .store("test.service", "test-account", b"first")
        .expect("create credential");
    assert_eq!(
        store.load("test.service", "test-account"),
        Ok(Some(b"first".to_vec()))
    );

    store
        .store("test.service", "test-account", b"replacement")
        .expect("overwrite credential");
    assert_eq!(
        store.load("test.service", "test-account"),
        Ok(Some(b"replacement".to_vec()))
    );

    store
        .delete("test.service", "test-account")
        .expect("delete credential");
    store
        .delete("test.service", "test-account")
        .expect("delete remains idempotent");
    assert_eq!(store.load("test.service", "test-account"), Ok(None));
}

#[test]
fn generic_store_reports_unavailable_for_every_operation() {
    let store = InMemoryCredentialStore::unavailable();

    assert_eq!(
        store.load("test.service", "test-account"),
        Err(CredentialError::Unavailable)
    );
    assert_eq!(
        store.store("test.service", "test-account", b"secret"),
        Err(CredentialError::Unavailable)
    );
    assert_eq!(
        store.delete("test.service", "test-account"),
        Err(CredentialError::Unavailable)
    );
}

#[test]
fn bridge_shared_secret_uses_only_the_fixed_keychain_identity() {
    let store = InMemoryCredentialStore::default();
    let expected = secret(0x5a);

    store_bridge_shared_secret(&store, &expected).expect("store shared secret");

    assert_eq!(
        store.load("com.pca.bridge", "shared-secret-v1"),
        Ok(Some(expected.to_vec()))
    );
    assert_eq!(load_bridge_shared_secret(&store), Ok(Some(expected)));

    delete_bridge_shared_secret(&store).expect("delete shared secret");
    assert_eq!(load_bridge_shared_secret(&store), Ok(None));
}

#[test]
fn bridge_shared_secret_rejects_weak_store_input_without_writing_it() {
    let store = InMemoryCredentialStore::default();

    for invalid in [vec![0x11; 31], vec![0x22; 33]] {
        assert_eq!(
            store_bridge_shared_secret(&store, &invalid),
            Err(CredentialError::InvalidSecretLength)
        );
        assert_eq!(store.load("com.pca.bridge", "shared-secret-v1"), Ok(None));
    }
}

#[test]
fn bridge_shared_secret_rejects_corrupt_keychain_values_on_load() {
    for invalid in [vec![0x33; 31], vec![0x44; 33]] {
        let store = InMemoryCredentialStore::default();
        store
            .store("com.pca.bridge", "shared-secret-v1", &invalid)
            .expect("inject malformed credential");

        assert_eq!(
            load_bridge_shared_secret(&store),
            Err(CredentialError::CorruptSecret)
        );
    }
}

#[test]
fn credential_errors_never_embed_secret_or_query_values() {
    let sensitive_values = ["com.pca.bridge", "shared-secret-v1", "credential bytes"];

    for error in [
        CredentialError::Unavailable,
        CredentialError::InvalidSecretLength,
        CredentialError::CorruptSecret,
        CredentialError::OperationFailed,
    ] {
        let display = error.to_string();
        let debug = format!("{error:?}");

        for sensitive in sensitive_values {
            assert!(!display.contains(sensitive));
            assert!(!debug.contains(sensitive));
        }
    }
}

#[test]
fn production_credential_adapters_have_no_plaintext_fallback_channel() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let sources = [
        manifest.join("src/lib.rs"),
        manifest.join("src/macos.rs"),
        manifest.join("../../platform/macos/Sources/BridgeProtocol/KeychainCredentialStore.swift"),
    ];
    let forbidden = [
        ("std::fs", "Rust filesystem access"),
        ("tokio::fs", "async Rust filesystem access"),
        ("OpenOptions", "Rust file creation"),
        ("File::create", "Rust file creation"),
        ("Command::new", "Rust command arguments"),
        ("std::env", "Rust environment fallback"),
        ("FileManager", "Swift filesystem access"),
        (".write(to:", "Swift file write"),
        ("UserDefaults", "Swift defaults fallback"),
        (
            "ProcessInfo.processInfo.environment",
            "Swift environment fallback",
        ),
        ("Process()", "Swift command arguments"),
        ("\"Data\"", "persistent Data directory reference"),
        ("\"Run\"", "ephemeral Run directory reference"),
    ];

    assert_eq!(sources.len(), 3, "scan must cover both production adapters");
    for path in sources {
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to scan {}: {error}", path.display()));
        assert!(!source.is_empty(), "{} must not be empty", path.display());

        for (needle, channel) in forbidden {
            assert!(
                !source.contains(needle),
                "{} contains forbidden {channel}: {needle}",
                path.display()
            );
        }
    }
}
