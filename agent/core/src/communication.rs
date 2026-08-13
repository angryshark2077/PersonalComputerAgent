//! Paired communication Provider lifecycle and private local media spooling.

use std::{
    collections::BTreeSet,
    fmt,
    fs::File,
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use pca_db_local::{
    CommunicationAttachmentSpoolReference, CommunicationMessageCommit, DbActorHandle, DbError,
};
use pca_domain::{
    CollectorState, CollectorStatus, DomainError, EventEnvelope, MessageKind, Sensitivity,
};
use pca_provider_contracts::{
    CommunicationProvider, CommunicationProviderFactory, CompletedMediaSource,
    NormalizedCommunicationRecord,
};
use rustix::fs::{AtFlags, Mode, OFlags};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{mpsc, oneshot, watch, OwnedRwLockReadGuard, RwLock},
    task::JoinHandle,
    time,
};
use uuid::Uuid;

pub const OUTBOX_HIGH_WATER: u64 = 10_000;
pub const OUTBOX_LOW_WATER: u64 = 8_000;
pub const SPOOL_HARD_LIMIT_BYTES: u64 = 6 * 1024 * 1024 * 1024;
pub const SPOOL_RESUME_BELOW_BYTES: u64 = 5 * 1024 * 1024 * 1024;

const POLL_INTERVAL: Duration = Duration::from_secs(30);
const MONITOR_INTERVAL: Duration = Duration::from_secs(30);
const RETRY_DELAYS: [Duration; 5] = [
    Duration::from_secs(30),
    Duration::from_mins(1),
    Duration::from_mins(2),
    Duration::from_mins(4),
    Duration::from_mins(5),
];
const COMMAND_CAPACITY: usize = 8;
const COLLECTOR_KEY: &str = "communication.wechat";
const STOP_FAILED: &str = "WECHAT_STOP_FAILED";

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CommunicationIdentity {
    workspace_id: Uuid,
    device_id: Uuid,
}

impl CommunicationIdentity {
    /// Validates the paired identity consumed by communication persistence.
    ///
    /// # Errors
    ///
    /// Returns a redacted control error for malformed or nil identifiers.
    pub fn try_new(workspace_id: &str, device_id: &str) -> Result<Self, CommunicationRuntimeError> {
        let workspace_id =
            Uuid::parse_str(workspace_id).map_err(|_| CommunicationRuntimeError::InvalidControl)?;
        let device_id =
            Uuid::parse_str(device_id).map_err(|_| CommunicationRuntimeError::InvalidControl)?;
        if workspace_id.is_nil() || device_id.is_nil() {
            return Err(CommunicationRuntimeError::InvalidControl);
        }
        Ok(Self {
            workspace_id,
            device_id,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CommunicationControl {
    identity: Option<CommunicationIdentity>,
    configuration_revision: u64,
    enabled: bool,
}

impl CommunicationControl {
    #[must_use]
    pub const fn unpaired() -> Self {
        Self {
            identity: None,
            configuration_revision: 0,
            enabled: false,
        }
    }

    /// Builds one already validated exact-v2 control transition.
    ///
    /// # Errors
    ///
    /// Returns a redacted control error when a paired revision is zero.
    pub const fn paired(
        identity: CommunicationIdentity,
        configuration_revision: u64,
        enabled: bool,
    ) -> Result<Self, CommunicationRuntimeError> {
        if configuration_revision == 0 {
            return Err(CommunicationRuntimeError::InvalidControl);
        }
        Ok(Self {
            identity: Some(identity),
            configuration_revision,
            enabled,
        })
    }

    const fn active(self) -> bool {
        self.identity.is_some() && self.enabled
    }
}

#[derive(Debug)]
pub enum CommunicationRuntimeError {
    Database(DbError),
    InvalidControl,
    StaleControl,
    AuthorizationReadOnly,
    QueueClosed,
    WorkerStopped,
    ProviderStopFailed,
    Clock,
}

impl fmt::Display for CommunicationRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "communication database: {error}"),
            Self::InvalidControl => formatter.write_str("communication control is invalid"),
            Self::StaleControl => formatter.write_str("communication control revision is stale"),
            Self::AuthorizationReadOnly => {
                formatter.write_str("communication authorization is read-only")
            }
            Self::QueueClosed => formatter.write_str("communication command queue is closed"),
            Self::WorkerStopped => formatter.write_str("communication worker stopped"),
            Self::ProviderStopFailed => formatter.write_str("communication provider stop failed"),
            Self::Clock => formatter.write_str("communication clock unavailable"),
        }
    }
}

impl std::error::Error for CommunicationRuntimeError {}

impl From<DbError> for CommunicationRuntimeError {
    fn from(error: DbError) -> Self {
        Self::Database(error)
    }
}

enum Command {
    Apply {
        control: CommunicationControl,
        response: oneshot::Sender<Result<(), CommunicationRuntimeError>>,
    },
}

enum Operation<T> {
    Completed(T),
    Command(Option<Command>),
    Authorization(CommunicationControl),
}

enum PollWait {
    Elapsed,
    CommandApplied,
    Closed,
}

/// Joined owner of the single communication Provider task.
pub struct CommunicationRuntime {
    control_writer: Option<CommunicationAuthorization>,
    commands: Option<mpsc::Sender<Command>>,
    worker: Option<JoinHandle<Result<(), CommunicationRuntimeError>>>,
}

#[derive(Clone, Copy)]
struct AuthorizationState {
    control: CommunicationControl,
    suspended_control: Option<CommunicationControl>,
    highest_revision: u64,
    generation: u64,
    owner_epoch: u64,
}

/// Authoritative, monotonic communication authorization observed directly by the runtime.
#[derive(Clone)]
pub struct CommunicationAuthorization {
    state: Arc<RwLock<AuthorizationState>>,
    updates: watch::Sender<AuthorizationState>,
}

impl CommunicationAuthorization {
    #[must_use]
    pub fn new() -> Self {
        let initial = AuthorizationState {
            control: CommunicationControl::unpaired(),
            suspended_control: None,
            highest_revision: 0,
            generation: 0,
            owner_epoch: 0,
        };
        let (updates, _) = watch::channel(initial);
        Self {
            state: Arc::new(RwLock::new(initial)),
            updates,
        }
    }

    /// Publishes an exact-v2 control only after its required durable revision is available.
    ///
    /// # Errors
    ///
    /// Returns a stale or invalid control error without changing current authorization.
    pub async fn apply_persisted(
        &self,
        control: CommunicationControl,
    ) -> Result<(), CommunicationRuntimeError> {
        if control.identity.is_none() || control.configuration_revision == 0 {
            return Err(CommunicationRuntimeError::InvalidControl);
        }
        let mut state = self.state.write().await;
        if control.configuration_revision == state.highest_revision {
            if control == state.control {
                return Ok(());
            }
            if state.control.identity.is_none() && state.suspended_control == Some(control) {
                state.generation = state
                    .generation
                    .checked_add(1)
                    .ok_or(CommunicationRuntimeError::InvalidControl)?;
                state.control = control;
                state.suspended_control = None;
                self.updates.send_replace(*state);
                return Ok(());
            }
        }
        if control.configuration_revision <= state.highest_revision {
            return Err(CommunicationRuntimeError::StaleControl);
        }
        state.highest_revision = control.configuration_revision;
        state.generation = state
            .generation
            .checked_add(1)
            .ok_or(CommunicationRuntimeError::InvalidControl)?;
        state.control = control;
        state.suspended_control = None;
        self.updates.send_replace(*state);
        Ok(())
    }

    /// Immediately invalidates every outstanding communication operation and commit attempt.
    pub async fn disable(&self) {
        let mut state = self.state.write().await;
        state.generation = state.generation.saturating_add(1);
        state.control = CommunicationControl::unpaired();
        state.suspended_control = None;
        self.updates.send_replace(*state);
    }

