use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

use pca_domain::{AgentStatus, BridgeStatus, CollectorState, EventCommit, EventEnvelope};
use rusqlite::Connection;
use tokio::sync::{mpsc, oneshot};

use crate::{migrations, repository, CommunicationMessageCommit, DbError, DbHealth, PairingState};

const REQUEST_CAPACITY: usize = 64;

#[cfg(feature = "process-test-hooks")]
#[derive(Clone, Debug)]
pub(crate) struct ProcessTestBarrier {
    pub(crate) ready: PathBuf,
    pub(crate) release: PathBuf,
}

#[cfg(feature = "process-test-hooks")]
#[derive(Clone, Debug)]
pub struct ProcessTestHooks {
    pub(crate) event_outbox: Option<ProcessTestBarrier>,
    pub(crate) collector_commit: Option<ProcessTestBarrier>,
}

#[cfg(feature = "process-test-hooks")]
impl ProcessTestHooks {
    /// Configures a deterministic, test-only rendezvous inside an Event/Outbox transaction.
    ///
    /// This API does not exist unless the `process-test-hooks` Cargo feature is enabled.
    ///
    /// # Errors
    ///
    /// Returns an error unless both paths are absolute, distinct, and share one parent.
    pub fn new(ready: PathBuf, release: PathBuf) -> Result<Self, DbError> {
        let event_outbox = validate_process_test_barrier(ready, release)?;
        Ok(Self {
            event_outbox: Some(event_outbox),
            collector_commit: None,
        })
    }

    /// Configures only the deterministic Collector commit rendezvous.
    ///
    /// # Errors
    ///
    /// Returns an error unless both paths are absolute, distinct, and share one parent.
    pub fn collector_commit(ready: PathBuf, release: PathBuf) -> Result<Self, DbError> {
        let collector_commit = validate_process_test_barrier(ready, release)?;
        Ok(Self {
            event_outbox: None,
            collector_commit: Some(collector_commit),
        })
    }

    /// Adds a deterministic Collector commit rendezvous without changing the Event/Outbox hook.
    ///
    /// # Errors
    ///
    /// Returns an error unless both paths are absolute, distinct, and share one parent.
    pub fn with_collector_commit_barrier(
        mut self,
        ready: PathBuf,
        release: PathBuf,
    ) -> Result<Self, DbError> {
        self.collector_commit = Some(validate_process_test_barrier(ready, release)?);
        Ok(self)
    }
}

#[cfg(feature = "process-test-hooks")]
fn validate_process_test_barrier(
    ready: PathBuf,
    release: PathBuf,
) -> Result<ProcessTestBarrier, DbError> {
    if !ready.is_absolute()
        || !release.is_absolute()
        || ready == release
        || ready.parent() != release.parent()
    {
        return Err(DbError::sqlite(
            "configure process test barrier",
            "barrier paths must be distinct absolute siblings",
        ));
    }
    Ok(ProcessTestBarrier { ready, release })
}

#[derive(Default)]
struct ActorOptions {
    #[cfg(feature = "process-test-hooks")]
    process_test_hooks: Option<ProcessTestHooks>,
}

