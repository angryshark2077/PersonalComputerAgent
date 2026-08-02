use std::{
    path::PathBuf,
    sync::{
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError},
        Mutex,
    },
    thread::{self, JoinHandle},
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

/// Verifies one recovered key against a private, read-only database snapshot without inspecting
/// application records. It is used only by the explicit local repair binary before Keychain
/// persistence.
///
/// # Errors
///
/// Returns a redacted failure; neither the key, salt, database path, nor database contents are
/// returned or logged.
#[cfg(target_os = "macos")]
pub fn validate_recovered_key(
    database_path: &std::path::Path,
    key_material: &WechatKeyMaterial,
    timeout: Duration,
) -> Result<(), SqlcipherProbeFailure> {
    macos::validate_recovered_key(database_path, key_material, timeout)
}

/// Bounded table metadata returned only to the explicit local repair diagnostic.
///
/// This value deliberately contains no rows, database path, account identifier, or key material.
pub struct SqlcipherSchemaTable {
    name: String,
    columns: Vec<String>,
}

impl SqlcipherSchemaTable {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
}

/// Reads only bounded table and column names from a private read-only source snapshot.
///
/// # Errors
///
/// Returns a redacted failure when the key, database, schema, or deadline cannot be verified.
#[cfg(target_os = "macos")]
pub fn inspect_recovered_schema(
    database_path: &std::path::Path,
    key_material: &WechatKeyMaterial,
    timeout: Duration,
) -> Result<Vec<SqlcipherSchemaTable>, SqlcipherProbeFailure> {
    macos::inspect_recovered_schema(database_path, key_material, timeout)
}

#[cfg(target_os = "macos")]
pub(crate) fn with_recovered_database<T>(
    database_path: &std::path::Path,
    key_material: &WechatKeyMaterial,
    timeout: Duration,
    operation: impl FnOnce(&rusqlite::Connection) -> Result<T, SqlcipherProbeFailure>,
) -> Result<T, SqlcipherProbeFailure> {
    macos::with_recovered_database(database_path, key_material, timeout, operation)
}

/// A source that returns records only after Keychain, `SQLCipher`, schema, and account proof pass.
pub struct SqlcipherWechatSource {
    request_sender: SyncSender<WorkerCommand>,
    result_receiver: Mutex<Receiver<WorkerResult>>,
    coordinator: Mutex<CoordinatorState>,
    worker_thread: Option<JoinHandle<()>>,
    probe_timeout: Duration,
}

#[derive(Default)]
struct CoordinatorState {
    next_generation: u64,
    in_flight: Option<InFlightProbe>,
    proven_generation: Option<u64>,
    worker_available: bool,
}

#[derive(Clone, Copy)]
struct InFlightProbe {
    generation: u64,
    waiter_active: bool,
    authoritative: bool,
}

enum WorkerCommand {
    Probe { generation: u64, deadline: Instant },
    Shutdown,
}

struct WorkerResult {
    generation: u64,
    outcome: Result<SourceCapabilities, DomainError>,
}

struct ProbeWorker<K, P> {
    key_store: K,
    probe: P,
    target: SqlcipherProbeTarget,
    request_receiver: Receiver<WorkerCommand>,
    result_sender: SyncSender<WorkerResult>,
}

