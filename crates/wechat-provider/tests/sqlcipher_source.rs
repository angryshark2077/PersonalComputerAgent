use std::{
    collections::BTreeMap,
    fs::Metadata,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, SystemTime},
};

use pca_domain::DomainError;
use pca_keychain::{
    CredentialError, CredentialStore, WechatKeyMaterial, WECHAT_CREDENTIAL_ACCOUNT,
    WECHAT_CREDENTIAL_SERVICE,
};
use pca_wechat_provider::{
    source::{SourceCapabilities, SourceCursor, WechatSource},
    sqlcipher_source::{
        ReadOnlySqlcipherProbe, RusqliteReadOnlyProbe, SqlcipherProbeFailure, SqlcipherProbeTarget,
        SqlcipherWechatSource,
    },
};
use rusqlite::Connection;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct EmptyKeyStore;

impl CredentialStore for EmptyKeyStore {
    fn load(&self, _: &str, _: &str) -> Result<Option<Vec<u8>>, CredentialError> {
        Ok(None)
    }

    fn store(&self, _: &str, _: &str, _: &[u8]) -> Result<(), CredentialError> {
        Err(CredentialError::UnsupportedIdentity)
    }

    fn delete(&self, _: &str, _: &str) -> Result<(), CredentialError> {
        Err(CredentialError::UnsupportedIdentity)
    }
}

struct UnavailableKeyStore;

impl CredentialStore for UnavailableKeyStore {
    fn load(&self, _: &str, _: &str) -> Result<Option<Vec<u8>>, CredentialError> {
        Err(CredentialError::Unavailable)
    }

    fn store(&self, _: &str, _: &str, _: &[u8]) -> Result<(), CredentialError> {
        Err(CredentialError::UnsupportedIdentity)
    }

    fn delete(&self, _: &str, _: &str) -> Result<(), CredentialError> {
        Err(CredentialError::UnsupportedIdentity)
    }
}

struct SlowKeyStore {
    delay: Duration,
    material: Vec<u8>,
}

impl CredentialStore for SlowKeyStore {
    fn load(&self, _: &str, _: &str) -> Result<Option<Vec<u8>>, CredentialError> {
        thread::sleep(self.delay);
        Ok(Some(self.material.clone()))
    }

    fn store(&self, _: &str, _: &str, _: &[u8]) -> Result<(), CredentialError> {
        Err(CredentialError::UnsupportedIdentity)
    }

    fn delete(&self, _: &str, _: &str) -> Result<(), CredentialError> {
        Err(CredentialError::UnsupportedIdentity)
    }
}

struct EncodedKeyStore(Vec<u8>);

impl CredentialStore for EncodedKeyStore {
    fn load(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, CredentialError> {
        if service == WECHAT_CREDENTIAL_SERVICE && account == WECHAT_CREDENTIAL_ACCOUNT {
            Ok(Some(self.0.clone()))
        } else {
            Ok(None)
        }
    }

    fn store(&self, _: &str, _: &str, _: &[u8]) -> Result<(), CredentialError> {
        Err(CredentialError::UnsupportedIdentity)
    }

    fn delete(&self, _: &str, _: &str) -> Result<(), CredentialError> {
        Err(CredentialError::UnsupportedIdentity)
    }
}

struct MustNotProbe;

impl ReadOnlySqlcipherProbe for MustNotProbe {
    fn probe(
        &self,
        _: &SqlcipherProbeTarget,
        _: &WechatKeyMaterial,
        _: Duration,
    ) -> Result<SourceCapabilities, SqlcipherProbeFailure> {
        panic!("database probe must not run without Keychain material")
    }
}

struct FixedProbe(Result<SourceCapabilities, SqlcipherProbeFailure>);

impl ReadOnlySqlcipherProbe for FixedProbe {
    fn probe(
        &self,
        _: &SqlcipherProbeTarget,
        _: &WechatKeyMaterial,
        _: Duration,
    ) -> Result<SourceCapabilities, SqlcipherProbeFailure> {
        self.0.clone()
    }
}

#[derive(Default)]
struct SequencedProbeState {
    calls: usize,
    release_first: bool,
}

#[derive(Clone, Default)]
struct SequencedProbe {
    inner: Arc<SequencedProbeInner>,
}

#[derive(Default)]
struct SequencedProbeInner {
    state: Mutex<SequencedProbeState>,
    changed: Condvar,
}

impl SequencedProbe {
    fn wait_for_first_call(&self) {
        let mut state = self.inner.state.lock().expect("sequenced probe lock");
        while state.calls == 0 {
            state = self
                .inner
                .changed
                .wait(state)
                .expect("wait for first probe");
        }
    }