enum Request {
    AppendEvent {
        event: Box<EventEnvelope>,
        response: oneshot::Sender<Result<(), DbError>>,
    },
    CommitEvents {
        commit: Box<EventCommit>,
        response: oneshot::Sender<Result<(), DbError>>,
    },
    SetAgentState {
        agent_status: AgentStatus,
        bridge_status: BridgeStatus,
        local_healthy: bool,
        updated_at_ms: i64,
        response: oneshot::Sender<Result<(), DbError>>,
    },
    LoadCollectorStates {
        response: oneshot::Sender<Result<Vec<CollectorState>, DbError>>,
    },
    UpsertCollectorState {
        state: Box<CollectorState>,
        response: oneshot::Sender<Result<(), DbError>>,
    },
    LoadPairingState {
        response: oneshot::Sender<Result<Option<PairingState>, DbError>>,
    },
    SavePairingState {
        state: Box<PairingState>,
        response: oneshot::Sender<Result<(), DbError>>,
    },
    SaveControlRevision {
        applied_control_revision: u64,
        response: oneshot::Sender<Result<(), DbError>>,
    },
    ClearPairingState {
        response: oneshot::Sender<Result<(), DbError>>,
    },
    ClearPairingStateAndDisableSensitiveCollectors {
        response: oneshot::Sender<Result<(), DbError>>,
    },
    Health {
        response: oneshot::Sender<Result<DbHealth, DbError>>,
    },
    Checkpoint {
        response: oneshot::Sender<Result<(), DbError>>,
    },
    CountEventAndOutbox {
        event_id: String,
        response: oneshot::Sender<Result<(u64, u64), DbError>>,
    },
    ActiveOutboxDepth {
        response: oneshot::Sender<Result<u64, DbError>>,
    },
    LoadPendingSystemEvents {
        limit: u16,
        response: oneshot::Sender<Result<Vec<EventEnvelope>, DbError>>,
    },
    AcknowledgeSystemEvents {
        event_ids: Vec<String>,
        response: oneshot::Sender<Result<(), DbError>>,
    },
    CommitCommunicationMessage {
        commit: Box<CommunicationMessageCommit>,
        response: oneshot::Sender<Result<(), DbError>>,
    },
    LoadPendingCommunicationEvents {
        limit: u16,
        response: oneshot::Sender<Result<Vec<EventEnvelope>, DbError>>,
    },
    AcknowledgeCommunicationEvents {
        event_ids: Vec<String>,
        response: oneshot::Sender<Result<(), DbError>>,
    },
}

impl Request {
    fn is_cancelled(&self) -> bool {
        match self {
            Self::AppendEvent { response, .. }
            | Self::CommitEvents { response, .. }
            | Self::SetAgentState { response, .. }
            | Self::UpsertCollectorState { response, .. }
            | Self::SavePairingState { response, .. }
            | Self::SaveControlRevision { response, .. }
            | Self::ClearPairingState { response }
            | Self::ClearPairingStateAndDisableSensitiveCollectors { response }
            | Self::Checkpoint { response }
            | Self::AcknowledgeSystemEvents { response, .. }
            | Self::CommitCommunicationMessage { response, .. }
            | Self::AcknowledgeCommunicationEvents { response, .. } => response.is_closed(),
            Self::LoadCollectorStates { response } => response.is_closed(),
            Self::LoadPairingState { response } => response.is_closed(),
            Self::Health { response } => response.is_closed(),
            Self::CountEventAndOutbox { response, .. } => response.is_closed(),
            Self::ActiveOutboxDepth { response } => response.is_closed(),
            Self::LoadPendingSystemEvents { response, .. }
            | Self::LoadPendingCommunicationEvents { response, .. } => response.is_closed(),
        }
    }
}

/// Async handle to the single thread that owns the local `SQLite` connection.
///
/// Dropping the handle never waits for the owner thread: it closes the bounded request queue and
/// detaches the thread, which skips canceled queued requests and exits after any request already in
/// progress returns. Call [`Self::shutdown`] when the caller must wait for connection close and a
/// deterministic thread join.
pub struct DbActorHandle {
    requests: Option<mpsc::Sender<Request>>,
    owner_stopped: Option<oneshot::Receiver<()>>,
    owner_thread: Option<thread::JoinHandle<()>>,
}

impl DbActorHandle {
    /// Opens and verifies a local database before accepting requests.
    ///
    /// # Errors
    ///
    /// Returns a migration, integrity, configuration, or actor startup error.
    pub async fn open(path: &Path, app_version: &str) -> Result<Self, DbError> {
        Self::open_with_options(path, app_version, ActorOptions::default()).await
    }

    /// Opens a database with a feature-gated deterministic process-test barrier.
    ///
    /// # Errors
    ///
    /// Returns the same startup errors as [`Self::open`].
    #[cfg(feature = "process-test-hooks")]
    pub async fn open_with_process_test_hooks(
        path: &Path,
        app_version: &str,
        hooks: ProcessTestHooks,
    ) -> Result<Self, DbError> {
        Self::open_with_options(
            path,
            app_version,
            ActorOptions {
                process_test_hooks: Some(hooks),
            },
        )
        .await
    }