    pub(crate) async fn replace_owner(&self) -> u64 {
        let mut state = self.state.write().await;
        state.owner_epoch = state.owner_epoch.saturating_add(1);
        state.highest_revision = 0;
        state.generation = state.generation.saturating_add(1);
        state.control = CommunicationControl::unpaired();
        state.suspended_control = None;
        self.updates.send_replace(*state);
        state.owner_epoch
    }

    pub(crate) async fn owner_epoch(&self) -> u64 {
        self.state.read().await.owner_epoch
    }

    pub(crate) async fn apply_persisted_for_owner(
        &self,
        owner_epoch: u64,
        control: CommunicationControl,
    ) -> Result<bool, CommunicationRuntimeError> {
        if control.identity.is_none() || control.configuration_revision == 0 {
            return Err(CommunicationRuntimeError::InvalidControl);
        }
        let mut state = self.state.write().await;
        if state.owner_epoch != owner_epoch {
            return Ok(false);
        }
        if control.configuration_revision == state.highest_revision {
            if control == state.control {
                return Ok(true);
            }
            if state.control.identity.is_none() && state.suspended_control == Some(control) {
                state.generation = state
                    .generation
                    .checked_add(1)
                    .ok_or(CommunicationRuntimeError::InvalidControl)?;
                state.control = control;
                state.suspended_control = None;
                self.updates.send_replace(*state);
                return Ok(true);
            }
        }
        if control.configuration_revision <= state.highest_revision {
            return Err(CommunicationRuntimeError::StaleControl);
        }
        state.highest_revision = control.configuration_revision;
        state.generation = state
            .generation
            .checked_add(1)
            .ok_or(CommunicationRuntimeError::InvalidControl)?;
        state.control = control;
        state.suspended_control = None;
        self.updates.send_replace(*state);
        Ok(true)
    }

    pub(crate) async fn disable_for_owner(&self, owner_epoch: u64) -> bool {
        let mut state = self.state.write().await;
        if state.owner_epoch != owner_epoch {
            return false;
        }
        state.generation = state.generation.saturating_add(1);
        state.control = CommunicationControl::unpaired();
        state.suspended_control = None;
        self.updates.send_replace(*state);
        true
    }

    pub(crate) async fn suspend_for_owner(&self, owner_epoch: u64) -> bool {
        let mut state = self.state.write().await;
        if state.owner_epoch != owner_epoch {
            return false;
        }
        if state.control.identity.is_some() {
            state.suspended_control = Some(state.control);
            state.generation = state.generation.saturating_add(1);
            state.control = CommunicationControl::unpaired();
            self.updates.send_replace(*state);
        }
        true
    }

    fn subscribe(&self) -> watch::Receiver<AuthorizationState> {
        self.updates.subscribe()
    }

    async fn commit_permit(
        &self,
        control: CommunicationControl,
    ) -> Option<CommunicationCommitPermit> {
        let state = Arc::clone(&self.state).read_owned().await;
        (state.control == control && state.control.active())
            .then_some(CommunicationCommitPermit { _state: state })
    }
}

struct CommunicationCommitPermit {
    _state: OwnedRwLockReadGuard<AuthorizationState>,
}

impl Default for CommunicationAuthorization {
    fn default() -> Self {
        Self::new()
    }
}

impl CommunicationRuntime {
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Starts one serial supervisor over an injected Provider factory.
    ///
    /// # Errors
    ///
    /// Returns a database error if the initial fail-closed state cannot be persisted.
    pub async fn start(
        database: Arc<DbActorHandle>,
        database_path: PathBuf,
        factory: Arc<dyn CommunicationProviderFactory>,
        initial_control: CommunicationControl,
    ) -> Result<Self, CommunicationRuntimeError> {
        let authorization = CommunicationAuthorization::new();
        if initial_control.identity.is_some() {
            authorization.apply_persisted(initial_control).await?;
        }
        Self::start_inner(
            database,
            database_path,
            factory,
            authorization.clone(),
            Some(authorization),
        )
        .await
    }

    /// Starts a runtime that observes a shared Cloud/pairing authorization generation directly.
    ///
    /// # Errors
    ///
    /// Returns a database error if the initial fail-closed state cannot be persisted.
    pub async fn start_authorized(
        database: Arc<DbActorHandle>,
        database_path: PathBuf,
        factory: Arc<dyn CommunicationProviderFactory>,
        authorization: CommunicationAuthorization,
    ) -> Result<Self, CommunicationRuntimeError> {
        Self::start_inner(database, database_path, factory, authorization, None).await
    }

    async fn start_inner(
        database: Arc<DbActorHandle>,
        database_path: PathBuf,
        factory: Arc<dyn CommunicationProviderFactory>,
        authorization: CommunicationAuthorization,
        control_writer: Option<CommunicationAuthorization>,
    ) -> Result<Self, CommunicationRuntimeError> {
        let authorization_receiver = authorization.subscribe();
        let initial_control = authorization_receiver.borrow().control;
        persist_collector_state(&database, initial_control, None).await?;
        let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
        let worker = tokio::spawn(run_supervisor(
            database,
            database_path,
            factory,
            initial_control,
            receiver,
            authorization.clone(),
            authorization_receiver,
        ));
        Ok(Self {
            control_writer,
            commands: Some(commands),
            worker: Some(worker),
        })
    }

    /// Applies only a newer same-identity configuration, or an immediate unpaired transition.
    ///
    /// # Errors
    ///
    /// Returns a stale-control or worker error without changing the active Provider.
    pub async fn apply_control(
        &self,
        control: CommunicationControl,
    ) -> Result<(), CommunicationRuntimeError> {
        let authorization = self
            .control_writer
            .as_ref()
            .ok_or(CommunicationRuntimeError::AuthorizationReadOnly)?;
        if control.identity.is_none() {
            authorization.disable().await;
        } else {
            authorization.apply_persisted(control).await?;
        }
        let (response_sender, response_receiver) = oneshot::channel();
        self.commands
            .as_ref()
            .ok_or(CommunicationRuntimeError::QueueClosed)?
            .send(Command::Apply {
                control,
                response: response_sender,
            })
            .await
            .map_err(|_| CommunicationRuntimeError::QueueClosed)?;
        response_receiver
            .await
            .map_err(|_| CommunicationRuntimeError::WorkerStopped)?
    }

    /// Cancels the current Provider operation, calls `stop`, and joins the worker.
    ///
    /// # Errors
    ///
    /// Returns an error when the worker panics or reports a database failure.
    pub async fn shutdown(mut self) -> Result<(), CommunicationRuntimeError> {
        self.commands.take();
        match self.worker.take() {
            Some(worker) => worker
                .await
                .map_err(|_| CommunicationRuntimeError::WorkerStopped)?,
            None => Ok(()),
        }
    }
}