impl<K, P> ProbeWorker<K, P>
where
    K: CredentialStore,
    P: ReadOnlySqlcipherProbe,
{
    fn run(self) {
        let Self {
            key_store,
            probe,
            target,
            request_receiver,
            result_sender,
        } = self;
        while let Ok(command) = request_receiver.recv() {
            match command {
                WorkerCommand::Probe {
                    generation,
                    deadline,
                } => {
                    let outcome = run_worker_probe(&key_store, &probe, &target, deadline);
                    if result_sender
                        .send(WorkerResult {
                            generation,
                            outcome,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                WorkerCommand::Shutdown => break,
            }
        }
    }
}

impl SqlcipherWechatSource {
    #[must_use]
    pub fn with_dependencies<K, P>(key_store: K, probe: P, target: SqlcipherProbeTarget) -> Self
    where
        K: CredentialStore + 'static,
        P: ReadOnlySqlcipherProbe + 'static,
    {
        Self::build(key_store, probe, target, DEFAULT_PROBE_TIMEOUT)
    }

    #[cfg(test)]
    fn with_dependencies_and_timeout<K, P>(
        key_store: K,
        probe: P,
        target: SqlcipherProbeTarget,
        probe_timeout: Duration,
    ) -> Self
    where
        K: CredentialStore + 'static,
        P: ReadOnlySqlcipherProbe + 'static,
    {
        Self::build(key_store, probe, target, probe_timeout)
    }

    fn build<K, P>(
        key_store: K,
        probe: P,
        target: SqlcipherProbeTarget,
        probe_timeout: Duration,
    ) -> Self
    where
        K: CredentialStore + 'static,
        P: ReadOnlySqlcipherProbe + 'static,
    {
        let (request_sender, request_receiver) = mpsc::sync_channel(1);
        let (result_sender, result_receiver) = mpsc::sync_channel(1);
        let worker_thread = thread::Builder::new()
            .name("pca-wechat-probe".to_owned())
            .spawn(move || {
                ProbeWorker {
                    key_store,
                    probe,
                    target,
                    request_receiver,
                    result_sender,
                }
                .run();
            })
            .ok();
        let worker_available = worker_thread.is_some();

        Self {
            request_sender,
            result_receiver: Mutex::new(result_receiver),
            coordinator: Mutex::new(CoordinatorState {
                worker_available,
                ..CoordinatorState::default()
            }),
            worker_thread,
            probe_timeout,
        }
    }

    /// Performs the fixed bounded probe without retaining `KeyMaterial`.
    ///
    /// # Errors
    ///
    /// Returns only explicit, redacted `WECHAT_*` errors.
    pub fn probe_blocking(&self) -> Result<SourceCapabilities, DomainError> {
        let deadline = Instant::now()
            .checked_add(self.probe_timeout)
            .ok_or_else(|| map_probe_failure(SqlcipherProbeFailure::TimedOut))?;
        let generation = self.start_probe(deadline)?;

        loop {
            let received = self
                .result_receiver
                .lock()
                .map_err(|_| capability_unavailable())?
                .recv_timeout(remaining_probe_time(deadline)?);
            match received {
                Ok(result) if result.generation == generation => {
                    if remaining_probe_time(deadline).is_err() {
                        self.timeout_probe(generation);
                        return Err(map_probe_failure(SqlcipherProbeFailure::TimedOut));
                    }
                    return self.accept_worker_result(result);
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {
                    self.timeout_probe(generation);
                    return Err(map_probe_failure(SqlcipherProbeFailure::TimedOut));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    self.disconnect_worker(generation);
                    return Err(capability_unavailable());
                }
            }
        }
    }

    fn start_probe(&self, deadline: Instant) -> Result<u64, DomainError> {
        let mut state = self
            .coordinator
            .lock()
            .map_err(|_| capability_unavailable())?;
        state.proven_generation = None;
        if !state.worker_available {
            return Err(capability_unavailable());
        }

        if let Some(mut in_flight) = state.in_flight {
            in_flight.authoritative = false;
            state.in_flight = Some(in_flight);
            if in_flight.waiter_active {
                return Err(map_probe_failure(SqlcipherProbeFailure::TimedOut));
            }

            let stale_result = self
                .result_receiver
                .lock()
                .map_err(|_| capability_unavailable())?
                .try_recv();
            match stale_result {
                Ok(result) if result.generation == in_flight.generation => {
                    state.in_flight = None;
                }
                Ok(_) => return Err(capability_unavailable()),
                Err(TryRecvError::Empty) => {
                    return Err(map_probe_failure(SqlcipherProbeFailure::TimedOut));
                }
                Err(TryRecvError::Disconnected) => {
                    state.in_flight = None;
                    state.worker_available = false;
                    return Err(capability_unavailable());
                }
            }
        }

        remaining_probe_time(deadline)?;
        state.next_generation = state
            .next_generation
            .checked_add(1)
            .ok_or_else(capability_unavailable)?;
        let generation = state.next_generation;
        state.in_flight = Some(InFlightProbe {
            generation,
            waiter_active: true,
            authoritative: true,
        });
        drop(state);

        match self.request_sender.try_send(WorkerCommand::Probe {
            generation,
            deadline,
        }) {
            Ok(()) => Ok(generation),
            Err(TrySendError::Full(_)) => {
                self.timeout_probe(generation);
                Err(map_probe_failure(SqlcipherProbeFailure::TimedOut))
            }
            Err(TrySendError::Disconnected(_)) => {
                self.disconnect_worker(generation);
                Err(capability_unavailable())
            }
        }
    }

    fn accept_worker_result(
        &self,
        result: WorkerResult,
    ) -> Result<SourceCapabilities, DomainError> {
        let mut state = self
            .coordinator
            .lock()
            .map_err(|_| capability_unavailable())?;
        let Some(in_flight) = state.in_flight else {
            return Err(map_probe_failure(SqlcipherProbeFailure::TimedOut));
        };
        if in_flight.generation != result.generation
            || !in_flight.waiter_active
            || !in_flight.authoritative
        {
            if in_flight.generation == result.generation {
                state.in_flight = None;
            }
            state.proven_generation = None;
            return Err(map_probe_failure(SqlcipherProbeFailure::TimedOut));
        }

        state.in_flight = None;
        state.proven_generation = result.outcome.as_ref().ok().map(|_| result.generation);
        result.outcome
    }

    fn timeout_probe(&self, generation: u64) {
        let mut state = match self.coordinator.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.proven_generation = None;
        if let Some(in_flight) = state.in_flight.as_mut() {
            if in_flight.generation == generation {
                in_flight.waiter_active = false;
                in_flight.authoritative = false;
            }
        }
    }

    fn disconnect_worker(&self, generation: u64) {
        let mut state = match self.coordinator.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.proven_generation = None;
        if state
            .in_flight
            .is_some_and(|in_flight| in_flight.generation == generation)
        {
            state.in_flight = None;
        }
        state.worker_available = false;
    }
}

impl WechatSource for SqlcipherWechatSource {
    fn probe(&self) -> SourceProbeFuture<'_> {
        Box::pin(async move { self.probe_blocking() })
    }

    fn read_after(&self, _: &SourceCursor) -> SourceReadFuture<'_> {
        Box::pin(async move {
            let probe_succeeded = self
                .coordinator
                .lock()
                .is_ok_and(|state| state.proven_generation.is_some());
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

impl Drop for SqlcipherWechatSource {
    fn drop(&mut self) {
        let _ = self.request_sender.try_send(WorkerCommand::Shutdown);
        if self
            .worker_thread
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
        {
            if let Some(worker_thread) = self.worker_thread.take() {
                let _ = worker_thread.join();
            }
        }
    }
}

fn run_worker_probe<K, P>(
    key_store: &K,
    probe: &P,
    target: &SqlcipherProbeTarget,
    deadline: Instant,
) -> Result<SourceCapabilities, DomainError>
where
    K: CredentialStore,
    P: ReadOnlySqlcipherProbe,
{
    remaining_probe_time(deadline)?;
    let key_material = load_wechat_key_material(key_store)
        .map_err(map_credential_error)?
        .ok_or_else(waiting_source)?;
    let capabilities = probe
        .probe(target, &key_material, remaining_probe_time(deadline)?)
        .map_err(map_probe_failure)?;
    remaining_probe_time(deadline)?;
    Ok(capabilities)
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
        SqlcipherProbeTarget, SqlcipherSchemaTable, WechatKeyMaterial,
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

            apply_key_material(&connection, key_material, &target.database_path)?;
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
    /// macOS cannot cancel an individual in-flight local filesystem syscall. All Keychain,
    /// snapshot, and `SQLCipher` work therefore stays on the source's single owned worker. The
    /// coordinator returns the timeout independently, rejects more work while that worker is
    /// occupied, and never accepts a late result. Nonblocking open flags, local/non-FUSE policy,
    /// and deadline checks still bound every normally returning operation.
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

    pub(super) fn apply_key_material(
        connection: &Connection,
        key_material: &WechatKeyMaterial,
        database_path: &Path,
    ) -> Result<(), SqlcipherProbeFailure> {
        let database_key = key_material
            .key_for_database(database_path)
            .ok_or(SqlcipherProbeFailure::KeyRejected)?;
        if database_key.is_database_scoped() && database_key.salt().is_none() {
            sqlcipher_ffi::apply_wechat_key(connection, database_key.raw_key())?;
            connection
                .pragma_update(None, "cipher_page_size", 4096_i64)
                .map_err(|_| SqlcipherProbeFailure::KeyRejected)?;
            return Ok(());
        }
        let salt_length = database_key.salt().map_or(0, |salt| salt.len());
        let mut raw_literal = String::with_capacity(67 + (salt_length * 2));
        raw_literal.push_str("x'");
        for byte in database_key.raw_key() {
            write!(&mut raw_literal, "{byte:02x}")
                .map_err(|_| SqlcipherProbeFailure::CapabilityUnavailable)?;
        }
        if let Some(salt) = database_key.salt() {
            for byte in salt {
                write!(&mut raw_literal, "{byte:02x}")
                    .map_err(|_| SqlcipherProbeFailure::CapabilityUnavailable)?;
            }
        }
        raw_literal.push('\'');
        connection
            .pragma_update(None, "key", raw_literal)
            .map_err(|_| SqlcipherProbeFailure::KeyRejected)
    }

    #[allow(unsafe_code)]
    mod sqlcipher_ffi {
        use rusqlite::{ffi, Connection};

        use super::SqlcipherProbeFailure;

        pub(super) fn apply_wechat_key(
            connection: &Connection,
            key: &[u8; 32],
        ) -> Result<(), SqlcipherProbeFailure> {
            let key_length = i32::try_from(key.len())
                .map_err(|_| SqlcipherProbeFailure::CapabilityUnavailable)?;
            // SAFETY: `connection.handle()` is valid for the lifetime of `connection`; SQLCipher
            // copies the exactly bounded 32-byte key during this call, and neither pointer escapes.
            let result = unsafe {
                ffi::sqlite3_key(
                    connection.handle(),
                    key.as_ptr().cast::<std::ffi::c_void>(),
                    key_length,
                )
            };
            if result == ffi::SQLITE_OK {
                Ok(())
            } else {
                Err(SqlcipherProbeFailure::KeyRejected)
            }
        }
    }

    pub(super) fn validate_recovered_key(
        database_path: &Path,
        key_material: &WechatKeyMaterial,
        timeout: Duration,
    ) -> Result<(), SqlcipherProbeFailure> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(SqlcipherProbeFailure::TimedOut)?;
        if timeout.is_zero() {
            return Err(SqlcipherProbeFailure::TimedOut);
        }
        let snapshot = PrivateSourceSnapshot::create(database_path, deadline)?;
        let connection = Connection::open_with_flags(
            snapshot.database_path(),
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
        apply_key_material(&connection, key_material, database_path)?;
        connection
            .pragma_update(None, "query_only", true)
            .map_err(|_| SqlcipherProbeFailure::CapabilityUnavailable)?;
        connection.authorizer(Some(|context: AuthContext<'_>| match context.action {
            AuthAction::Select | AuthAction::Read { .. } => Authorization::Allow,
            _ => Authorization::Deny,
        }));
        ensure_unlocked(&connection, deadline)
    }

    pub(super) fn with_recovered_database<T>(
        database_path: &Path,
        key_material: &WechatKeyMaterial,
        timeout: Duration,
        operation: impl FnOnce(&Connection) -> Result<T, SqlcipherProbeFailure>,
    ) -> Result<T, SqlcipherProbeFailure> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(SqlcipherProbeFailure::TimedOut)?;
        if timeout.is_zero() {
            return Err(SqlcipherProbeFailure::TimedOut);
        }
        let snapshot = PrivateSourceSnapshot::create(database_path, deadline)?;
        let connection = Connection::open_with_flags(
            snapshot.database_path(),
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
        apply_key_material(&connection, key_material, database_path)?;
        connection
            .pragma_update(None, "query_only", true)
            .map_err(|_| SqlcipherProbeFailure::CapabilityUnavailable)?;
        ensure_unlocked(&connection, deadline)?;
        let result = operation(&connection)?;
        remaining(deadline)?;
        Ok(result)
    }

    pub(super) fn inspect_recovered_schema(
        database_path: &Path,
        key_material: &WechatKeyMaterial,
        timeout: Duration,
    ) -> Result<Vec<SqlcipherSchemaTable>, SqlcipherProbeFailure> {
        const MAX_TABLES: usize = 4_096;
        const MAX_COLUMNS_PER_TABLE: usize = 256;
        const MAX_IDENTIFIER_BYTES: usize = 512;

        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(SqlcipherProbeFailure::TimedOut)?;
        if timeout.is_zero() {
            return Err(SqlcipherProbeFailure::TimedOut);
        }
        let snapshot = PrivateSourceSnapshot::create(database_path, deadline)?;
        let connection = Connection::open_with_flags(
            snapshot.database_path(),
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
        apply_key_material(&connection, key_material, database_path)?;
        connection
            .pragma_update(None, "query_only", true)
            .map_err(|_| SqlcipherProbeFailure::CapabilityUnavailable)?;
        ensure_unlocked(&connection, deadline)?;

        let mut names_statement = connection
            .prepare(
                "SELECT name FROM sqlite_master \
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name LIMIT 4097",
            )
            .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
        let table_names = names_statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
        if table_names.len() > MAX_TABLES
            || table_names.iter().any(|name| {
                name.is_empty()
                    || name.len() > MAX_IDENTIFIER_BYTES
                    || name.chars().any(char::is_control)
            })
        {
            return Err(SqlcipherProbeFailure::UnsupportedSchema);
        }

        let mut tables = Vec::with_capacity(table_names.len());
        for name in table_names {
            remaining(deadline)?;
            let mut columns_statement = connection
                .prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid LIMIT 257")
                .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
            let columns = columns_statement
                .query_map([&name], |row| row.get::<_, String>(0))
                .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
            if columns.is_empty()
                || columns.len() > MAX_COLUMNS_PER_TABLE
                || columns.iter().any(|column| {
                    column.is_empty()
                        || column.len() > MAX_IDENTIFIER_BYTES
                        || column.chars().any(char::is_control)
                })
            {
                return Err(SqlcipherProbeFailure::UnsupportedSchema);
            }
            tables.push(SqlcipherSchemaTable { name, columns });
        }
        remaining(deadline)?;
        Ok(tables)
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

#[cfg(test)]
mod coordinator_tests {
    use std::{
        path::PathBuf,
        sync::{mpsc, Arc, Condvar, Mutex},
        thread,
        time::{Duration, Instant},
    };

    use pca_keychain::{
        CredentialError, CredentialStore, WechatKeyMaterial, WECHAT_CREDENTIAL_ACCOUNT,
        WECHAT_CREDENTIAL_SERVICE,
    };

    use super::{
        ReadOnlySqlcipherProbe, SourceCapabilities, SourceCursor, SqlcipherProbeFailure,
        SqlcipherProbeTarget, SqlcipherWechatSource,
    };
    use crate::source::WechatSource;

    const TEST_TIMEOUT: Duration = Duration::from_millis(50);
    const ASSERTION_TIMEOUT: Duration = Duration::from_millis(500);

    #[derive(Default)]
    struct BlockingGate {
        state: Mutex<BlockingState>,
        changed: Condvar,
    }

    #[derive(Default)]
    struct BlockingState {
        entered: bool,
        released: bool,
        completed: bool,
        calls: usize,
    }

    impl BlockingGate {
        fn enter_and_wait(&self) {
            let mut state = self.state.lock().expect("blocking gate lock");
            state.calls += 1;
            state.entered = true;
            self.changed.notify_all();
            while !state.released {
                state = self.changed.wait(state).expect("blocking gate wait");
            }
            state.completed = true;
            self.changed.notify_all();
        }

        fn wait_until_entered(&self) {
            let mut state = self.state.lock().expect("blocking gate lock");
            while !state.entered {
                state = self.changed.wait(state).expect("blocking gate wait");
            }
        }

        fn release(&self) {
            let mut state = self.state.lock().expect("blocking gate lock");
            state.released = true;
            self.changed.notify_all();
        }

        fn wait_until_completed(&self) {
            let mut state = self.state.lock().expect("blocking gate lock");
            while !state.completed {
                state = self.changed.wait(state).expect("blocking gate wait");
            }
        }

        fn call_count(&self) -> usize {
            self.state.lock().expect("blocking gate lock").calls
        }
    }

    struct BlockingKeyStore {
        gate: Arc<BlockingGate>,
        material: Vec<u8>,
    }

    impl CredentialStore for BlockingKeyStore {
        fn load(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, CredentialError> {
            assert_eq!(service, WECHAT_CREDENTIAL_SERVICE);
            assert_eq!(account, WECHAT_CREDENTIAL_ACCOUNT);
            self.gate.enter_and_wait();
            Ok(Some(self.material.clone()))
        }

        fn store(&self, _: &str, _: &str, _: &[u8]) -> Result<(), CredentialError> {
            Err(CredentialError::UnsupportedIdentity)
        }

        fn delete(&self, _: &str, _: &str) -> Result<(), CredentialError> {
            Err(CredentialError::UnsupportedIdentity)
        }
    }

    struct ImmediateKeyStore(Vec<u8>);

    impl CredentialStore for ImmediateKeyStore {
        fn load(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, CredentialError> {
            assert_eq!(service, WECHAT_CREDENTIAL_SERVICE);
            assert_eq!(account, WECHAT_CREDENTIAL_ACCOUNT);
            Ok(Some(self.0.clone()))
        }

        fn store(&self, _: &str, _: &str, _: &[u8]) -> Result<(), CredentialError> {
            Err(CredentialError::UnsupportedIdentity)
        }

        fn delete(&self, _: &str, _: &str) -> Result<(), CredentialError> {
            Err(CredentialError::UnsupportedIdentity)
        }
    }

    struct SuccessfulProbe;

    impl ReadOnlySqlcipherProbe for SuccessfulProbe {
        fn probe(
            &self,
            _: &SqlcipherProbeTarget,
            _: &WechatKeyMaterial,
            _: Duration,
        ) -> Result<SourceCapabilities, SqlcipherProbeFailure> {
            Ok(test_capabilities())
        }
    }

    struct BlockingSuccessfulProbe {
        gate: Arc<BlockingGate>,
    }

    impl ReadOnlySqlcipherProbe for BlockingSuccessfulProbe {
        fn probe(
            &self,
            _: &SqlcipherProbeTarget,
            _: &WechatKeyMaterial,
            _: Duration,
        ) -> Result<SourceCapabilities, SqlcipherProbeFailure> {
            self.gate.enter_and_wait();
            Ok(test_capabilities())
        }
    }

    #[test]
    fn blocking_keychain_does_not_extend_the_caller_deadline() {
        let gate = Arc::new(BlockingGate::default());
        let source = Arc::new(SqlcipherWechatSource::with_dependencies_and_timeout(
            BlockingKeyStore {
                gate: Arc::clone(&gate),
                material: encoded_material(),
            },
            SuccessfulProbe,
            test_target(),
            TEST_TIMEOUT,
        ));
        let (result_sender, result_receiver) = mpsc::channel();
        let caller_source = Arc::clone(&source);
        let caller = thread::spawn(move || {
            result_sender
                .send(caller_source.probe_blocking())
                .expect("send probe result");
        });
        gate.wait_until_entered();

        let result = result_receiver.recv_timeout(ASSERTION_TIMEOUT);
        gate.release();
        caller.join().expect("probe caller");
        let error = result
            .expect("caller must return while Keychain work remains blocked")
            .expect_err("blocked Keychain probe must time out");

        assert_eq!(error.code, "WECHAT_PROBE_TIMEOUT");
    }

    #[test]
    fn a_stuck_worker_rejects_further_probes_without_queueing_work() {
        let gate = Arc::new(BlockingGate::default());
        let source = Arc::new(SqlcipherWechatSource::with_dependencies_and_timeout(
            ImmediateKeyStore(encoded_material()),
            BlockingSuccessfulProbe {
                gate: Arc::clone(&gate),
            },
            test_target(),
            TEST_TIMEOUT,
        ));
        let first_source = Arc::clone(&source);
        let first = thread::spawn(move || first_source.probe_blocking());
        gate.wait_until_entered();
        let first_error = first
            .join()
            .expect("first probe caller")
            .expect_err("first probe must time out");

        let started_at = Instant::now();
        let second_error = source
            .probe_blocking()
            .expect_err("stuck worker must reject a later probe");
        let second_elapsed = started_at.elapsed();
        gate.release();

        assert_eq!(first_error.code, "WECHAT_PROBE_TIMEOUT");
        assert_eq!(second_error.code, "WECHAT_PROBE_TIMEOUT");
        assert!(second_elapsed < TEST_TIMEOUT);
        assert_eq!(gate.call_count(), 1, "no second job may be queued");
    }

    #[tokio::test]
    async fn a_late_success_after_timeout_never_restores_source_proof() {
        let gate = Arc::new(BlockingGate::default());
        let source = Arc::new(SqlcipherWechatSource::with_dependencies_and_timeout(
            ImmediateKeyStore(encoded_material()),
            BlockingSuccessfulProbe {
                gate: Arc::clone(&gate),
            },
            test_target(),
            TEST_TIMEOUT,
        ));
        let caller_source = Arc::clone(&source);
        let caller = thread::spawn(move || caller_source.probe_blocking());
        gate.wait_until_entered();
        let error = caller
            .join()
            .expect("probe caller")
            .expect_err("blocked probe must time out");

        gate.release();
        gate.wait_until_completed();
        let Err(read_error) = source.read_after(&SourceCursor).await else {
            panic!("late success must not restore proof");
        };

        assert_eq!(error.code, "WECHAT_PROBE_TIMEOUT");
        assert_eq!(read_error.code, "WECHAT_WAITING_SOURCE");
    }

    fn encoded_material() -> Vec<u8> {
        WechatKeyMaterial::new("test-account-proof", [0x2a; 32])
            .expect("valid test material")
            .encode()
            .expect("encoded test material")
    }

    fn test_target() -> SqlcipherProbeTarget {
        SqlcipherProbeTarget::new(
            PathBuf::from("unused-test-source.db"),
            "test-version",
            1,
            1,
            "SELECT source_version",
            "SELECT schema_version",
            "SELECT account_id",
        )
        .expect("valid test target")
    }

    fn test_capabilities() -> SourceCapabilities {
        SourceCapabilities {
            source_version: "test-version".to_owned(),
            schema_version: 1,
        }
    }
}
