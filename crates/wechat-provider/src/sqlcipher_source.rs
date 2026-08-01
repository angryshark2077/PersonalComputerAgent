use std::{
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use pca_domain::DomainError;
use pca_keychain::{load_wechat_key_material, CredentialError, CredentialStore, WechatKeyMaterial};

use crate::source::{
    SourceCapabilities, SourceCursor, SourceProbeFuture, SourceReadFuture, WechatSource,
};

const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_PROBE_QUERY_LENGTH: usize = 4_096;

/// Fixed, non-sensitive evidence required from one `SQLCipher` source database.
///
/// The path and query text intentionally have no `Debug` implementation so diagnostics cannot
/// accidentally disclose a local account path or source schema detail.
pub struct SqlcipherProbeTarget {
    database_path: PathBuf,
    expected_source_version: &'static str,
    minimum_schema_version: u32,
    maximum_schema_version: u32,
    source_version_query: &'static str,
    schema_version_query: &'static str,
    account_id_query: &'static str,
}

impl SqlcipherProbeTarget {
    /// Builds a bounded read-only probe target from adapter-owned constants.
    ///
    /// # Errors
    ///
    /// Returns a redacted capability error if any static expectation is empty, unbounded, or not
    /// a single read-only `SELECT` statement.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        database_path: PathBuf,
        expected_source_version: &'static str,
        minimum_schema_version: u32,
        maximum_schema_version: u32,
        source_version_query: &'static str,
        schema_version_query: &'static str,
        account_id_query: &'static str,
    ) -> Result<Self, DomainError> {
        let version_is_valid = !expected_source_version.trim().is_empty()
            && expected_source_version.len() <= 128
            && !expected_source_version.chars().any(char::is_control);
        let schema_range_is_valid =
            minimum_schema_version > 0 && minimum_schema_version <= maximum_schema_version;
        let queries_are_valid = [source_version_query, schema_version_query, account_id_query]
            .into_iter()
            .all(is_bounded_select);

        if database_path.as_os_str().is_empty()
            || !version_is_valid
            || !schema_range_is_valid
            || !queries_are_valid
        {
            return Err(capability_unavailable());
        }

        Ok(Self {
            database_path,
            expected_source_version,
            minimum_schema_version,
            maximum_schema_version,
            source_version_query,
            schema_version_query,
            account_id_query,
        })
    }
}

fn is_bounded_select(query: &str) -> bool {
    let trimmed = query.trim();
    trimmed.len() <= MAX_PROBE_QUERY_LENGTH
        && !trimmed.contains(';')
        && !trimmed.contains('\0')
        && trimmed
            .split_ascii_whitespace()
            .next()
            .is_some_and(|token| token.eq_ignore_ascii_case("select"))
}

/// Safe failure classification for the `SQLCipher` adapter boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SqlcipherProbeFailure {
    CapabilityUnavailable,
    DatabaseUnavailable,
    KeyRejected,
    TimedOut,
    UnsupportedSourceVersion,
    UnsupportedSchema,
    AccountUnverified,
}

/// Port for the native, read-only `SQLCipher` capability check.
pub trait ReadOnlySqlcipherProbe: Send + Sync {
    /// Validates the source without writing to it or returning source records.
    ///
    /// # Errors
    ///
    /// Returns a redacted capability failure when any required proof is absent.
    fn probe(
        &self,
        target: &SqlcipherProbeTarget,
        key_material: &WechatKeyMaterial,
        timeout: Duration,
    ) -> Result<SourceCapabilities, SqlcipherProbeFailure>;
}

/// `SQLCipher` probe backed by the workspace's bundled native library.
#[derive(Clone, Copy, Debug, Default)]
pub struct RusqliteReadOnlyProbe;

/// A source that returns records only after Keychain, `SQLCipher`, schema, and account proof pass.
pub struct SqlcipherWechatSource<K, P> {
    key_store: K,
    probe: P,
    target: SqlcipherProbeTarget,
    probe_succeeded: AtomicBool,
}

impl<K, P> SqlcipherWechatSource<K, P>
where
    K: CredentialStore,
    P: ReadOnlySqlcipherProbe,
{
    #[must_use]
    pub fn with_dependencies(key_store: K, probe: P, target: SqlcipherProbeTarget) -> Self {
        Self {
            key_store,
            probe,
            target,
            probe_succeeded: AtomicBool::new(false),
        }
    }

    /// Performs the fixed bounded probe without retaining `KeyMaterial`.
    ///
    /// # Errors
    ///
    /// Returns only explicit, redacted `WECHAT_*` errors.
    pub fn probe_blocking(&self) -> Result<SourceCapabilities, DomainError> {
        self.probe_succeeded.store(false, Ordering::Release);
        let key_material = load_wechat_key_material(&self.key_store)
            .map_err(map_credential_error)?
            .ok_or_else(waiting_source)?;
        let capabilities = self
            .probe
            .probe(&self.target, &key_material, DEFAULT_PROBE_TIMEOUT)
            .map_err(map_probe_failure)?;
        self.probe_succeeded.store(true, Ordering::Release);
        Ok(capabilities)
    }
}