#[allow(
    clippy::single_match_else,
    clippy::too_many_lines,
    reason = "serial Provider cancellation, retry, backpressure, and persistence share one owner"
)]
async fn run_supervisor(
    database: Arc<DbActorHandle>,
    database_path: PathBuf,
    factory: Arc<dyn CommunicationProviderFactory>,
    mut control: CommunicationControl,
    mut commands: mpsc::Receiver<Command>,
    authorization_gate: CommunicationAuthorization,
    mut authorization: watch::Receiver<AuthorizationState>,
) -> Result<(), CommunicationRuntimeError> {
    let mut highest_revision = control.configuration_revision;
    let mut outbox_paused = false;
    let mut spool_paused = false;
    let mut retry_index = 0_usize;
    let mut retry_revision = control.configuration_revision;
    let mut emitted_metadata_events = BTreeSet::new();

    'supervisor: loop {
        let authoritative_control = authorization.borrow().control;
        if control != authoritative_control {
            control = authoritative_control;
            highest_revision = highest_revision.max(control.configuration_revision);
            persist_collector_state(&database, control, None).await?;
        }
        if control.configuration_revision != retry_revision {
            retry_index = 0;
            retry_revision = control.configuration_revision;
        }
        if !control.active() {
            tokio::select! {
                biased;
                changed = authorization.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                    control = authorization.borrow_and_update().control;
                    highest_revision = highest_revision.max(control.configuration_revision);
                    persist_collector_state(&database, control, None).await?;
                }
                command = commands.recv() => match command {
                    Some(command) => {
                        apply_command(&database, &mut control, &mut highest_revision, command).await?;
                    }
                    None => return Ok(()),
                }
            }
            continue;
        }

        let depth = database.active_outbox_depth().await?;
        update_hysteresis(
            &mut outbox_paused,
            depth,
            OUTBOX_HIGH_WATER,
            OUTBOX_LOW_WATER,
        );
        if outbox_paused {
            if wait_or_command(
                &database,
                &mut control,
                &mut highest_revision,
                &mut commands,
                &mut authorization,
                MONITOR_INTERVAL,
            )
            .await?
            {
                continue;
            }
            return Ok(());
        }

        let provider_result = {
            let Some(_permit) = authorization_gate.commit_permit(control).await else {
                continue;
            };
            factory.create()
        };
        let mut provider = match provider_result {
            Ok(provider) => provider,
            Err(error) => {
                persist_collector_state(&database, control, Some(&error)).await?;
                if should_retry(&error) {
                    let delay = RETRY_DELAYS[retry_index.min(RETRY_DELAYS.len() - 1)];
                    retry_index = (retry_index + 1).min(RETRY_DELAYS.len() - 1);
                    if wait_or_command(
                        &database,
                        &mut control,
                        &mut highest_revision,
                        &mut commands,
                        &mut authorization,
                        delay,
                    )
                    .await?
                    {
                        continue;
                    }
                    return Ok(());
                }
                if wait_for_new_control(
                    &database,
                    &mut control,
                    &mut highest_revision,
                    &mut commands,
                    &mut authorization,
                )
                .await?
                {
                    continue;
                }
                return Ok(());
            }
        };

        let discovered =
            match discover_or_command(provider.as_mut(), &mut commands, &mut authorization).await {
                Operation::Completed(result) => result,
                Operation::Authorization(next) => {
                    let stop_result = provider.stop();
                    control = next;
                    highest_revision = highest_revision.max(control.configuration_revision);
                    persist_collector_state(&database, control, None).await?;
                    if stop_result.is_err() {
                        return quarantine_provider(
                            provider.as_mut(),
                            &database,
                            &mut control,
                            &mut highest_revision,
                            &mut commands,
                            &mut authorization,
                        )
                        .await;
                    }
                    continue 'supervisor;
                }
                Operation::Command(command) => {
                    let stop_result = provider.stop();
                    match command {
                        Some(command) => {
                            apply_command(&database, &mut control, &mut highest_revision, command)
                                .await?;
                            if stop_result.is_err() {
                                return quarantine_provider(
                                    provider.as_mut(),
                                    &database,
                                    &mut control,
                                    &mut highest_revision,
                                    &mut commands,
                                    &mut authorization,
                                )
                                .await;
                            }
                            continue 'supervisor;
                        }
                        None => {
                            if stop_result.is_err() {
                                persist_collector_code(&database, control, STOP_FAILED).await?;
                            }
                            return Ok(());
                        }
                    }
                }
            };
        if let Err(error) = discovered {
            if provider.stop().is_err() {
                return quarantine_provider(
                    provider.as_mut(),
                    &database,
                    &mut control,
                    &mut highest_revision,
                    &mut commands,
                    &mut authorization,
                )
                .await;
            }
            persist_collector_state(&database, control, Some(&error)).await?;
            if should_retry(&error) {
                let delay = RETRY_DELAYS[retry_index.min(RETRY_DELAYS.len() - 1)];
                retry_index = (retry_index + 1).min(RETRY_DELAYS.len() - 1);
                if wait_or_command(
                    &database,
                    &mut control,
                    &mut highest_revision,
                    &mut commands,
                    &mut authorization,
                    delay,
                )
                .await?
                {
                    continue;
                }
                return Ok(());
            }
            if wait_for_new_control(
                &database,
                &mut control,
                &mut highest_revision,
                &mut commands,
                &mut authorization,
            )
            .await?
            {
                continue;
            }
            return Ok(());
        }
        retry_index = 0;
        retry_revision = control.configuration_revision;
        persist_collector_state(&database, control, None).await?;

        loop {
            let depth = database.active_outbox_depth().await?;
            update_hysteresis(
                &mut outbox_paused,
                depth,
                OUTBOX_HIGH_WATER,
                OUTBOX_LOW_WATER,
            );
            if outbox_paused {
                if provider.stop().is_err() {
                    return quarantine_provider(
                        provider.as_mut(),
                        &database,
                        &mut control,
                        &mut highest_revision,
                        &mut commands,
                        &mut authorization,
                    )
                    .await;
                }
                continue 'supervisor;
            }

            let records =
                match poll_or_command(provider.as_mut(), &mut commands, &mut authorization).await {
                    Operation::Completed(result) => result,
                    Operation::Authorization(next) => {
                        let stop_result = provider.stop();
                        control = next;
                        highest_revision = highest_revision.max(control.configuration_revision);
                        persist_collector_state(&database, control, None).await?;
                        if stop_result.is_err() {
                            return quarantine_provider(
                                provider.as_mut(),
                                &database,
                                &mut control,
                                &mut highest_revision,
                                &mut commands,
                                &mut authorization,
                            )
                            .await;
                        }
                        continue 'supervisor;
                    }
                    Operation::Command(command) => {
                        let stop_result = provider.stop();
                        match command {
                            Some(command) => {
                                apply_command(
                                    &database,
                                    &mut control,
                                    &mut highest_revision,
                                    command,
                                )
                                .await?;
                                if stop_result.is_err() {
                                    return quarantine_provider(
                                        provider.as_mut(),
                                        &database,
                                        &mut control,
                                        &mut highest_revision,
                                        &mut commands,
                                        &mut authorization,
                                    )
                                    .await;
                                }
                                continue 'supervisor;
                            }
                            None => {
                                if stop_result.is_err() {
                                    persist_collector_code(&database, control, STOP_FAILED).await?;
                                }
                                return Ok(());
                            }
                        }
                    }
                };

            match records {
                Ok(mut records) => {
                    records.sort_by_key(|record| !record.completed_media().is_empty());
                    let mut persistence_failed = false;
                    let mut batch_paused = false;
                    let mut event_observed = false;
                    for record in records {
                        if let Some(identity) = control.identity {
                            let event_id = stable_communication_event_id(
                                identity,
                                record.account_id(),
                                record.message().source_key(),
                            );
                            let (event_count, _) =
                                database.count_event_and_outbox(&event_id).await?;
                            if event_count == 1 {
                                continue;
                            }
                        }
                        let depth = database.active_outbox_depth().await?;
                        update_hysteresis(
                            &mut outbox_paused,
                            depth,
                            OUTBOX_HIGH_WATER,
                            OUTBOX_LOW_WATER,
                        );
                        if outbox_paused {
                            batch_paused = true;
                            break;
                        }
                        let preparation =
                            prepare_record(&database_path, control, record, &mut spool_paused);
                        tokio::pin!(preparation);
                        let commit = tokio::select! {
                            biased;
                            changed = authorization.changed() => {
                                let stop_result = provider.stop();
                                if changed.is_err() {
                                    return Ok(());
                                }
                                control = authorization.borrow_and_update().control;
                                highest_revision = highest_revision.max(control.configuration_revision);
                                persist_collector_state(&database, control, None).await?;
                                if stop_result.is_err() {
                                    return quarantine_provider(
                                        provider.as_mut(),
                                        &database,
                                        &mut control,
                                        &mut highest_revision,
                                        &mut commands,
                                        &mut authorization,
                                    ).await;
                                }
                                continue 'supervisor;
                            }
                            command = commands.recv() => {
                                let stop_result = provider.stop();
                                match command {
                                    Some(command) => {
                                        apply_command(&database, &mut control, &mut highest_revision, command).await?;
                                        if stop_result.is_err() {
                                            return quarantine_provider(
                                                provider.as_mut(),
                                                &database,
                                                &mut control,
                                                &mut highest_revision,
                                                &mut commands,
                                                &mut authorization,
                                            ).await;
                                        }
                                        continue 'supervisor;
                                    }
                                    None => {
                                        if stop_result.is_err() {
                                            persist_collector_code(&database, control, STOP_FAILED).await?;
                                        }
                                        return Ok(());
                                    },
                                }
                            }
                            result = &mut preparation => result,
                        };
                        let Ok(mut prepared) = commit else {
                            persistence_failed = true;
                            continue;
                        };
                        match commands.try_recv() {
                            Ok(command) => {
                                let stop_result = provider.stop();
                                apply_command(
                                    &database,
                                    &mut control,
                                    &mut highest_revision,
                                    command,
                                )
                                .await?;
                                if stop_result.is_err() {
                                    return quarantine_provider(
                                        provider.as_mut(),
                                        &database,
                                        &mut control,
                                        &mut highest_revision,
                                        &mut commands,
                                        &mut authorization,
                                    )
                                    .await;
                                }
                                continue 'supervisor;
                            }
                            Err(mpsc::error::TryRecvError::Disconnected) => {
                                if provider.stop().is_err() {
                                    persist_collector_code(&database, control, STOP_FAILED).await?;
                                }
                                return Ok(());
                            }
                            Err(mpsc::error::TryRecvError::Empty) => {}
                        }
                        let Some(_permit) = authorization_gate.commit_permit(control).await else {
                            let stop_result = provider.stop();
                            if authorization.has_changed().unwrap_or(false) {
                                control = authorization.borrow_and_update().control;
                                highest_revision =
                                    highest_revision.max(control.configuration_revision);
                                persist_collector_state(&database, control, None).await?;
                            }
                            if stop_result.is_err() {
                                return quarantine_provider(
                                    provider.as_mut(),
                                    &database,
                                    &mut control,
                                    &mut highest_revision,
                                    &mut commands,
                                    &mut authorization,
                                )
                                .await;
                            }
                            continue 'supervisor;
                        };
                        if database
                            .commit_communication_message(&prepared.commit)
                            .await
                            .map_err(|_| LocalPersistenceError::Database)
                            .is_err()
                        {
                            persistence_failed = true;
                            break;
                        }
                        prepared.spool.disarm();
                        for metadata_event in [&prepared.conversation_event, &prepared.sender_event]
                        {
                            if !emitted_metadata_events.contains(&metadata_event.event_id) {
                                if database
                                    .append_event_with_outbox(metadata_event)
                                    .await
                                    .map_err(|_| LocalPersistenceError::Database)
                                    .is_err()
                                {
                                    persistence_failed = true;
                                    break;
                                }
                                emitted_metadata_events.insert(metadata_event.event_id.clone());
                            }
                        }
                        if persistence_failed {
                            break;
                        }
                        event_observed = true;
                    }
                    if batch_paused {
                        if provider.stop().is_err() {
                            return quarantine_provider(
                                provider.as_mut(),
                                &database,
                                &mut control,
                                &mut highest_revision,
                                &mut commands,
                                &mut authorization,
                            )
                            .await;
                        }
                        continue 'supervisor;
                    }
                    if persistence_failed {
                        persist_collector_code(
                            &database,
                            control,
                            "WECHAT_LOCAL_SPOOL_UNAVAILABLE",
                        )
                        .await?;
                        if provider.stop().is_err() {
                            return quarantine_provider(
                                provider.as_mut(),
                                &database,
                                &mut control,
                                &mut highest_revision,
                                &mut commands,
                                &mut authorization,
                            )
                            .await;
                        }
                        if wait_or_command(
                            &database,
                            &mut control,
                            &mut highest_revision,
                            &mut commands,
                            &mut authorization,
                            RETRY_DELAYS[0],
                        )
                        .await?
                        {
                            continue 'supervisor;
                        }
                        return Ok(());
                    }
                    if event_observed {
                        persist_collector_event(&database, control).await?;
                    } else {
                        persist_collector_state(&database, control, None).await?;
                    }
                }
                Err(error) => {
                    if provider.stop().is_err() {
                        return quarantine_provider(
                            provider.as_mut(),
                            &database,
                            &mut control,
                            &mut highest_revision,
                            &mut commands,
                            &mut authorization,
                        )
                        .await;
                    }
                    persist_collector_state(&database, control, Some(&error)).await?;
                    if should_retry(&error) {
                        let delay = RETRY_DELAYS[retry_index.min(RETRY_DELAYS.len() - 1)];
                        retry_index = (retry_index + 1).min(RETRY_DELAYS.len() - 1);
                        if wait_or_command(
                            &database,
                            &mut control,
                            &mut highest_revision,
                            &mut commands,
                            &mut authorization,
                            delay,
                        )
                        .await?
                        {
                            continue 'supervisor;
                        }
                        return Ok(());
                    }
                    if wait_for_new_control(
                        &database,
                        &mut control,
                        &mut highest_revision,
                        &mut commands,
                        &mut authorization,
                    )
                    .await?
                    {
                        continue 'supervisor;
                    }
                    return Ok(());
                }
            }

            match wait_for_poll_or_command(
                &database,
                &mut control,
                &mut highest_revision,
                &mut commands,
                &mut authorization,
                POLL_INTERVAL,
            )
            .await?
            {
                PollWait::Elapsed => {}
                PollWait::CommandApplied => {
                    if provider.stop().is_err() {
                        return quarantine_provider(
                            provider.as_mut(),
                            &database,
                            &mut control,
                            &mut highest_revision,
                            &mut commands,
                            &mut authorization,
                        )
                        .await;
                    }
                    continue 'supervisor;
                }
                PollWait::Closed => {
                    if provider.stop().is_err() {
                        persist_collector_code(&database, control, STOP_FAILED).await?;
                    }
                    return Ok(());
                }
            }
        }
    }
}