    async fn open_with_options(
        path: &Path,
        app_version: &str,
        options: ActorOptions,
    ) -> Result<Self, DbError> {
        let (request_sender, request_receiver) = mpsc::channel(REQUEST_CAPACITY);
        let (startup_sender, startup_receiver) = oneshot::channel();
        let (owner_stopped_sender, owner_stopped_receiver) = oneshot::channel();
        let path = path.to_owned();
        let communication_spool_root = Self::communication_spool_root(&path);
        let app_version = app_version.to_owned();
        let owner_thread = thread::Builder::new()
            .name("pca-sqlite-owner".to_owned())
            .spawn(move || {
                let result = open_connection(&path, &app_version);
                match result {
                    Ok(connection) => match ensure_private_spool_root(&communication_spool_root) {
                        Ok(()) => {
                            let _ = startup_sender.send(Ok(()));
                            run(
                                connection,
                                request_receiver,
                                &options,
                                &communication_spool_root,
                            );
                        }
                        Err(error) => {
                            let _ = startup_sender.send(Err(error));
                        }
                    },
                    Err(error) => {
                        let _ = startup_sender.send(Err(error));
                    }
                }
                let _ = owner_stopped_sender.send(());
            })
            .map_err(|error| DbError::sqlite("spawn database owner thread", error))?;

        match startup_receiver.await {
            Ok(Ok(())) => Ok(Self {
                requests: Some(request_sender),
                owner_stopped: Some(owner_stopped_receiver),
                owner_thread: Some(owner_thread),
            }),
            Ok(Err(error)) => {
                drop(request_sender);
                owner_thread.join().map_err(|_| DbError::ActorThreadPanic)?;
                Err(error)
            }
            Err(_) => {
                drop(request_sender);
                owner_thread.join().map_err(|_| DbError::ActorThreadPanic)?;
                Err(DbError::ActorUnavailable)
            }
        }
    }

    /// Atomically appends one Event and its stable Outbox intent.
    ///
    /// # Errors
    ///
    /// Returns an actor, serialization, constraint, lock, or transaction error.
    pub async fn append_event_with_outbox(&self, event: &EventEnvelope) -> Result<(), DbError> {
        let (response_sender, response_receiver) = oneshot::channel();
        self.send(Request::AppendEvent {
            event: Box::new(event.clone()),
            response: response_sender,
        })
        .await?;
        receive(response_receiver).await
    }

    /// Atomically commits one bounded Event batch, stable Outbox rows, and optional Collector state.
    ///
    /// # Errors
    ///
    /// Returns an actor, serialization, constraint, lock, or transaction error.
    pub async fn commit_events(&self, commit: &EventCommit) -> Result<(), DbError> {
        let (response_sender, response_receiver) = oneshot::channel();
        self.send(Request::CommitEvents {
            commit: Box::new(commit.clone()),
            response: response_sender,
        })
        .await?;
        receive(response_receiver).await
    }

    /// Replaces the durable singleton runtime state using canonical domain enums.
    ///
    /// # Errors
    ///
    /// Returns an actor or `SQLite` write error.
    pub async fn set_agent_state(
        &self,
        agent_status: AgentStatus,
        bridge_status: BridgeStatus,
        local_healthy: bool,
        updated_at_ms: i64,
    ) -> Result<(), DbError> {
        let (response_sender, response_receiver) = oneshot::channel();
        self.send(Request::SetAgentState {
            agent_status,
            bridge_status,
            local_healthy,
            updated_at_ms,
            response: response_sender,
        })
        .await?;
        receive(response_receiver).await
    }