impl<K, P> WechatSource for SqlcipherWechatSource<K, P>
where
    K: CredentialStore,
    P: ReadOnlySqlcipherProbe,
{
    fn probe(&self) -> SourceProbeFuture<'_> {
        Box::pin(async move { self.probe_blocking() })
    }

    fn read_after(&self, _: &SourceCursor) -> SourceReadFuture<'_> {
        Box::pin(async move {
            if self.probe_succeeded.load(Ordering::Acquire) {
                // Message extraction belongs to the later source-schema task. Returning an empty
                // batch here proves this capability gate cannot leak a record by itself.
                Ok(Vec::new())
            } else {
                Err(waiting_source())
            }
        })
    }
}

fn map_credential_error(error: CredentialError) -> DomainError {
    match error {
        CredentialError::Unavailable
        | CredentialError::OperationFailed
        | CredentialError::UnsupportedIdentity => capability_unavailable(),
        CredentialError::InvalidSecretLength
        | CredentialError::CorruptSecret
        | CredentialError::InvalidCredential => {
            map_probe_failure(SqlcipherProbeFailure::KeyRejected)
        }
    }
}

fn map_probe_failure(failure: SqlcipherProbeFailure) -> DomainError {
    match failure {
        SqlcipherProbeFailure::CapabilityUnavailable => capability_unavailable(),
        SqlcipherProbeFailure::DatabaseUnavailable => DomainError::new(
            "WECHAT_DATABASE_UNAVAILABLE",
            "WeChat source database is unavailable",
            true,
        ),
        SqlcipherProbeFailure::KeyRejected => DomainError::new(
            "WECHAT_KEY_REJECTED",
            "stored WeChat key material was rejected",
            false,
        ),
        SqlcipherProbeFailure::TimedOut => DomainError::new(
            "WECHAT_PROBE_TIMEOUT",
            "WeChat source verification timed out",
            true,
        ),
        SqlcipherProbeFailure::UnsupportedSourceVersion => DomainError::new(
            "WECHAT_UNSUPPORTED_SOURCE_VERSION",
            "WeChat source version is unsupported",
            false,
        ),
        SqlcipherProbeFailure::UnsupportedSchema => DomainError::new(
            "WECHAT_UNSUPPORTED_SCHEMA",
            "WeChat source schema is unsupported",
            false,
        ),
        SqlcipherProbeFailure::AccountUnverified => DomainError::new(
            "WECHAT_ACCOUNT_UNVERIFIED",
            "WeChat source account could not be verified",
            false,
        ),
    }
}

fn waiting_source() -> DomainError {
    DomainError::new(
        "WECHAT_WAITING_SOURCE",
        "WeChat source key material is not available",
        true,
    )
}

fn capability_unavailable() -> DomainError {
    DomainError::new(
        "WECHAT_CAPABILITY_UNAVAILABLE",
        "WeChat source verification capability is unavailable",
        true,
    )
}

#[cfg(target_os = "macos")]
mod macos {
    use std::{fmt::Write as _, time::Instant};

    use rusqlite::{
        hooks::{AuthAction, AuthContext, Authorization},
        Connection, OpenFlags,
    };

    use super::{
        ReadOnlySqlcipherProbe, RusqliteReadOnlyProbe, SourceCapabilities, SqlcipherProbeFailure,
        SqlcipherProbeTarget, WechatKeyMaterial,
    };
    use std::time::Duration;