async fn discover_or_command(
    provider: &mut dyn CommunicationProvider,
    commands: &mut mpsc::Receiver<Command>,
    authorization: &mut watch::Receiver<AuthorizationState>,
) -> Operation<Result<(), DomainError>> {
    tokio::select! {
        biased;
        changed = authorization.changed() => {
            if changed.is_err() {
                Operation::Command(None)
            } else {
                Operation::Authorization(authorization.borrow_and_update().control)
            }
        },
        command = commands.recv() => Operation::Command(command),
        result = provider.discover() => Operation::Completed(result),
    }
}

async fn poll_or_command(
    provider: &mut dyn CommunicationProvider,
    commands: &mut mpsc::Receiver<Command>,
    authorization: &mut watch::Receiver<AuthorizationState>,
) -> Operation<Result<Vec<NormalizedCommunicationRecord>, DomainError>> {
    tokio::select! {
        biased;
        changed = authorization.changed() => {
            if changed.is_err() {
                Operation::Command(None)
            } else {
                Operation::Authorization(authorization.borrow_and_update().control)
            }
        },
        command = commands.recv() => Operation::Command(command),
        result = provider.poll_once() => Operation::Completed(result),
    }
}

async fn apply_command(
    database: &DbActorHandle,
    current: &mut CommunicationControl,
    highest_revision: &mut u64,
    command: Command,
) -> Result<(), CommunicationRuntimeError> {
    let Command::Apply { control, response } = command;
    let result = if control == *current {
        Ok(())
    } else if control.identity.is_none() {
        *current = control;
        persist_collector_state(database, control, None).await
    } else if control.configuration_revision <= *highest_revision {
        Err(CommunicationRuntimeError::StaleControl)
    } else {
        *highest_revision = control.configuration_revision;
        *current = control;
        persist_collector_state(database, control, None).await
    };
    let is_fatal = matches!(result, Err(CommunicationRuntimeError::Database(_)));
    let _ = response.send(result);
    if is_fatal {
        return Err(CommunicationRuntimeError::WorkerStopped);
    }
    Ok(())
}