    /// Loads all durable Collector runtime state ordered by Collector key.
    ///
    /// The persisted status is restart data; Agent Core remains responsible for recomputing
    /// runtime policy before starting a Collector.
    ///
    /// # Errors
    ///
    /// Returns an actor, decoding, or `SQLite` query error.
    pub async fn load_collector_states(&self) -> Result<Vec<CollectorState>, DbError> {
        let (response_sender, response_receiver) = oneshot::channel();
        self.send(Request::LoadCollectorStates {
            response: response_sender,
        })
        .await?;
        receive(response_receiver).await
    }

    /// Inserts or replaces one durable Collector runtime state row.
    ///
    /// # Errors
    ///
    /// Returns an actor, conversion, constraint, or `SQLite` write error.
    pub async fn upsert_collector_state(&self, state: &CollectorState) -> Result<(), DbError> {
        let (response_sender, response_receiver) = oneshot::channel();
        self.send(Request::UpsertCollectorState {
            state: Box::new(state.clone()),
            response: response_sender,
        })
        .await?;
        receive(response_receiver).await
    }

    /// Loads the non-secret pairing pointer, or `None` while unpaired.
    ///
    /// # Errors
    ///
    /// Returns an actor, decoding, or `SQLite` query error.
    pub async fn load_pairing_state(&self) -> Result<Option<PairingState>, DbError> {
        let (response_sender, response_receiver) = oneshot::channel();
        self.send(Request::LoadPairingState {
            response: response_sender,
        })
        .await?;
        receive(response_receiver).await
    }

    /// Saves a reference only after the caller has validated the Keychain credential.
    ///
    /// # Errors
    ///
    /// Returns an actor, constraint, conversion, or `SQLite` write error.
    pub async fn save_pairing_state(&self, state: &PairingState) -> Result<(), DbError> {
        let (response_sender, response_receiver) = oneshot::channel();
        self.send(Request::SavePairingState {
            state: Box::new(state.clone()),
            response: response_sender,
        })
        .await?;
        receive(response_receiver).await
    }

    /// Persists a complete applied control revision without allowing rollback.
    ///
    /// # Errors
    ///
    /// Returns an actor, conversion, or `SQLite` write error.
    pub async fn save_control_revision(
        &self,
        applied_control_revision: u64,
    ) -> Result<(), DbError> {
        let (response_sender, response_receiver) = oneshot::channel();
        self.send(Request::SaveControlRevision {
            applied_control_revision,
            response: response_sender,
        })
        .await?;
        receive(response_receiver).await
    }

    /// Deletes the local pairing pointer while leaving all credential deletion to Keychain.
    ///
    /// # Errors
    ///
    /// Returns an actor or `SQLite` write error.
    pub async fn clear_pairing_state(&self) -> Result<(), DbError> {
        let (response_sender, response_receiver) = oneshot::channel();
        self.send(Request::ClearPairingState {
            response: response_sender,
        })
        .await?;
        receive(response_receiver).await
    }

    /// Atomically removes pairing and durably keeps future sensitive Collector sources disabled.
    ///
    /// S1B has no Network or `WeChat` source implementation; these rows prevent a later runtime
    /// from treating a revoked pairing as an authorization to start either source.
    ///
    /// # Errors
    ///
    /// Returns an actor, transaction, or `SQLite` write error.
    pub async fn clear_pairing_state_and_disable_sensitive_collectors(
        &self,
    ) -> Result<(), DbError> {
        let (response_sender, response_receiver) = oneshot::channel();
        self.send(Request::ClearPairingStateAndDisableSensitiveCollectors {
            response: response_sender,
        })
        .await?;
        receive(response_receiver).await
    }

    /// Runs fresh integrity, foreign-key, and schema-version checks.
    ///
    /// # Errors
    ///
    /// Returns an actor, integrity, foreign-key, or schema-version error.
    pub async fn health(&self) -> Result<DbHealth, DbError> {
        let (response_sender, response_receiver) = oneshot::channel();
        self.send(Request::Health {
            response: response_sender,
        })
        .await?;
        receive(response_receiver).await
    }