    fn call_count(&self) -> usize {
        self.inner.state.lock().expect("sequenced probe lock").calls
    }

    fn release_first(&self) {
        let mut state = self.inner.state.lock().expect("sequenced probe lock");
        state.release_first = true;
        self.inner.changed.notify_all();
    }
}

impl ReadOnlySqlcipherProbe for SequencedProbe {
    fn probe(
        &self,
        _: &SqlcipherProbeTarget,
        _: &WechatKeyMaterial,
        _: Duration,
    ) -> Result<SourceCapabilities, SqlcipherProbeFailure> {
        let mut state = self.inner.state.lock().expect("sequenced probe lock");
        state.calls += 1;
        let call = state.calls;
        self.inner.changed.notify_all();
        while call == 1 && !state.release_first {
            state = self
                .inner
                .changed
                .wait(state)
                .expect("wait to release probe");
        }
        drop(state);

        if call == 1 {
            Ok(SourceCapabilities {
                source_version: "4.1.12".to_owned(),
                schema_version: 7,
            })
        } else {
            Err(SqlcipherProbeFailure::AccountUnverified)
        }
    }
}

#[test]
fn missing_key_material_returns_waiting_state_without_probing_a_database() {
    let source = SqlcipherWechatSource::with_dependencies(
        EmptyKeyStore,
        MustNotProbe,
        fixture_target(PathBuf::from("unused-encrypted-source.db")),
    );

    let error = source.probe_blocking().unwrap_err();

    assert_eq!(error.code, "WECHAT_WAITING_SOURCE");
    assert!(error.retryable);
}

#[test]
fn noninteractive_keychain_failure_returns_capability_state_without_probing_a_database() {
    let source = SqlcipherWechatSource::with_dependencies(
        UnavailableKeyStore,
        MustNotProbe,
        fixture_target(PathBuf::from("unused-encrypted-source.db")),
    );

    let error = source.probe_blocking().unwrap_err();

    assert_eq!(error.code, "WECHAT_CAPABILITY_UNAVAILABLE");
    assert!(error.retryable);
}

#[test]
fn keychain_work_that_exhausts_the_total_deadline_never_starts_the_database_probe() {
    let source = SqlcipherWechatSource::with_dependencies(
        SlowKeyStore {
            delay: Duration::from_millis(2_050),
            material: encoded_material("local-account-proof", [0x2a; 32]),
        },
        MustNotProbe,
        fixture_target(PathBuf::from("unused-encrypted-source.db")),
    );

    let error = source.probe_blocking().unwrap_err();

    assert_eq!(error.code, "WECHAT_PROBE_TIMEOUT");
    assert!(error.retryable);
}

#[tokio::test]
async fn overlapping_probes_are_serialized_and_the_later_failure_clears_success() {
    let probe = SequencedProbe::default();
    let source = Arc::new(SqlcipherWechatSource::with_dependencies(
        EncodedKeyStore(encoded_material("local-account-proof", [0x2a; 32])),
        probe.clone(),
        fixture_target(PathBuf::from("unused-encrypted-source.db")),
    ));

    let first_source = Arc::clone(&source);
    let first = thread::spawn(move || first_source.probe_blocking());
    probe.wait_for_first_call();

    let second_source = Arc::clone(&source);
    let second = thread::spawn(move || second_source.probe_blocking());
    thread::sleep(Duration::from_millis(50));
    assert_eq!(
        probe.call_count(),
        1,
        "only one probe may execute at a time"
    );

    probe.release_first();
    first
        .join()
        .expect("first probe thread")
        .expect("first probe succeeds");
    let second_error = second
        .join()
        .expect("second probe thread")
        .expect_err("later probe fails");
    assert_eq!(second_error.code, "WECHAT_ACCOUNT_UNVERIFIED");

    let Err(read_error) = source.read_after(&SourceCursor).await else {
        panic!("later failure must clear prior proof");
    };
    assert_eq!(read_error.code, "WECHAT_WAITING_SOURCE");
}

#[tokio::test]
async fn no_record_is_returned_until_all_probe_evidence_is_established() {
    let material = encoded_material("local-account-proof", [0x2a; 32]);
    let source = SqlcipherWechatSource::with_dependencies(
        EncodedKeyStore(material),
        FixedProbe(Err(SqlcipherProbeFailure::AccountUnverified)),
        fixture_target(PathBuf::from("unused-encrypted-source.db")),
    );

    let probe_error = source.probe().await.unwrap_err();
    let Err(read_error) = source.read_after(&SourceCursor).await else {
        panic!("unverified source must not return records");
    };

    assert_eq!(probe_error.code, "WECHAT_ACCOUNT_UNVERIFIED");
    assert_eq!(read_error.code, "WECHAT_WAITING_SOURCE");
}

#[test]
fn every_probe_failure_maps_to_an_explicit_redacted_wechat_code() {
    let cases = [
        (
            SqlcipherProbeFailure::CapabilityUnavailable,
            "WECHAT_CAPABILITY_UNAVAILABLE",
        ),
        (
            SqlcipherProbeFailure::DatabaseUnavailable,
            "WECHAT_DATABASE_UNAVAILABLE",
        ),
        (SqlcipherProbeFailure::KeyRejected, "WECHAT_KEY_REJECTED"),
        (SqlcipherProbeFailure::TimedOut, "WECHAT_PROBE_TIMEOUT"),
        (
            SqlcipherProbeFailure::UnsupportedSourceVersion,
            "WECHAT_UNSUPPORTED_SOURCE_VERSION",
        ),
        (
            SqlcipherProbeFailure::UnsupportedSchema,
            "WECHAT_UNSUPPORTED_SCHEMA",
        ),
        (
            SqlcipherProbeFailure::AccountUnverified,
            "WECHAT_ACCOUNT_UNVERIFIED",
        ),
    ];

    for (failure, expected_code) in cases {
        let source = SqlcipherWechatSource::with_dependencies(
            EncodedKeyStore(encoded_material("private-account-proof", [0x5c; 32])),
            FixedProbe(Err(failure)),
            fixture_target(PathBuf::from("/private/source-path-must-not-appear")),
        );
        let error = source.probe_blocking().unwrap_err();
        let output = format!("{error:?} {error}");

        assert_eq!(error.code, expected_code);
        assert!(!output.contains("private-account-proof"));
        assert!(!output.contains("source-path-must-not-appear"));
        assert!(!output.contains("92"));
    }
}

#[tokio::test]
async fn encrypted_fixture_proves_read_only_sqlcipher_version_schema_and_account_capability() {
    let fixture = EncryptedFixture::new([0x4d; 32]);
    let source = SqlcipherWechatSource::with_dependencies(
        EncodedKeyStore(encoded_material("fixture-local-account", [0x4d; 32])),
        RusqliteReadOnlyProbe,
        fixture_target(fixture.database_path().to_path_buf()),
    );

    let capabilities = source.probe().await.expect("fixture probe");
    let records = source
        .read_after(&SourceCursor)
        .await
        .expect("verified source can be read by later task");

    assert_eq!(capabilities.schema_version, 7);
    assert!(records.is_empty());
    assert_eq!(fixture.metadata_row_count(), 1);
}

#[tokio::test]
async fn wal_source_probe_creates_or_modifies_nothing_in_the_source_directory() {
    let fixture = WalEncryptedFixture::new([0x6d; 32]);
    let before = DirectorySnapshot::capture(fixture.source_directory());
    let source = SqlcipherWechatSource::with_dependencies(
        EncodedKeyStore(encoded_material("fixture-local-account", [0x6d; 32])),
        RusqliteReadOnlyProbe,
        fixture_target(fixture.database_path().to_path_buf()),
    );

    let capabilities = source.probe().await.expect("WAL fixture probe");
    let after = DirectorySnapshot::capture(fixture.source_directory());

    assert_eq!(capabilities.schema_version, 7);
    assert_eq!(
        after, before,
        "source directory must remain byte-for-byte and metadata unchanged"
    );
}

#[test]
fn encrypted_fixture_rejects_wrong_key_version_schema_and_account_without_details() {
    let fixture = EncryptedFixture::new([0x31; 32]);
    let database_path = fixture.database_path().to_path_buf();
    let cases = [
        (
            encoded_material("fixture-local-account", [0x32; 32]),
            fixture_target(database_path.clone()),
            "WECHAT_KEY_REJECTED",
        ),
        (
            encoded_material("fixture-local-account", [0x31; 32]),
            target_with_expectations(database_path.clone(), "4.1.13", 7, 7),
            "WECHAT_UNSUPPORTED_SOURCE_VERSION",
        ),
        (
            encoded_material("fixture-local-account", [0x31; 32]),
            target_with_expectations(database_path.clone(), "4.1.12", 8, 9),
            "WECHAT_UNSUPPORTED_SCHEMA",
        ),
        (
            encoded_material("different-local-account", [0x31; 32]),
            fixture_target(database_path),
            "WECHAT_ACCOUNT_UNVERIFIED",
        ),
    ];

    for (material, target, expected_code) in cases {
        let source = SqlcipherWechatSource::with_dependencies(
            EncodedKeyStore(material),
            RusqliteReadOnlyProbe,
            target,
        );
        let error = source.probe_blocking().unwrap_err();
        let output = format!("{error:?} {error}");

        assert_eq!(error.code, expected_code);
        assert!(!output.contains("fixture-local-account"));
        assert!(!output.contains("different-local-account"));
        assert!(!output.contains("wechat-fixture"));
    }
}

fn encoded_material(account_id: &str, key: [u8; 32]) -> Vec<u8> {
    WechatKeyMaterial::new(account_id, key)
        .expect("valid fixture material")
        .encode()
        .expect("encoded fixture material")
}

fn fixture_target(path: PathBuf) -> SqlcipherProbeTarget {
    target_with_expectations(path, "4.1.12", 7, 7)
}

fn target_with_expectations(
    path: PathBuf,
    source_version: &'static str,
    minimum_schema: u32,
    maximum_schema: u32,
) -> SqlcipherProbeTarget {
    SqlcipherProbeTarget::new(
        path,
        source_version,
        minimum_schema,
        maximum_schema,
        "SELECT source_version FROM source_probe_metadata LIMIT 1",
        "SELECT schema_version FROM source_probe_metadata LIMIT 1",
        "SELECT account_id FROM source_probe_metadata LIMIT 1",
    )
    .expect("valid static fixture probe")
}

struct EncryptedFixture {
    directory: PathBuf,
    database: PathBuf,
    key: [u8; 32],
}

impl EncryptedFixture {
    fn new(key: [u8; 32]) -> Self {
        let suffix = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "pca-wechat-fixture-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir(&directory).expect("create isolated fixture directory");
        let database = directory.join("source.db");
        let connection = Connection::open(&database).expect("create SQLCipher fixture");
        apply_raw_key(&connection, &key).expect("set fixture SQLCipher key");
        connection
            .execute_batch(
                "CREATE TABLE source_probe_metadata (\
                    source_version TEXT NOT NULL, \
                    schema_version INTEGER NOT NULL, \
                    account_id TEXT NOT NULL\
                 );\
                 INSERT INTO source_probe_metadata VALUES ('4.1.12', 7, 'fixture-local-account');",
            )
            .expect("create encrypted fixture schema");
        drop(connection);

        Self {
            directory,
            database,
            key,
        }
    }

