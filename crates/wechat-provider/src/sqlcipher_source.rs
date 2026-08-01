use std::{
    path::PathBuf,
    sync::{Condvar, Mutex},
    time::{Duration, Instant},
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
    probe_state: Mutex<ProbeState>,
    probe_ready: Condvar,
}

#[derive(Default)]
struct ProbeState {
    running: bool,
    succeeded: bool,
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
            probe_state: Mutex::new(ProbeState::default()),
            probe_ready: Condvar::new(),
        }
    }

    /// Performs the fixed bounded probe without retaining `KeyMaterial`.
    ///
    /// # Errors
    ///
    /// Returns only explicit, redacted `WECHAT_*` errors.
    pub fn probe_blocking(&self) -> Result<SourceCapabilities, DomainError> {
        let deadline = Instant::now()
            .checked_add(DEFAULT_PROBE_TIMEOUT)
            .ok_or_else(|| map_probe_failure(SqlcipherProbeFailure::TimedOut))?;
        self.begin_probe(deadline)?;

        let result = (|| {
            let key_material = load_wechat_key_material(&self.key_store)
                .map_err(map_credential_error)?
                .ok_or_else(waiting_source)?;
            let capabilities = self
                .probe
                .probe(&self.target, &key_material, remaining_probe_time(deadline)?)
                .map_err(map_probe_failure)?;
            remaining_probe_time(deadline)?;
            Ok(capabilities)
        })();

        self.finish_probe(result.is_ok());
        result
    }

    fn begin_probe(&self, deadline: Instant) -> Result<(), DomainError> {
        let mut state = self
            .probe_state
            .lock()
            .map_err(|_| capability_unavailable())?;
        while state.running {
            let remaining = remaining_probe_time(deadline)?;
            let (next_state, wait) = self
                .probe_ready
                .wait_timeout(state, remaining)
                .map_err(|_| capability_unavailable())?;
            state = next_state;
            if wait.timed_out() && state.running {
                return Err(map_probe_failure(SqlcipherProbeFailure::TimedOut));
            }
        }
        state.running = true;
        state.succeeded = false;
        Ok(())
    }

    fn finish_probe(&self, succeeded: bool) {
        let mut state = match self.probe_state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.succeeded = succeeded;
        state.running = false;
        self.probe_ready.notify_all();
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
            let probe_succeeded = self.probe_state.lock().is_ok_and(|state| state.succeeded);
            if probe_succeeded {
                // Message extraction belongs to the later source-schema task. Returning an empty
                // batch here proves this capability gate cannot leak a record by itself.
                Ok(Vec::new())
            } else {
                Err(waiting_source())
            }
        })
    }
}