    /// Truncates the write-ahead log after a successful checkpoint.
    ///
    /// # Errors
    ///
    /// Returns an actor or `SQLite` checkpoint error.
    pub async fn checkpoint(&self) -> Result<(), DbError> {
        let (response_sender, response_receiver) = oneshot::channel();
        self.send(Request::Checkpoint {
            response: response_sender,
        })
        .await?;
        receive(response_receiver).await
    }

    /// Counts the durable Event and Outbox rows for one Event identifier.
    ///
    /// # Errors
    ///
    /// Returns an actor or `SQLite` query error.
    pub async fn count_event_and_outbox(&self, event_id: &str) -> Result<(u64, u64), DbError> {
        let (response_sender, response_receiver) = oneshot::channel();
        self.send(Request::CountEventAndOutbox {
            event_id: event_id.to_owned(),
            response: response_sender,
        })
        .await?;
        receive(response_receiver).await
    }

    /// Counts Outbox rows that have not reached the acknowledged terminal state.
    ///
    /// # Errors
    ///
    /// Returns an actor or `SQLite` query error.
    pub async fn active_outbox_depth(&self) -> Result<u64, DbError> {
        let (response_sender, response_receiver) = oneshot::channel();
        self.send(Request::ActiveOutboxDepth {
            response: response_sender,
        })
        .await?;
        receive(response_receiver).await
    }

    /// Loads at most 200 pending, non-sensitive System Collector events in Outbox order.
    ///
    /// Other event classes remain pending for their own future sync protocol.
    ///
    /// # Errors
    ///
    /// Returns an actor, decoding, or `SQLite` query error.
    pub async fn load_pending_system_events(
        &self,
        limit: u16,
    ) -> Result<Vec<EventEnvelope>, DbError> {
        if limit == 0 || limit > 200 {
            return Err(DbError::sqlite(
                "load pending system events",
                "limit must be 1 through 200",
            ));
        }
        let (response_sender, response_receiver) = oneshot::channel();
        self.send(Request::LoadPendingSystemEvents {
            limit,
            response: response_sender,
        })
        .await?;
        receive(response_receiver).await
    }

    /// Marks only the exact System events accepted by the authenticated Cloud endpoint as acknowledged.
    ///
    /// # Errors
    ///
    /// Returns an actor, transaction, or `SQLite` write error.
    pub async fn acknowledge_system_events(&self, event_ids: &[String]) -> Result<(), DbError> {
        if event_ids.is_empty() || event_ids.len() > 200 {
            return Err(DbError::sqlite(
                "acknowledge system events",
                "event IDs must contain 1 through 200 items",
            ));
        }
        let (response_sender, response_receiver) = oneshot::channel();
        self.send(Request::AcknowledgeSystemEvents {
            event_ids: event_ids.to_vec(),
            response: response_sender,
        })
        .await?;
        receive(response_receiver).await
    }

    /// Returns the app-private attachment spool root tied to this local database path.
    #[must_use]
    pub fn communication_spool_root(database_path: &Path) -> PathBuf {
        database_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("communication-spool")
    }

    /// Atomically stores one communication Event, its local projection, Cursor, private spool
    /// metadata, and stable Outbox intent.
    ///
    /// # Errors
    ///
    /// Returns an actor, validation, serialization, constraint, or transaction error. Any error
    /// leaves no partial rows from this commit.
    pub async fn commit_communication_message(
        &self,
        commit: &CommunicationMessageCommit,
    ) -> Result<(), DbError> {
        let (response_sender, response_receiver) = oneshot::channel();
        self.send(Request::CommitCommunicationMessage {
            commit: Box::new(commit.clone()),
            response: response_sender,
        })
        .await?;
        receive(response_receiver).await
    }

    /// Loads at most 200 pending, high-sensitivity communication events in Outbox order.
    ///
    /// # Errors
    ///
    /// Returns an actor, decoding, or `SQLite` query error.
    pub async fn load_pending_communication_events(
        &self,
        limit: u16,
    ) -> Result<Vec<EventEnvelope>, DbError> {
        validate_communication_limit("load pending communication events", limit)?;
        let (response_sender, response_receiver) = oneshot::channel();
        self.send(Request::LoadPendingCommunicationEvents {
            limit,
            response: response_sender,
        })
        .await?;
        receive(response_receiver).await
    }

