use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
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