fn remaining_probe_time(deadline: Instant) -> Result<Duration, DomainError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| map_probe_failure(SqlcipherProbeFailure::TimedOut))
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
    use std::{
        fmt::Write as _,
        fs::{File, Metadata, OpenOptions},
        io::{Read, Write},
        os::unix::fs::{MetadataExt, OpenOptionsExt},
        path::{Path, PathBuf},
        time::Instant,
    };

    use rusqlite::{
        hooks::{AuthAction, AuthContext, Authorization},
        Connection, OpenFlags,
    };
    use tempfile::{Builder as TempDirBuilder, TempDir};

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
            if timeout.is_zero() {
                return Err(SqlcipherProbeFailure::TimedOut);
            }
            let snapshot = PrivateSourceSnapshot::create(&target.database_path, deadline)?;
            let connection = Connection::open_with_flags(
                snapshot.database_path(),
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(|_| SqlcipherProbeFailure::DatabaseUnavailable)?;
            remaining(deadline)?;
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

    /// A private DB/WAL copy prevents `SQLite`'s WAL VFS from creating or updating `-shm` in the
    /// `WeChat` source directory. The source files are opened only read-only with `O_NOFOLLOW` and
    /// must be regular files on a local, non-FUSE filesystem.
    ///
    /// macOS cannot cancel an individual in-flight local filesystem syscall. To avoid leaking a
    /// detached worker, the probe stays on the caller thread, uses nonblocking open flags, rejects
    /// network/FUSE sources before `SQLite` sees them, and checks the shared wall-clock deadline
    /// before and after every syscall and copy chunk. A kernel or hardware stall can delay the
    /// error return, but no result is accepted after the two-second deadline.
    struct PrivateSourceSnapshot {
        _directory: TempDir,
        database_path: PathBuf,
    }

    impl PrivateSourceSnapshot {
        fn create(
            source_database: &Path,
            deadline: Instant,
        ) -> Result<Self, SqlcipherProbeFailure> {
            remaining(deadline)?;
            let directory = TempDirBuilder::new()
                .prefix("pca-wechat-probe-")
                .tempdir()
                .map_err(|_| SqlcipherProbeFailure::CapabilityUnavailable)?;
            ensure_directory_is_private_local(directory.path(), deadline)?;

            let database_name = source_database
                .file_name()
                .filter(|name| !name.is_empty())
                .ok_or(SqlcipherProbeFailure::DatabaseUnavailable)?;
            let source_file = open_local_regular_file(source_database, None, deadline)?
                .ok_or(SqlcipherProbeFailure::DatabaseUnavailable)?;
            let source_device = source_file.metadata.dev();
            let database_path = directory.path().join(database_name);
            copy_stable_source_file(source_file, &database_path, deadline)?;

            let wal_path = sidecar_path(source_database, "-wal")?;
            if let Some(wal_file) =
                open_local_regular_file(&wal_path, Some(source_device), deadline)?
            {
                let snapshot_wal = sidecar_path(&database_path, "-wal")?;
                copy_stable_source_file(wal_file, &snapshot_wal, deadline)?;
            }

            remaining(deadline)?;
            Ok(Self {
                _directory: directory,
                database_path,
            })
        }

        fn database_path(&self) -> &Path {
            &self.database_path
        }
    }

    struct OpenSourceFile {
        file: File,
        metadata: Metadata,
    }

    fn open_local_regular_file(
        path: &Path,
        expected_device: Option<u64>,
        deadline: Instant,
    ) -> Result<Option<OpenSourceFile>, SqlcipherProbeFailure> {
        remaining(deadline)?;
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(SqlcipherProbeFailure::DatabaseUnavailable),
        };
        remaining(deadline)?;
        let metadata = file
            .metadata()
            .map_err(|_| SqlcipherProbeFailure::DatabaseUnavailable)?;
        if !metadata.is_file() || expected_device.is_some_and(|device| metadata.dev() != device) {
            return Err(SqlcipherProbeFailure::DatabaseUnavailable);
        }
        ensure_file_is_on_supported_filesystem(&file)?;
        remaining(deadline)?;
        Ok(Some(OpenSourceFile { file, metadata }))
    }

    fn ensure_directory_is_private_local(
        directory: &Path,
        deadline: Instant,
    ) -> Result<(), SqlcipherProbeFailure> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(directory)
            .map_err(|_| SqlcipherProbeFailure::CapabilityUnavailable)?;
        ensure_file_is_on_supported_filesystem(&file)?;
        remaining(deadline).map(|_| ())
    }

    fn ensure_file_is_on_supported_filesystem(file: &File) -> Result<(), SqlcipherProbeFailure> {
        let filesystem =
            rustix::fs::fstatfs(file).map_err(|_| SqlcipherProbeFailure::DatabaseUnavailable)?;
        let filesystem_name = filesystem
            .f_fstypename
            .iter()
            .take_while(|byte| **byte != 0)
            .map(|byte| byte.cast_unsigned())
            .collect::<Vec<_>>();
        let filesystem_name = std::str::from_utf8(&filesystem_name)
            .map_err(|_| SqlcipherProbeFailure::DatabaseUnavailable)?;
        let is_local = filesystem.f_flags & u32::try_from(libc::MNT_LOCAL).unwrap_or(0) != 0;
        if source_filesystem_is_supported(is_local, filesystem_name) {
            Ok(())
        } else {
            Err(SqlcipherProbeFailure::DatabaseUnavailable)
        }
    }

    fn source_filesystem_is_supported(is_local: bool, filesystem_name: &str) -> bool {
        is_local
            && !matches!(
                filesystem_name.to_ascii_lowercase().as_str(),
                "fusefs" | "macfuse" | "osxfuse"
            )
    }

    fn copy_stable_source_file(
        mut source: OpenSourceFile,
        destination: &Path,
        deadline: Instant,
    ) -> Result<(), SqlcipherProbeFailure> {
        let before = StableMetadata::from(&source.metadata);
        let mut destination = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(destination)
            .map_err(|_| SqlcipherProbeFailure::CapabilityUnavailable)?;
        let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
        loop {
            remaining(deadline)?;
            let read = source
                .file
                .read(&mut buffer)
                .map_err(|_| SqlcipherProbeFailure::DatabaseUnavailable)?;
            remaining(deadline)?;
            if read == 0 {
                break;
            }
            destination
                .write_all(&buffer[..read])
                .map_err(|_| SqlcipherProbeFailure::CapabilityUnavailable)?;
            remaining(deadline)?;
        }
        destination
            .flush()
            .map_err(|_| SqlcipherProbeFailure::CapabilityUnavailable)?;
        remaining(deadline)?;

        let after = source
            .file
            .metadata()
            .map_err(|_| SqlcipherProbeFailure::DatabaseUnavailable)?;
        if before != StableMetadata::from(&after) {
            return Err(SqlcipherProbeFailure::DatabaseUnavailable);
        }
        Ok(())
    }

    #[derive(Eq, PartialEq)]
    struct StableMetadata {
        device: u64,
        inode: u64,
        length: u64,
        modified_seconds: i64,
        modified_nanoseconds: i64,
    }

    impl From<&Metadata> for StableMetadata {
        fn from(metadata: &Metadata) -> Self {
            Self {
                device: metadata.dev(),
                inode: metadata.ino(),
                length: metadata.len(),
                modified_seconds: metadata.mtime(),
                modified_nanoseconds: metadata.mtime_nsec(),
            }
        }
    }

    fn sidecar_path(database: &Path, suffix: &str) -> Result<PathBuf, SqlcipherProbeFailure> {
        let mut name = database
            .file_name()
            .ok_or(SqlcipherProbeFailure::DatabaseUnavailable)?
            .to_os_string();
        name.push(suffix);
        Ok(database.with_file_name(name))
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

    #[cfg(test)]
    mod tests {
        use super::source_filesystem_is_supported;

        #[test]
        fn source_policy_rejects_network_and_fuse_filesystems() {
            assert!(!source_filesystem_is_supported(false, "smbfs"));
            assert!(!source_filesystem_is_supported(true, "macfuse"));
            assert!(!source_filesystem_is_supported(true, "osxfuse"));
            assert!(!source_filesystem_is_supported(true, "fusefs"));
            assert!(source_filesystem_is_supported(true, "apfs"));
        }
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