async fn quarantine_provider(
    provider: &mut dyn CommunicationProvider,
    database: &DbActorHandle,
    control: &mut CommunicationControl,
    highest_revision: &mut u64,
    commands: &mut mpsc::Receiver<Command>,
    authorization: &mut watch::Receiver<AuthorizationState>,
) -> Result<(), CommunicationRuntimeError> {
    persist_collector_code(database, *control, STOP_FAILED).await?;
    loop {
        tokio::select! {
            biased;
            changed = authorization.changed() => {
                if changed.is_err() {
                    if provider.stop().is_err() {
                        persist_collector_code(database, *control, STOP_FAILED).await?;
                    }
                    return Err(CommunicationRuntimeError::ProviderStopFailed);
                }
                *control = authorization.borrow_and_update().control;
                *highest_revision = (*highest_revision).max(control.configuration_revision);
                if provider.stop().is_err() {
                    persist_collector_code(database, *control, STOP_FAILED).await?;
                }
                persist_collector_code(database, *control, STOP_FAILED).await?;
            }
            command = commands.recv() => {
                if let Some(command) = command {
                    let command_changes_control = match &command {
                        Command::Apply { control: next, .. } => next != control,
                    };
                    apply_command(database, control, highest_revision, command).await?;
                    if command_changes_control && provider.stop().is_err() {
                        persist_collector_code(database, *control, STOP_FAILED).await?;
                    }
                    persist_collector_code(database, *control, STOP_FAILED).await?;
                } else {
                    if provider.stop().is_err() {
                        persist_collector_code(database, *control, STOP_FAILED).await?;
                    }
                    return Err(CommunicationRuntimeError::ProviderStopFailed);
                }
            }
        }
    }
}

async fn wait_or_command(
    database: &DbActorHandle,
    control: &mut CommunicationControl,
    highest_revision: &mut u64,
    commands: &mut mpsc::Receiver<Command>,
    authorization: &mut watch::Receiver<AuthorizationState>,
    delay: Duration,
) -> Result<bool, CommunicationRuntimeError> {
    tokio::select! {
        biased;
        changed = authorization.changed() => {
            if changed.is_err() {
                return Ok(false);
            }
            *control = authorization.borrow_and_update().control;
            *highest_revision = (*highest_revision).max(control.configuration_revision);
            persist_collector_state(database, *control, None).await?;
            Ok(true)
        },
        command = commands.recv() => match command {
            Some(command) => {
                apply_command(database, control, highest_revision, command).await?;
                Ok(true)
            }
            None => Ok(false),
        },
        () = time::sleep(delay) => Ok(true),
    }
}

async fn wait_for_poll_or_command(
    database: &DbActorHandle,
    control: &mut CommunicationControl,
    highest_revision: &mut u64,
    commands: &mut mpsc::Receiver<Command>,
    authorization: &mut watch::Receiver<AuthorizationState>,
    delay: Duration,
) -> Result<PollWait, CommunicationRuntimeError> {
    tokio::select! {
        biased;
        changed = authorization.changed() => {
            if changed.is_err() {
                return Ok(PollWait::Closed);
            }
            *control = authorization.borrow_and_update().control;
            *highest_revision = (*highest_revision).max(control.configuration_revision);
            persist_collector_state(database, *control, None).await?;
            Ok(PollWait::CommandApplied)
        },
        command = commands.recv() => match command {
            Some(command) => {
                apply_command(database, control, highest_revision, command).await?;
                Ok(PollWait::CommandApplied)
            }
            None => Ok(PollWait::Closed),
        },
        () = time::sleep(delay) => Ok(PollWait::Elapsed),
    }
}

async fn wait_for_new_control(
    database: &DbActorHandle,
    control: &mut CommunicationControl,
    highest_revision: &mut u64,
    commands: &mut mpsc::Receiver<Command>,
    authorization: &mut watch::Receiver<AuthorizationState>,
) -> Result<bool, CommunicationRuntimeError> {
    tokio::select! {
        biased;
        changed = authorization.changed() => {
            if changed.is_err() {
                return Ok(false);
            }
            *control = authorization.borrow_and_update().control;
            *highest_revision = (*highest_revision).max(control.configuration_revision);
            persist_collector_state(database, *control, None).await?;
            Ok(true)
        }
        command = commands.recv() => match command {
            Some(command) => {
                apply_command(database, control, highest_revision, command).await?;
                Ok(true)
            }
            None => Ok(false),
        },
    }
}

fn update_hysteresis(paused: &mut bool, value: u64, high: u64, low: u64) {
    if value > high {
        *paused = true;
    } else if value < low {
        *paused = false;
    }
}

fn should_retry(error: &DomainError) -> bool {
    error.retryable
        && matches!(
            error.code.as_str(),
            "WECHAT_WAITING_SOURCE"
                | "WECHAT_CAPABILITY_UNAVAILABLE"
                | "WECHAT_KEY_REJECTED"
                | "WECHAT_ACCOUNT_UNVERIFIED"
                | "WECHAT_MULTIPLE_ACCOUNTS"
                | "WECHAT_DATABASE_UNAVAILABLE"
                | "WECHAT_PROBE_TIMEOUT"
                | "WECHAT_PERMISSION_REQUIRED"
                | "WECHAT_SESSION_READ_FAILED"
                | "WECHAT_CONTACT_READ_FAILED"
                | "WECHAT_MESSAGE_READ_FAILED"
        )
}