    impl ReadOnlySqlcipherProbe for RusqliteReadOnlyProbe {
        fn probe(
            &self,
            target: &SqlcipherProbeTarget,
            key_material: &WechatKeyMaterial,
            timeout: Duration,
        ) -> Result<SourceCapabilities, SqlcipherProbeFailure> {
            let deadline = Instant::now()
                .checked_add(timeout)
                .ok_or(SqlcipherProbeFailure::TimedOut)?;
            if timeout.is_zero() || !target.database_path.is_file() {
                return Err(SqlcipherProbeFailure::DatabaseUnavailable);
            }

            let connection = Connection::open_with_flags(
                &target.database_path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(|_| SqlcipherProbeFailure::DatabaseUnavailable)?;
            connection
                .busy_timeout(remaining(deadline)?)
                .map_err(|_| SqlcipherProbeFailure::CapabilityUnavailable)?;
            connection.progress_handler(
                1_000,
                Some(move || Instant::now().checked_duration_since(deadline).is_some()),
            );

            let cipher_version: String = connection
                .query_row("PRAGMA cipher_version", [], |row| row.get(0))
                .map_err(|_| SqlcipherProbeFailure::CapabilityUnavailable)?;
            if cipher_version.trim().is_empty() {
                return Err(SqlcipherProbeFailure::CapabilityUnavailable);
            }

            apply_raw_key(&connection, key_material.raw_key())?;
            connection
                .pragma_update(None, "query_only", true)
                .map_err(|_| SqlcipherProbeFailure::CapabilityUnavailable)?;
            connection.authorizer(Some(|context: AuthContext<'_>| match context.action {
                AuthAction::Select | AuthAction::Read { .. } => Authorization::Allow,
                _ => Authorization::Deny,
            }));

            ensure_unlocked(&connection, deadline)?;
            let source_version: String = query_value(
                &connection,
                target.source_version_query,
                deadline,
                SqlcipherProbeFailure::UnsupportedSourceVersion,
            )?;
            if source_version != target.expected_source_version {
                return Err(SqlcipherProbeFailure::UnsupportedSourceVersion);
            }

            let schema_version: i64 = query_value(
                &connection,
                target.schema_version_query,
                deadline,
                SqlcipherProbeFailure::UnsupportedSchema,
            )?;
            let schema_version = u32::try_from(schema_version)
                .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
            if !(target.minimum_schema_version..=target.maximum_schema_version)
                .contains(&schema_version)
            {
                return Err(SqlcipherProbeFailure::UnsupportedSchema);
            }

            let account_id: String = query_value(
                &connection,
                target.account_id_query,
                deadline,
                SqlcipherProbeFailure::AccountUnverified,
            )?;
            if account_id != key_material.account_id() {
                return Err(SqlcipherProbeFailure::AccountUnverified);
            }

            Ok(SourceCapabilities {
                source_version,
                schema_version,
            })
        }
    }

    fn apply_raw_key(
        connection: &Connection,
        raw_key: &[u8; 32],
    ) -> Result<(), SqlcipherProbeFailure> {
        let mut raw_literal = String::with_capacity(67);
        raw_literal.push_str("x'");
        for byte in raw_key {
            write!(&mut raw_literal, "{byte:02x}")
                .map_err(|_| SqlcipherProbeFailure::CapabilityUnavailable)?;
        }
        raw_literal.push('\'');
        connection
            .pragma_update(None, "key", raw_literal)
            .map_err(|_| SqlcipherProbeFailure::KeyRejected)
    }

    fn ensure_unlocked(
        connection: &Connection,
        deadline: Instant,
    ) -> Result<(), SqlcipherProbeFailure> {
        query_value::<String>(
            connection,
            "SELECT type FROM sqlite_master LIMIT 1",
            deadline,
            SqlcipherProbeFailure::KeyRejected,
        )
        .map(|_| ())
    }

    fn query_value<T>(
        connection: &Connection,
        query: &str,
        deadline: Instant,
        failure: SqlcipherProbeFailure,
    ) -> Result<T, SqlcipherProbeFailure>
    where
        T: rusqlite::types::FromSql,
    {
        connection
            .busy_timeout(remaining(deadline)?)
            .map_err(|_| SqlcipherProbeFailure::CapabilityUnavailable)?;
        connection
            .query_row(query, [], |row| row.get(0))
            .map_err(|error| {
                if is_timed_out(deadline, &error) {
                    SqlcipherProbeFailure::TimedOut
                } else {
                    failure
                }
            })
    }

    fn remaining(deadline: Instant) -> Result<Duration, SqlcipherProbeFailure> {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(SqlcipherProbeFailure::TimedOut)
    }

    fn is_timed_out(deadline: Instant, error: &rusqlite::Error) -> bool {
        Instant::now().checked_duration_since(deadline).is_some()
            || matches!(
                error,
                rusqlite::Error::SqliteFailure(code, _)
                    if code.code == rusqlite::ErrorCode::OperationInterrupted
            )
    }
}

#[cfg(not(target_os = "macos"))]
impl ReadOnlySqlcipherProbe for RusqliteReadOnlyProbe {
    fn probe(
        &self,
        _: &SqlcipherProbeTarget,
        _: &WechatKeyMaterial,
        _: Duration,
    ) -> Result<SourceCapabilities, SqlcipherProbeFailure> {
        Err(SqlcipherProbeFailure::CapabilityUnavailable)
    }
}
