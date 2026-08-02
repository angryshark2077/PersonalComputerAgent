//! Paired communication Provider lifecycle and private local media spooling.

use std::{
    fmt,
    fs::{self, File, Metadata},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use ::time::{format_description::well_known::Rfc3339, OffsetDateTime};
use pca_db_local::{
    CommunicationAttachmentSpoolReference, CommunicationMessageCommit, DbActorHandle, DbError,
};
use pca_domain::{CollectorState, CollectorStatus, DomainError, EventEnvelope, Sensitivity};
use pca_provider_contracts::{
    CommunicationProvider, CommunicationProviderFactory, CompletedMediaSource,
    NormalizedCommunicationRecord,
};
use rustix::fs::{AtFlags, Mode, OFlags};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{mpsc, oneshot},
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
    QueueClosed,
    WorkerStopped,
    Clock,
}

impl fmt::Display for CommunicationRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "communication database: {error}"),
            Self::InvalidControl => formatter.write_str("communication control is invalid"),
            Self::StaleControl => formatter.write_str("communication control revision is stale"),
            Self::QueueClosed => formatter.write_str("communication command queue is closed"),
            Self::WorkerStopped => formatter.write_str("communication worker stopped"),
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
}

enum PollWait {
    Elapsed,
    CommandApplied,
    Closed,
}

/// Joined owner of the single communication Provider task.
pub struct CommunicationRuntime {
    commands: Option<mpsc::Sender<Command>>,
    worker: Option<JoinHandle<Result<(), CommunicationRuntimeError>>>,
}