    /// Marks only the exact pending communication events accepted by the dedicated Cloud protocol.
    ///
    /// # Errors
    ///
    /// Returns an actor, transaction, or `SQLite` write error.
    pub async fn acknowledge_communication_events(
        &self,
        event_ids: &[String],
    ) -> Result<(), DbError> {
        if event_ids.is_empty() || event_ids.len() > 200 {
            return Err(DbError::sqlite(
                "acknowledge communication events",
                "event IDs must contain 1 through 200 items",
            ));
        }
        let (response_sender, response_receiver) = oneshot::channel();
        self.send(Request::AcknowledgeCommunicationEvents {
            event_ids: event_ids.to_vec(),
            response: response_sender,
        })
        .await?;
        receive(response_receiver).await
    }

    /// Closes the request queue, waits without blocking the async executor, and joins the owner.
    ///
    /// Requests whose response futures were canceled are skipped before they touch `SQLite`.
    /// When this method returns, the connection has been dropped and the owner thread has exited.
    ///
    /// # Errors
    ///
    /// Returns an actor error if the owner exits without signaling, or a panic error if joining
    /// the owner thread reports a panic.
    pub async fn shutdown(mut self) -> Result<(), DbError> {
        self.requests.take();
        let owner_stopped = self.owner_stopped.take().ok_or(DbError::ActorUnavailable)?;
        let owner_thread = self.owner_thread.take().ok_or(DbError::ActorUnavailable)?;
        let stopped_result = owner_stopped.await;
        while !owner_thread.is_finished() {
            tokio::task::yield_now().await;
        }
        owner_thread.join().map_err(|_| DbError::ActorThreadPanic)?;
        stopped_result.map_err(|_| DbError::ActorUnavailable)
    }

    async fn send(&self, request: Request) -> Result<(), DbError> {
        self.requests
            .as_ref()
            .ok_or(DbError::ActorUnavailable)?
            .send(request)
            .await
            .map_err(|_| DbError::ActorUnavailable)
    }
}

impl Drop for DbActorHandle {
    fn drop(&mut self) {
        self.requests.take();
        if let Some(owner_thread) = self.owner_thread.take() {
            if owner_thread.is_finished() {
                let _ = owner_thread.join();
            }
        }
    }
}

async fn receive<T>(receiver: oneshot::Receiver<Result<T, DbError>>) -> Result<T, DbError> {
    receiver.await.map_err(|_| DbError::ActorUnavailable)?
}

fn open_connection(path: &Path, app_version: &str) -> Result<Connection, DbError> {
    let mut connection =
        Connection::open(path).map_err(|error| DbError::startup_sqlite("open database", error))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| DbError::sqlite("restrict database permissions", error))?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(|error| DbError::startup_sqlite("set busy timeout", error))?;
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .map_err(|error| DbError::startup_sqlite("enable foreign keys", error))?;
    let journal_mode = connection
        .query_row("PRAGMA journal_mode = WAL", [], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|error| DbError::startup_sqlite("enable WAL", error))?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        return Err(DbError::sqlite(
            "enable WAL",
            format!("unexpected journal mode {journal_mode}"),
        ));
    }
    connection
        .pragma_update(None, "synchronous", "NORMAL")
        .map_err(|error| DbError::startup_sqlite("set synchronous mode", error))?;
    migrations::run(&mut connection, app_version)?;
    repository::health(&connection)?;
    repository::smoke_queries(&connection)?;
    Ok(connection)
}