#[allow(
    clippy::too_many_lines,
    reason = "one transaction prepares all normalized communication payload variants"
)]
async fn prepare_record(
    database_path: &Path,
    control: CommunicationControl,
    record: NormalizedCommunicationRecord,
    spool_paused: &mut bool,
) -> Result<PreparedRecord, LocalPersistenceError> {
    let identity = control
        .identity
        .ok_or(LocalPersistenceError::InvalidRecord)?;
    let conversation_avatar_url = record.conversation_avatar_url().map(str::to_owned);
    let sender_avatar_url = record.sender_avatar_url().map(str::to_owned);
    let (account_id, source_sequence, conversation_display_name, message, completed_media) =
        record.into_parts();
    let prepared_media = copy_completed_media(
        database_path,
        message.attachments(),
        &completed_media,
        spool_paused,
    )
    .await?;
    let created_at = message.occurred_at().to_owned();
    let display_name_fingerprint = Sha256::digest(
        format!(
            "{}\0{}",
            conversation_display_name,
            conversation_avatar_url.as_deref().unwrap_or_default()
        )
        .as_bytes(),
    );
    let conversation_source_key = format!(
        "conversation-observed:{}:{display_name_fingerprint:x}",
        message.source_key()
    );
    let mut conversation_payload = serde_json::json!({
        "conversation_id": message.conversation_id(),
        "display_name": conversation_display_name,
        "observed_at": message.occurred_at(),
        "conversation": message.conversation(),
    })
    .as_object()
    .cloned()
    .ok_or(LocalPersistenceError::InvalidRecord)?;
    if let Some(avatar_url) = conversation_avatar_url {
        conversation_payload.insert("avatar_url".to_owned(), avatar_url.into());
    }
    let conversation_event_id =
        stable_communication_event_id(identity, &account_id, &conversation_source_key);
    let conversation_event = EventEnvelope {
        event_id: conversation_event_id.clone(),
        workspace_id: identity.workspace_id.hyphenated().to_string(),
        device_id: identity.device_id.hyphenated().to_string(),
        event_type: "communication.conversation_observed".to_owned(),
        source: COLLECTOR_KEY.to_owned(),
        schema_version: 1,
        occurred_at: message.occurred_at().to_owned(),
        created_at: created_at.clone(),
        sensitivity: Sensitivity::High,
        payload: conversation_payload,
        attachment_refs: Vec::new(),
        idempotency_key: Some(format!("conversation-observed:{conversation_event_id}")),
    };
    let sender_fingerprint = Sha256::digest(
        format!(
            "{}\0{}\0{}",
            message.sender_id(),
            message.sender_display_name(),
            sender_avatar_url.as_deref().unwrap_or_default()
        )
        .as_bytes(),
    );
    let sender_source_key = format!(
        "message-sender-observed:{}:{sender_fingerprint:x}",
        message.source_key()
    );
    let sender_event_id = stable_communication_event_id(identity, &account_id, &sender_source_key);
    let mut sender_payload = serde_json::json!({
        "message_id": message.message_id(),
        "source_key": message.source_key(),
        "sender_id": message.sender_id(),
        "sender_display_name": message.sender_display_name(),
        "observed_at": message.occurred_at(),
    })
    .as_object()
    .cloned()
    .ok_or(LocalPersistenceError::InvalidRecord)?;
    if let Some(avatar_url) = sender_avatar_url {
        sender_payload.insert("avatar_url".to_owned(), avatar_url.into());
    }
    let sender_event = EventEnvelope {
        event_id: sender_event_id.clone(),
        workspace_id: identity.workspace_id.hyphenated().to_string(),
        device_id: identity.device_id.hyphenated().to_string(),
        event_type: "communication.message_sender_observed".to_owned(),
        source: COLLECTOR_KEY.to_owned(),
        schema_version: 1,
        occurred_at: message.occurred_at().to_owned(),
        created_at: created_at.clone(),
        sensitivity: Sensitivity::High,
        payload: sender_payload,
        attachment_refs: Vec::new(),
        idempotency_key: Some(format!("message-sender-observed:{sender_event_id}")),
    };
    let payload = serde_json::to_value(&message)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .ok_or(LocalPersistenceError::InvalidRecord)?;
    let event = EventEnvelope {
        event_id: stable_communication_event_id(identity, &account_id, message.source_key()),
        workspace_id: identity.workspace_id.hyphenated().to_string(),
        device_id: identity.device_id.hyphenated().to_string(),
        event_type: "communication.message_recorded".to_owned(),
        source: COLLECTOR_KEY.to_owned(),
        schema_version: 1,
        occurred_at: message.occurred_at().to_owned(),
        created_at,
        sensitivity: Sensitivity::High,
        payload,
        attachment_refs: message
            .attachments()
            .iter()
            .map(|attachment| attachment.attachment_id().to_owned())
            .collect(),
        idempotency_key: Some(message.source_key().to_owned()),
    };
    Ok(PreparedRecord {
        conversation_event,
        sender_event,
        commit: CommunicationMessageCommit {
            account_id,
            source_sequence,
            event,
            message,
            attachment_spool: prepared_media.references,
        },
        spool: prepared_media.spool,
    })
}

fn stable_communication_event_id(
    identity: CommunicationIdentity,
    account_id: &str,
    source_key: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(identity.workspace_id.as_bytes());
    hasher.update(identity.device_id.as_bytes());
    hasher.update(account_id.as_bytes());
    hasher.update(source_key.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes).hyphenated().to_string()
}

struct PreparedRecord {
    conversation_event: EventEnvelope,
    sender_event: EventEnvelope,
    commit: CommunicationMessageCommit,
    spool: AttemptSpoolLease,
}

struct PreparedMedia {
    references: Vec<CommunicationAttachmentSpoolReference>,
    spool: AttemptSpoolLease,
}

#[derive(Debug)]
enum LocalPersistenceError {
    Database,
    Filesystem,
    InvalidRecord,
    Quota,
}

async fn copy_completed_media(
    database_path: &Path,
    attachments: &[pca_domain::CommunicationAttachment],
    completed_media: &[CompletedMediaSource],
    spool_paused: &mut bool,
) -> Result<PreparedMedia, LocalPersistenceError> {
    if attachments.is_empty() {
        return Ok(PreparedMedia {
            references: Vec::new(),
            spool: AttemptSpoolLease::empty(),
        });
    }
    if attachments
        .iter()
        .any(|attachment| !mime_matches_kind(attachment.kind(), attachment.mime_type()))
    {
        return Err(LocalPersistenceError::InvalidRecord);
    }
    let root_path = DbActorHandle::communication_spool_root(database_path);
    let root = open_spool_root(&root_path)?;
    let usage = spool_usage(&root)?;
    if *spool_paused {
        if usage >= SPOOL_RESUME_BELOW_BYTES {
            return Err(LocalPersistenceError::Quota);
        }
        *spool_paused = false;
    }
    let declared = attachments.iter().try_fold(0_u64, |sum, attachment| {
        sum.checked_add(attachment.size_bytes())
            .ok_or(LocalPersistenceError::Quota)
    })?;
    if usage
        .checked_add(declared)
        .is_none_or(|total| total > SPOOL_HARD_LIMIT_BYTES)
    {
        *spool_paused = true;
        return Err(LocalPersistenceError::Quota);
    }

    let mut references = Vec::with_capacity(attachments.len());
    let mut spool = AttemptSpoolLease::new(&root)?;
    for attachment in attachments {
        let source = completed_media
            .iter()
            .find(|source| source.attachment_id() == attachment.attachment_id())
            .ok_or(LocalPersistenceError::InvalidRecord)?;
        copy_one(
            &root,
            &mut spool,
            source.source_path(),
            attachment.sha256(),
            attachment.size_bytes(),
        )
        .await?;
        references.push(CommunicationAttachmentSpoolReference {
            attachment_id: attachment.attachment_id().to_owned(),
            file_name: attachment.sha256().to_owned(),
        });
    }
    Ok(PreparedMedia { references, spool })
}

fn mime_matches_kind(kind: MessageKind, mime_type: &str) -> bool {
    let family = match kind {
        MessageKind::Audio => "audio/",
        MessageKind::Image => "image/",
        MessageKind::Video => "video/",
        MessageKind::File => return !mime_type.trim().is_empty(),
        MessageKind::Text => return false,
    };
    mime_type
        .strip_prefix(family)
        .is_some_and(|subtype| !subtype.is_empty())
}