impl CommunicationRuntime {
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
        persist_collector_state(&database, initial_control, None).await?;
        let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
        let worker = tokio::spawn(run_supervisor(
            database,
            database_path,
            factory,
            initial_control,
            receiver,
        ));
        Ok(Self {
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
    clippy::too_many_lines,
    reason = "serial Provider cancellation, retry, backpressure, and persistence share one owner"
)]
async fn run_supervisor(
    database: Arc<DbActorHandle>,
    database_path: PathBuf,
    factory: Arc<dyn CommunicationProviderFactory>,
    mut control: CommunicationControl,
    mut commands: mpsc::Receiver<Command>,
) -> Result<(), CommunicationRuntimeError> {
    let mut highest_revision = control.configuration_revision;
    let mut outbox_paused = false;
    let mut spool_paused = false;
    let mut retry_index = 0_usize;

    'supervisor: loop {
        if !control.active() {
            match commands.recv().await {
                Some(command) => {
                    apply_command(&database, &mut control, &mut highest_revision, command).await?;
                    continue;
                }
                None => return Ok(()),
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
            if wait_or_command(
                &database,
                &mut control,
                &mut highest_revision,
                &mut commands,
                MONITOR_INTERVAL,
            )
            .await?
            {
                continue;
            }
            return Ok(());
        }

        let mut provider = match factory.create() {
            Ok(provider) => provider,
            Err(error) => {
                persist_collector_state(&database, control, Some(&error)).await?;
                if error.retryable {
                    let delay = RETRY_DELAYS[retry_index.min(RETRY_DELAYS.len() - 1)];
                    retry_index = (retry_index + 1).min(RETRY_DELAYS.len() - 1);
                    if wait_or_command(
                        &database,
                        &mut control,
                        &mut highest_revision,
                        &mut commands,
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
                )
                .await?
                {
                    continue;
                }
                return Ok(());
            }
        };

        let discovered = match discover_or_command(provider.as_mut(), &mut commands).await {
            Operation::Completed(result) => result,
            Operation::Command(command) => {
                let _ = provider.stop();
                match command {
                    Some(command) => {
                        apply_command(&database, &mut control, &mut highest_revision, command)
                            .await?;
                        continue 'supervisor;
                    }
                    None => return Ok(()),
                }
            }
        };
        if let Err(error) = discovered {
            let _ = provider.stop();
            persist_collector_state(&database, control, Some(&error)).await?;
            if error.retryable {
                let delay = RETRY_DELAYS[retry_index.min(RETRY_DELAYS.len() - 1)];
                retry_index = (retry_index + 1).min(RETRY_DELAYS.len() - 1);
                if wait_or_command(
                    &database,
                    &mut control,
                    &mut highest_revision,
                    &mut commands,
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
            )
            .await?
            {
                continue;
            }
            return Ok(());
        }
        retry_index = 0;
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
                let _ = provider.stop();
                continue 'supervisor;
            }

            let records = match poll_or_command(provider.as_mut(), &mut commands).await {
                Operation::Completed(result) => result,
                Operation::Command(command) => {
                    let _ = provider.stop();
                    match command {
                        Some(command) => {
                            apply_command(&database, &mut control, &mut highest_revision, command)
                                .await?;
                            continue 'supervisor;
                        }
                        None => return Ok(()),
                    }
                }
            };

            match records {
                Ok(records) => {
                    let mut persistence_failed = false;
                    for record in records {
                        let preparation =
                            prepare_record(&database_path, control, record, &mut spool_paused);
                        tokio::pin!(preparation);
                        let commit = tokio::select! {
                            biased;
                            command = commands.recv() => {
                                let _ = provider.stop();
                                match command {
                                    Some(command) => {
                                        apply_command(&database, &mut control, &mut highest_revision, command).await?;
                                        continue 'supervisor;
                                    }
                                    None => return Ok(()),
                                }
                            }
                            result = &mut preparation => result,
                        };
                        let Ok(commit) = commit else {
                            persistence_failed = true;
                            break;
                        };
                        match commands.try_recv() {
                            Ok(command) => {
                                let _ = provider.stop();
                                apply_command(
                                    &database,
                                    &mut control,
                                    &mut highest_revision,
                                    command,
                                )
                                .await?;
                                continue 'supervisor;
                            }
                            Err(mpsc::error::TryRecvError::Disconnected) => {
                                let _ = provider.stop();
                                return Ok(());
                            }
                            Err(mpsc::error::TryRecvError::Empty) => {}
                        }
                        if database
                            .commit_communication_message(&commit)
                            .await
                            .map_err(|_| LocalPersistenceError::Database)
                            .is_err()
                        {
                            persistence_failed = true;
                            break;
                        }
                    }
                    if persistence_failed {
                        persist_collector_code(
                            &database,
                            control,
                            "WECHAT_LOCAL_SPOOL_UNAVAILABLE",
                        )
                        .await?;
                    } else {
                        persist_collector_state(&database, control, None).await?;
                    }
                }
                Err(error) => {
                    let _ = provider.stop();
                    persist_collector_state(&database, control, Some(&error)).await?;
                    if error.retryable {
                        let delay = RETRY_DELAYS[retry_index.min(RETRY_DELAYS.len() - 1)];
                        retry_index = (retry_index + 1).min(RETRY_DELAYS.len() - 1);
                        if wait_or_command(
                            &database,
                            &mut control,
                            &mut highest_revision,
                            &mut commands,
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
                POLL_INTERVAL,
            )
            .await?
            {
                PollWait::Elapsed => {}
                PollWait::CommandApplied => {
                    let _ = provider.stop();
                    continue 'supervisor;
                }
                PollWait::Closed => {
                    let _ = provider.stop();
                    return Ok(());
                }
            }
        }
    }
}

async fn discover_or_command(
    provider: &mut dyn CommunicationProvider,
    commands: &mut mpsc::Receiver<Command>,
) -> Operation<Result<(), DomainError>> {
    tokio::select! {
        biased;
        command = commands.recv() => Operation::Command(command),
        result = provider.discover() => Operation::Completed(result),
    }
}

async fn poll_or_command(
    provider: &mut dyn CommunicationProvider,
    commands: &mut mpsc::Receiver<Command>,
) -> Operation<Result<Vec<NormalizedCommunicationRecord>, DomainError>> {
    tokio::select! {
        biased;
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
    let result = if control.identity.is_none() {
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

async fn wait_or_command(
    database: &DbActorHandle,
    control: &mut CommunicationControl,
    highest_revision: &mut u64,
    commands: &mut mpsc::Receiver<Command>,
    delay: Duration,
) -> Result<bool, CommunicationRuntimeError> {
    tokio::select! {
        biased;
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
    delay: Duration,
) -> Result<PollWait, CommunicationRuntimeError> {
    tokio::select! {
        biased;
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
) -> Result<bool, CommunicationRuntimeError> {
    match commands.recv().await {
        Some(command) => {
            apply_command(database, control, highest_revision, command).await?;
            Ok(true)
        }
        None => Ok(false),
    }
}

fn update_hysteresis(paused: &mut bool, value: u64, high: u64, low: u64) {
    if value > high {
        *paused = true;
    } else if value < low {
        *paused = false;
    }
}

async fn prepare_record(
    database_path: &Path,
    control: CommunicationControl,
    record: NormalizedCommunicationRecord,
    spool_paused: &mut bool,
) -> Result<CommunicationMessageCommit, LocalPersistenceError> {
    let identity = control
        .identity
        .ok_or(LocalPersistenceError::InvalidRecord)?;
    let (account_id, source_sequence, message, completed_media) = record.into_parts();
    let attachment_spool = copy_completed_media(
        database_path,
        message.attachments(),
        &completed_media,
        spool_paused,
    )
    .await?;
    let created_at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|_| LocalPersistenceError::Clock)?;
    let payload = serde_json::to_value(&message)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .ok_or(LocalPersistenceError::InvalidRecord)?;
    let event = EventEnvelope {
        event_id: Uuid::new_v4().hyphenated().to_string(),
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
    Ok(CommunicationMessageCommit {
        account_id,
        source_sequence,
        event,
        message,
        attachment_spool,
    })
}

#[derive(Debug)]
enum LocalPersistenceError {
    Database,
    Filesystem,
    InvalidRecord,
    Quota,
    Clock,
}

async fn copy_completed_media(
    database_path: &Path,
    attachments: &[pca_domain::CommunicationAttachment],
    completed_media: &[CompletedMediaSource],
    spool_paused: &mut bool,
) -> Result<Vec<CommunicationAttachmentSpoolReference>, LocalPersistenceError> {
    if attachments.is_empty() {
        return Ok(Vec::new());
    }
    let root_path = DbActorHandle::communication_spool_root(database_path);
    let root = open_spool_root(&root_path)?;
    let usage = spool_usage(&root_path, &root)?;
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
    for attachment in attachments {
        let source = completed_media
            .iter()
            .find(|source| source.attachment_id() == attachment.attachment_id())
            .ok_or(LocalPersistenceError::InvalidRecord)?;
        copy_one(
            &root,
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
    Ok(references)
}

async fn copy_one(
    root: &File,
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
    let mut cleanup = PartialFile::new(root, temporary_name)?;
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
    rustix::fs::renameat(root, cleanup.name(), root, expected_hash)
        .map_err(|_| LocalPersistenceError::Filesystem)?;
    cleanup.disarm();
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
    validate_open_file(final_file, expected_hash, expected_size).await
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
    let root = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|_| LocalPersistenceError::Filesystem)?;
    let metadata = root
        .metadata()
        .map_err(|_| LocalPersistenceError::Filesystem)?;
    if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(LocalPersistenceError::Filesystem);
    }
    Ok(root)
}

fn spool_usage(path: &Path, root: &File) -> Result<u64, LocalPersistenceError> {
    let opened = root
        .metadata()
        .map_err(|_| LocalPersistenceError::Filesystem)?;
    ensure_same_root(path, &opened)?;
    let mut total = 0_u64;
    for entry in fs::read_dir(path).map_err(|_| LocalPersistenceError::Filesystem)? {
        let entry = entry.map_err(|_| LocalPersistenceError::Filesystem)?;
        let metadata =
            fs::symlink_metadata(entry.path()).map_err(|_| LocalPersistenceError::Filesystem)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(LocalPersistenceError::Filesystem);
        }
        total = total
            .checked_add(metadata.len())
            .ok_or(LocalPersistenceError::Quota)?;
    }
    ensure_same_root(path, &opened)?;
    Ok(total)
}

fn ensure_same_root(path: &Path, opened: &Metadata) -> Result<(), LocalPersistenceError> {
    let current = fs::symlink_metadata(path).map_err(|_| LocalPersistenceError::Filesystem)?;
    if current.file_type().is_symlink()
        || !current.is_dir()
        || current.dev() != opened.dev()
        || current.ino() != opened.ino()
    {
        return Err(LocalPersistenceError::Filesystem);
    }
    Ok(())
}

struct PartialFile {
    root: File,
    name: String,
    armed: bool,
}

impl PartialFile {
    fn new(root: &File, name: String) -> Result<Self, LocalPersistenceError> {
        Ok(Self {
            root: root
                .try_clone()
                .map_err(|_| LocalPersistenceError::Filesystem)?,
            name,
            armed: true,
        })
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PartialFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = rustix::fs::unlinkat(&self.root, self.name.as_str(), AtFlags::empty());
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
    database
        .upsert_collector_state(&CollectorState {
            collector_key: COLLECTOR_KEY.to_owned(),
            collector_version: env!("CARGO_PKG_VERSION").to_owned(),
            status,
            desired_config_revision: revision,
            applied_config_revision: revision,
            last_event_at_ms: None,
            last_health_at_ms: Some(now_ms),
            last_error_code: error_code.map(str::to_owned),
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        })
        .await
        .map_err(CommunicationRuntimeError::Database)
}

/// Fail-closed factory installed by production until a versioned source factory is available.
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