    fn database_path(&self) -> &Path {
        &self.database
    }

    fn metadata_row_count(&self) -> i64 {
        let connection = Connection::open(&self.database).expect("open fixture for verification");
        apply_raw_key(&connection, &self.key).expect("unlock fixture for verification");
        connection
            .query_row("SELECT count(*) FROM source_probe_metadata", [], |row| {
                row.get(0)
            })
            .expect("fixture row count")
    }
}

impl Drop for EncryptedFixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.directory).expect("remove owned fixture directory");
    }
}

struct WalEncryptedFixture {
    root: PathBuf,
    source_directory: PathBuf,
    database: PathBuf,
    _writer: Connection,
}

impl WalEncryptedFixture {
    fn new(key: [u8; 32]) -> Self {
        let suffix = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "pca-wechat-wal-fixture-{}-{suffix}",
            std::process::id()
        ));
        let staging = root.join("staging");
        let source_directory = root.join("source");
        std::fs::create_dir_all(&staging).expect("create WAL staging directory");
        std::fs::create_dir(&source_directory).expect("create WAL source directory");
        let staging_database = staging.join("source.db");
        let writer = Connection::open(&staging_database).expect("create WAL SQLCipher fixture");
        apply_raw_key(&writer, &key).expect("set WAL fixture SQLCipher key");
        writer
            .pragma_update(None, "journal_mode", "WAL")
            .expect("enable WAL mode");
        writer
            .pragma_update(None, "wal_autocheckpoint", 0)
            .expect("disable fixture auto-checkpoint");
        writer
            .execute_batch(
                "CREATE TABLE source_probe_metadata (\
                    source_version TEXT NOT NULL, \
                    schema_version INTEGER NOT NULL, \
                    account_id TEXT NOT NULL\
                 );\
                 INSERT INTO source_probe_metadata VALUES ('4.1.12', 7, 'fixture-local-account');",
            )
            .expect("create WAL encrypted fixture schema");

        let database = source_directory.join("source.db");
        std::fs::copy(&staging_database, &database).expect("snapshot WAL fixture database");
        std::fs::copy(
            staging_database.with_file_name("source.db-wal"),
            source_directory.join("source.db-wal"),
        )
        .expect("snapshot WAL fixture log without SHM");

        Self {
            root,
            source_directory,
            database,
            _writer: writer,
        }
    }

    fn source_directory(&self) -> &Path {
        &self.source_directory
    }

    fn database_path(&self) -> &Path {
        &self.database
    }
}