#[allow(
    clippy::too_many_lines,
    reason = "stream validation and no-replace publication share one attempt-owned lease"
)]
async fn copy_one(
    root: &File,
    spool: &mut AttemptSpoolLease,
    source_path: &Path,
    expected_hash: &str,
    expected_size: u64,
) -> Result<(), LocalPersistenceError> {
    match rustix::fs::openat(
        root,
        expected_hash,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(existing) => {
            validate_open_file(File::from(existing), expected_hash, expected_size).await?;
            return Ok(());
        }
        Err(error) if error == rustix::io::Errno::NOENT => {}
        Err(_) => return Err(LocalPersistenceError::Filesystem),
    }

    let source = open_source_without_symlinks(source_path)?;
    let temporary_name = format!(".partial-{}", Uuid::new_v4().simple());
    let temporary = rustix::fs::openat(
        root,
        temporary_name.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map(File::from)
    .map_err(|_| LocalPersistenceError::Filesystem)?;
    spool.track(temporary_name.clone());
    let mut source = tokio::fs::File::from_std(source);
    let mut temporary = tokio::fs::File::from_std(temporary);
    let mut hasher = Sha256::new();
    let mut count = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = source
            .read(&mut buffer)
            .await
            .map_err(|_| LocalPersistenceError::Filesystem)?;
        if read == 0 {
            break;
        }
        count = count
            .checked_add(u64::try_from(read).map_err(|_| LocalPersistenceError::Filesystem)?)
            .ok_or(LocalPersistenceError::Filesystem)?;
        if count > expected_size {
            return Err(LocalPersistenceError::InvalidRecord);
        }
        hasher.update(&buffer[..read]);
        temporary
            .write_all(&buffer[..read])
            .await
            .map_err(|_| LocalPersistenceError::Filesystem)?;
    }
    temporary
        .flush()
        .await
        .map_err(|_| LocalPersistenceError::Filesystem)?;
    temporary
        .sync_all()
        .await
        .map_err(|_| LocalPersistenceError::Filesystem)?;
    if count != expected_size || format!("{:x}", hasher.finalize()) != expected_hash {
        return Err(LocalPersistenceError::InvalidRecord);
    }
    drop(temporary);
    match rustix::fs::linkat(
        root,
        temporary_name.as_str(),
        root,
        expected_hash,
        AtFlags::empty(),
    ) {
        Ok(()) => spool.track(expected_hash.to_owned()),
        Err(error) if error == rustix::io::Errno::EXIST => {
            let existing = rustix::fs::openat(
                root,
                expected_hash,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map(File::from)
            .map_err(|_| LocalPersistenceError::Filesystem)?;
            validate_open_file(existing, expected_hash, expected_size).await?;
            spool.remove_owned(temporary_name.as_str())?;
            root.sync_all()
                .map_err(|_| LocalPersistenceError::Filesystem)?;
            return Ok(());
        }
        Err(_) => return Err(LocalPersistenceError::Filesystem),
    }
    root.sync_all()
        .map_err(|_| LocalPersistenceError::Filesystem)?;
    let final_file = rustix::fs::openat(
        root,
        expected_hash,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|_| LocalPersistenceError::Filesystem)?;
    validate_open_file(final_file, expected_hash, expected_size).await?;
    spool.remove_owned(temporary_name.as_str())?;
    root.sync_all()
        .map_err(|_| LocalPersistenceError::Filesystem)
}

async fn validate_open_file(
    file: File,
    expected_hash: &str,
    expected_size: u64,
) -> Result<(), LocalPersistenceError> {
    if !file
        .metadata()
        .map_err(|_| LocalPersistenceError::Filesystem)?
        .is_file()
    {
        return Err(LocalPersistenceError::Filesystem);
    }
    let mut file = tokio::fs::File::from_std(file);
    let mut hasher = Sha256::new();
    let mut count = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|_| LocalPersistenceError::Filesystem)?;
        if read == 0 {
            break;
        }
        count = count
            .checked_add(u64::try_from(read).map_err(|_| LocalPersistenceError::Filesystem)?)
            .ok_or(LocalPersistenceError::Filesystem)?;
        if count > expected_size {
            return Err(LocalPersistenceError::InvalidRecord);
        }
        hasher.update(&buffer[..read]);
    }
    if count == expected_size && format!("{:x}", hasher.finalize()) == expected_hash {
        Ok(())
    } else {
        Err(LocalPersistenceError::InvalidRecord)
    }
}

fn open_source_without_symlinks(path: &Path) -> Result<File, LocalPersistenceError> {
    if !path.is_absolute() {
        return Err(LocalPersistenceError::Filesystem);
    }
    let mut directory = rustix::fs::open(
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| LocalPersistenceError::Filesystem)?;
    let components = path.components().collect::<Vec<_>>();
    let normal_count = components
        .iter()
        .filter(|component| matches!(component, Component::Normal(_)))
        .count();
    let mut seen = 0_usize;
    for component in components {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                seen += 1;
                let flags = if seen == normal_count {
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC
                } else {
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
                };
                directory = rustix::fs::openat(&directory, name, flags, Mode::empty())
                    .map_err(|_| LocalPersistenceError::Filesystem)?;
            }
            Component::ParentDir | Component::Prefix(_) => {
                return Err(LocalPersistenceError::Filesystem);
            }
        }
    }
    let file = File::from(directory);
    if normal_count == 0
        || !file
            .metadata()
            .map_err(|_| LocalPersistenceError::Filesystem)?
            .is_file()
    {
        return Err(LocalPersistenceError::Filesystem);
    }
    Ok(file)
}

fn open_spool_root(path: &Path) -> Result<File, LocalPersistenceError> {
    if !path.is_absolute() {
        return Err(LocalPersistenceError::Filesystem);
    }
    let mut directory = rustix::fs::open(
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| LocalPersistenceError::Filesystem)?;
    let mut opened_component = false;
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                directory = rustix::fs::openat(
                    &directory,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|_| LocalPersistenceError::Filesystem)?;
                opened_component = true;
            }
            Component::ParentDir | Component::Prefix(_) => {
                return Err(LocalPersistenceError::Filesystem);
            }
        }
    }
    if !opened_component {
        return Err(LocalPersistenceError::Filesystem);
    }
    let root = File::from(directory);
    let metadata = root
        .metadata()
        .map_err(|_| LocalPersistenceError::Filesystem)?;
    if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(LocalPersistenceError::Filesystem);
    }
    Ok(root)
}