fn ensure_private_spool_root(path: &Path) -> Result<(), DbError> {
    fs::create_dir_all(path)
        .map_err(|error| DbError::sqlite("create communication spool root", error))?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| DbError::sqlite("inspect communication spool root", error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(DbError::sqlite(
            "inspect communication spool root",
            "communication spool root must be a private directory",
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| DbError::sqlite("restrict communication spool permissions", error))
}

#[cfg_attr(not(feature = "process-test-hooks"), allow(unused_variables))]
#[allow(clippy::too_many_lines)] // Request variants preserve a single SQLite-owner dispatch point.
fn run(
    mut connection: Connection,
    mut requests: mpsc::Receiver<Request>,
    options: &ActorOptions,
    communication_spool_root: &Path,
) {
    while let Some(request) = requests.blocking_recv() {
        if request.is_cancelled() {
            continue;
        }
        match request {
            Request::AppendEvent { event, response } => {
                let _ = response.send(repository::append_event_with_outbox(
                    &mut connection,
                    &event,
                    #[cfg(feature = "process-test-hooks")]
                    options.process_test_hooks.as_ref(),
                ));
            }
            Request::CommitEvents { commit, response } => {
                let _ = response.send(repository::commit_events(
                    &mut connection,
                    &commit,
                    #[cfg(feature = "process-test-hooks")]
                    options.process_test_hooks.as_ref(),
                ));
            }
            Request::SetAgentState {
                agent_status,
                bridge_status,
                local_healthy,
                updated_at_ms,
                response,
            } => {
                let _ = response.send(repository::set_agent_state(
                    &connection,
                    agent_status,
                    bridge_status,
                    local_healthy,
                    updated_at_ms,
                ));
            }
            Request::LoadCollectorStates { response } => {
                let _ = response.send(repository::load_collector_states(&connection));
            }
            Request::UpsertCollectorState { state, response } => {
                let _ = response.send(repository::upsert_collector_state_in(&connection, &state));
            }
            Request::LoadPairingState { response } => {
                let _ = response.send(repository::load_pairing_state(&connection));
            }
            Request::SavePairingState { state, response } => {
                let _ = response.send(repository::save_pairing_state(&connection, &state));
            }
            Request::SaveControlRevision {
                applied_control_revision,
                response,
            } => {
                let _ = response.send(repository::save_control_revision(
                    &connection,
                    applied_control_revision,
                ));
            }
            Request::ClearPairingState { response } => {
                let _ = response.send(repository::clear_pairing_state(&connection));
            }
            Request::ClearPairingStateAndDisableSensitiveCollectors { response } => {
                let _ = response.send(
                    repository::clear_pairing_state_and_disable_sensitive_collectors(
                        &mut connection,
                    ),
                );
            }
            Request::Health { response } => {
                let _ = response.send(repository::health(&connection));
            }
            Request::Checkpoint { response } => {
                let _ = response.send(repository::checkpoint(&connection));
            }
            Request::CountEventAndOutbox { event_id, response } => {
                let _ = response.send(repository::count_event_and_outbox(&connection, &event_id));
            }
            Request::ActiveOutboxDepth { response } => {
                let _ = response.send(repository::active_outbox_depth(&connection));
            }
            Request::LoadPendingSystemEvents { limit, response } => {
                let _ = response.send(repository::load_pending_system_events(&connection, limit));
            }
            Request::AcknowledgeSystemEvents {
                event_ids,
                response,
            } => {
                let _ = response.send(repository::acknowledge_system_events(
                    &mut connection,
                    &event_ids,
                ));
            }
            Request::CommitCommunicationMessage { commit, response } => {
                let _ = response.send(repository::commit_communication_message(
                    &mut connection,
                    communication_spool_root,
                    &commit,
                ));
            }
            Request::LoadPendingCommunicationEvents { limit, response } => {
                let _ = response.send(repository::load_pending_communication_events(
                    &connection,
                    limit,
                ));
            }
            Request::AcknowledgeCommunicationEvents {
                event_ids,
                response,
            } => {
                let _ = response.send(repository::acknowledge_communication_events(
                    &mut connection,
                    &event_ids,
                ));
            }
        }
    }
}

fn validate_communication_limit(operation: &'static str, limit: u16) -> Result<(), DbError> {
    if (1..=200).contains(&limit) {
        Ok(())
    } else {
        Err(DbError::sqlite(operation, "limit must be 1 through 200"))
    }
}