impl Drop for WalEncryptedFixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).expect("remove owned WAL fixture directory");
    }
}

#[derive(Debug, Eq, PartialEq)]
struct DirectorySnapshot {
    directory: MetadataSnapshot,
    entries: BTreeMap<String, FileSnapshot>,
}

impl DirectorySnapshot {
    fn capture(directory: &Path) -> Self {
        let mut entries = BTreeMap::new();
        for entry in std::fs::read_dir(directory).expect("read fixture source directory") {
            let entry = entry.expect("fixture directory entry");
            let name = entry.file_name().to_string_lossy().into_owned();
            let contents = std::fs::read(entry.path()).expect("read fixture source entry");
            let metadata = entry.metadata().expect("fixture source entry metadata");
            entries.insert(
                name,
                FileSnapshot {
                    metadata: MetadataSnapshot::from(&metadata),
                    contents,
                },
            );
        }
        let directory_metadata = std::fs::metadata(directory).expect("fixture directory metadata");
        Self {
            directory: MetadataSnapshot::from(&directory_metadata),
            entries,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct FileSnapshot {
    metadata: MetadataSnapshot,
    contents: Vec<u8>,
}

#[derive(Debug, Eq, PartialEq)]
struct MetadataSnapshot {
    len: u64,
    permissions: u32,
    device: u64,
    inode: u64,
    modified: Option<SystemTime>,
    accessed: Option<SystemTime>,
    created: Option<SystemTime>,
}

impl From<&Metadata> for MetadataSnapshot {
    fn from(metadata: &Metadata) -> Self {
        Self {
            len: metadata.len(),
            permissions: metadata.permissions().mode(),
            device: metadata.dev(),
            inode: metadata.ino(),
            modified: metadata.modified().ok(),
            accessed: metadata.accessed().ok(),
            created: metadata.created().ok(),
        }
    }
}

fn apply_raw_key(connection: &Connection, key: &[u8; 32]) -> Result<(), DomainError> {
    let mut raw_literal = String::with_capacity(67);
    raw_literal.push_str("x'");
    for byte in key {
        use std::fmt::Write as _;
        write!(&mut raw_literal, "{byte:02x}")
            .map_err(|_| DomainError::new("FIXTURE_ERROR", "key formatting failed", false))?;
    }
    raw_literal.push('\'');
    connection
        .pragma_update(None, "key", raw_literal)
        .map_err(|_| DomainError::new("FIXTURE_ERROR", "key setup failed", false))
}