fn spool_usage(root: &File) -> Result<u64, LocalPersistenceError> {
    let entries =
        rustix::fs::Dir::read_from(root).map_err(|_| LocalPersistenceError::Filesystem)?;
    let mut total = 0_u64;
    for entry in entries {
        let entry = entry.map_err(|_| LocalPersistenceError::Filesystem)?;
        let name = entry.file_name();
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        let file = rustix::fs::openat(
            root,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map(File::from)
        .map_err(|_| LocalPersistenceError::Filesystem)?;
        let metadata = file
            .metadata()
            .map_err(|_| LocalPersistenceError::Filesystem)?;
        if !metadata.is_file() {
            return Err(LocalPersistenceError::Filesystem);
        }
        total = total
            .checked_add(metadata.len())
            .ok_or(LocalPersistenceError::Quota)?;
    }
    Ok(total)
}

struct AttemptSpoolLease {
    root: Option<File>,
    owned: BTreeSet<String>,
    armed: bool,
}

impl AttemptSpoolLease {
    fn empty() -> Self {
        Self {
            root: None,
            owned: BTreeSet::new(),
            armed: true,
        }
    }

    fn new(root: &File) -> Result<Self, LocalPersistenceError> {
        Ok(Self {
            root: Some(
                root.try_clone()
                    .map_err(|_| LocalPersistenceError::Filesystem)?,
            ),
            owned: BTreeSet::new(),
            armed: true,
        })
    }

    fn track(&mut self, name: String) {
        self.owned.insert(name);
    }

    fn remove_owned(&mut self, name: &str) -> Result<(), LocalPersistenceError> {
        let root = self
            .root
            .as_ref()
            .ok_or(LocalPersistenceError::Filesystem)?;
        rustix::fs::unlinkat(root, name, AtFlags::empty())
            .map_err(|_| LocalPersistenceError::Filesystem)?;
        self.owned.remove(name);
        Ok(())
    }

    fn disarm(&mut self) {
        self.owned.clear();
        self.armed = false;
    }
}

impl Drop for AttemptSpoolLease {
    fn drop(&mut self) {
        if self.armed {
            if let Some(root) = self.root.as_ref() {
                for name in &self.owned {
                    let _ = rustix::fs::unlinkat(root, name.as_str(), AtFlags::empty());
                }
                let _ = root.sync_all();
            }
        }
    }
}

async fn persist_collector_state(
    database: &DbActorHandle,
    control: CommunicationControl,
    error: Option<&DomainError>,
) -> Result<(), CommunicationRuntimeError> {
    let status = if !control.active() {
        CollectorStatus::Disabled
    } else if error.is_some_and(|error| error.code == "WECHAT_PERMISSION_REQUIRED") {
        CollectorStatus::PermissionRequired
    } else if error.is_some_and(|error| error.code == "WECHAT_CAPABILITY_UNAVAILABLE") {
        CollectorStatus::Unsupported
    } else if error.is_some() {
        CollectorStatus::Degraded
    } else {
        CollectorStatus::Running
    };
    persist_collector(
        database,
        control,
        status,
        error.map(|error| error.code.as_str()),
    )
    .await
}

async fn persist_collector_code(
    database: &DbActorHandle,
    control: CommunicationControl,
    error_code: &str,
) -> Result<(), CommunicationRuntimeError> {
    persist_collector(
        database,
        control,
        CollectorStatus::Degraded,
        Some(error_code),
    )
    .await
}

async fn persist_collector(
    database: &DbActorHandle,
    control: CommunicationControl,
    status: CollectorStatus,
    error_code: Option<&str>,
) -> Result<(), CommunicationRuntimeError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| CommunicationRuntimeError::Clock)?;
    let now_ms = i64::try_from(now.as_millis()).map_err(|_| CommunicationRuntimeError::Clock)?;
    let revision = if control.identity.is_some() {
        control.configuration_revision
    } else {
        0
    };
    let prior = database
        .load_collector_states()
        .await
        .map_err(CommunicationRuntimeError::Database)?
        .into_iter()
        .find(|state| state.collector_key == COLLECTOR_KEY);
    database
        .upsert_collector_state(&CollectorState {
            collector_key: COLLECTOR_KEY.to_owned(),
            collector_version: env!("CARGO_PKG_VERSION").to_owned(),
            status,
            desired_config_revision: revision,
            applied_config_revision: revision,
            last_event_at_ms: prior.as_ref().and_then(|state| state.last_event_at_ms),
            last_health_at_ms: if error_code.is_none() && control.active() {
                Some(now_ms)
            } else {
                prior.as_ref().and_then(|state| state.last_health_at_ms)
            },
            last_error_code: error_code.map(str::to_owned),
            created_at_ms: prior.as_ref().map_or(now_ms, |state| state.created_at_ms),
            updated_at_ms: now_ms,
        })
        .await
        .map_err(CommunicationRuntimeError::Database)
}

async fn persist_collector_event(
    database: &DbActorHandle,
    control: CommunicationControl,
) -> Result<(), CommunicationRuntimeError> {
    persist_collector_state(database, control, None).await?;
    let mut state = database
        .load_collector_states()
        .await
        .map_err(CommunicationRuntimeError::Database)?
        .into_iter()
        .find(|state| state.collector_key == COLLECTOR_KEY)
        .ok_or(CommunicationRuntimeError::InvalidControl)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| CommunicationRuntimeError::Clock)?;
    state.last_event_at_ms =
        Some(i64::try_from(now.as_millis()).map_err(|_| CommunicationRuntimeError::Clock)?);
    database
        .upsert_collector_state(&state)
        .await
        .map_err(CommunicationRuntimeError::Database)
}

/// Fail-closed factory used by process-test and unsupported-platform builds.
pub struct UnavailableCommunicationProviderFactory;

impl CommunicationProviderFactory for UnavailableCommunicationProviderFactory {
    fn create(&self) -> Result<Box<dyn CommunicationProvider>, DomainError> {
        Err(DomainError::new(
            "WECHAT_CAPABILITY_UNAVAILABLE",
            "verified WeChat source capability is unavailable",
            false,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        persist_collector_state, should_retry, stable_communication_event_id, CommunicationControl,
        CommunicationIdentity,
    };
    use pca_db_local::DbActorHandle;
    use pca_domain::{CollectorStatus, DomainError};
    use uuid::Uuid;

    #[test]
    fn communication_event_identity_is_stable_for_source_replay() {
        let identity = CommunicationIdentity {
            workspace_id: Uuid::parse_str("11111111-1111-4111-8111-111111111111")
                .expect("workspace UUID"),
            device_id: Uuid::parse_str("22222222-2222-4222-8222-222222222222")
                .expect("device UUID"),
        };
        let first = stable_communication_event_id(identity, "account", "source-key");
        let replay = stable_communication_event_id(identity, "account", "source-key");
        let different = stable_communication_event_id(identity, "account", "other-source-key");

        assert_eq!(first, replay);
        assert_ne!(first, different);
    }

    #[test]
    fn transient_wechat_read_stage_failures_are_retried() {
        for code in [
            "WECHAT_CAPABILITY_UNAVAILABLE",
            "WECHAT_KEY_REJECTED",
            "WECHAT_ACCOUNT_UNVERIFIED",
            "WECHAT_MULTIPLE_ACCOUNTS",
            "WECHAT_SESSION_READ_FAILED",
            "WECHAT_CONTACT_READ_FAILED",
            "WECHAT_MESSAGE_READ_FAILED",
        ] {
            assert!(should_retry(&DomainError::new(code, "read failed", true)));
        }
        assert!(!should_retry(&DomainError::new(
            "WECHAT_UNSUPPORTED_SCHEMA",
            "unsupported schema",
            false,
        )));
    }

    #[tokio::test]
    async fn suspended_authorization_accepts_the_same_persisted_revision() {
        let authorization = super::CommunicationAuthorization::new();
        let owner_epoch = authorization.replace_owner().await;
        let control = CommunicationControl::paired(
            CommunicationIdentity {
                workspace_id: Uuid::new_v4(),
                device_id: Uuid::new_v4(),
            },
            5,
            true,
        )
        .expect("valid control");

        assert!(authorization
            .apply_persisted_for_owner(owner_epoch, control)
            .await
            .expect("apply control"));
        assert!(authorization.suspend_for_owner(owner_epoch).await);
        assert!(authorization.commit_permit(control).await.is_none());
        assert!(authorization
            .apply_persisted_for_owner(owner_epoch, control)
            .await
            .expect("restore same revision"));
        assert!(authorization.commit_permit(control).await.is_some());
    }

    #[tokio::test]
    async fn app_data_access_error_is_persisted_as_permission_required() {
        let directory = tempfile::tempdir().expect("create database fixture");
        let database = DbActorHandle::open(&directory.path().join("agent.sqlite3"), "0.1.0")
            .await
            .expect("open database");
        let control = CommunicationControl::paired(
            CommunicationIdentity {
                workspace_id: Uuid::new_v4(),
                device_id: Uuid::new_v4(),
            },
            1,
            true,
        )
        .expect("valid control");
        let error = DomainError::new(
            "WECHAT_PERMISSION_REQUIRED",
            "Access to WeChat app data is required",
            true,
        );

        persist_collector_state(&database, control, Some(&error))
            .await
            .expect("persist permission state");
        let state = database
            .load_collector_states()
            .await
            .expect("read collector")
            .into_iter()
            .find(|state| state.collector_key == "communication.wechat")
            .expect("collector exists");

        assert_eq!(state.status, CollectorStatus::PermissionRequired);
        assert_eq!(
            state.last_error_code.as_deref(),
            Some("WECHAT_PERMISSION_REQUIRED")
        );
    }
}
