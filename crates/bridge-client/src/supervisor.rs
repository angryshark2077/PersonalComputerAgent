use std::{
    fs,
    future::Future,
    io,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::Component,
    path::{Path, PathBuf},
    process::{Command as StdCommand, ExitStatus, Stdio},
    sync::Arc,
    time::Duration,
};

use pca_domain::BridgeStatus;
use pca_keychain::CredentialStore;
use rand::{rngs::OsRng, RngCore};
use thiserror::Error;
use tokio::{
    process::{Child, Command},
    sync::{mpsc, oneshot, watch},
    time::{sleep, Instant},
};

use crate::{
    BridgeClient, BridgeClientConfig, BridgeClientError, NetworkObservationState, PhotoAssetRecord,
    PlatformLifecycleEvent, ScreenCaptureResult, ScreenContext,
};

const MAX_BACKOFF: Duration = Duration::from_secs(30);
const DEFAULT_BACKOFF: Duration = Duration::from_millis(250);
const DEFAULT_STABLE_READY: Duration = Duration::from_secs(10);
const CONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(10);
const CHILD_REAP_RETRY_BACKOFF: Duration = Duration::from_millis(10);
const MAX_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_STABLE_READY: Duration = Duration::from_secs(30);
const NETWORK_OBSERVATION_INTERVAL: Duration = Duration::from_mins(30);
const NETWORK_ENABLE_POLL_INTERVAL: Duration = Duration::from_secs(1);
const LIFECYCLE_POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
pub struct ScreenCaptureCommandHandle {
    sender: mpsc::Sender<ScreenCaptureCommand>,
}

impl ScreenCaptureCommandHandle {
    /// Reads the current lock and activity state through the supervised Bridge connection.
    ///
    /// # Errors
    ///
    /// Returns `Disconnected` when the supervisor is unavailable, otherwise the Bridge error.
    pub async fn context(&self) -> Result<ScreenContext, BridgeClientError> {
        let (sender, receiver) = oneshot::channel();
        self.sender
            .send(ScreenCaptureCommand::Context { response: sender })
            .await
            .map_err(|_| BridgeClientError::Disconnected)?;
        receiver
            .await
            .map_err(|_| BridgeClientError::Disconnected)?
    }

    /// Captures the active display through the supervised Bridge connection.
    ///
    /// # Errors
    ///
    /// Returns `Disconnected` when the supervisor is unavailable, otherwise the Bridge error.
    pub async fn capture(
        &self,
        excluded_bundle_ids: Vec<String>,
    ) -> Result<ScreenCaptureResult, BridgeClientError> {
        let (sender, receiver) = oneshot::channel();
        self.sender
            .send(ScreenCaptureCommand::Capture {
                excluded_bundle_ids,
                response: sender,
            })
            .await
            .map_err(|_| BridgeClientError::Disconnected)?;
        receiver
            .await
            .map_err(|_| BridgeClientError::Disconnected)?
    }

    /// Decodes Apple attributed message bodies using the supervised Bridge.
    ///
    /// # Errors
    ///
    /// Returns `Disconnected` when supervision stops, otherwise the Bridge error.
    pub async fn decode_message_bodies(
        &self,
        encoded_bodies: Vec<String>,
    ) -> Result<Vec<Option<String>>, BridgeClientError> {
        let (response, receiver) = oneshot::channel();
        self.sender
            .send(ScreenCaptureCommand::DecodeMessages {
                encoded_bodies,
                response,
            })
            .await
            .map_err(|_| BridgeClientError::Disconnected)?;
        receiver
            .await
            .map_err(|_| BridgeClientError::Disconnected)?
    }

    /// Reads the Photo Library authorization status using the supervised Bridge.
    ///
    /// # Errors
    ///
    /// Returns `Disconnected` when supervision stops, otherwise the Bridge error.
    pub async fn photo_authorization(&self) -> Result<String, BridgeClientError> {
        let (response, receiver) = oneshot::channel();
        self.sender
            .send(ScreenCaptureCommand::PhotoAuthorization { response })
            .await
            .map_err(|_| BridgeClientError::Disconnected)?;
        receiver
            .await
            .map_err(|_| BridgeClientError::Disconnected)?
    }

    /// Lists Photo Library assets using the supervised Bridge.
    ///
    /// # Errors
    ///
    /// Returns `Disconnected` when supervision stops, otherwise the Bridge error.
    pub async fn list_photo_assets(
        &self,
        after_created_at: Option<String>,
        after_local_identifier: Option<String>,
        cutoff: String,
        limit: u8,
    ) -> Result<(String, Vec<PhotoAssetRecord>), BridgeClientError> {
        let (response, receiver) = oneshot::channel();
        self.sender
            .send(ScreenCaptureCommand::ListPhotos {
                after_created_at,
                after_local_identifier,
                cutoff,
                limit,
                response,
            })
            .await
            .map_err(|_| BridgeClientError::Disconnected)?;
        receiver
            .await
            .map_err(|_| BridgeClientError::Disconnected)?
    }

    /// Exports one original Photo Library asset using the supervised Bridge.
    ///
    /// # Errors
    ///
    /// Returns `Disconnected` when supervision stops, otherwise the Bridge error.
    pub async fn export_photo_asset(
        &self,
        local_identifier: String,
        file_name: uuid::Uuid,
    ) -> Result<Option<PathBuf>, BridgeClientError> {
        let (response, receiver) = oneshot::channel();
        self.sender
            .send(ScreenCaptureCommand::ExportPhoto {
                local_identifier,
                file_name,
                response,
            })
            .await
            .map_err(|_| BridgeClientError::Disconnected)?;
        receiver
            .await
            .map_err(|_| BridgeClientError::Disconnected)?
    }
}

#[derive(Debug)]
pub struct ScreenCaptureCommandReceiver {
    receiver: mpsc::Receiver<ScreenCaptureCommand>,
}

#[derive(Debug)]
enum ScreenCaptureCommand {
    Context {
        response: oneshot::Sender<Result<ScreenContext, BridgeClientError>>,
    },
    Capture {
        excluded_bundle_ids: Vec<String>,
        response: oneshot::Sender<Result<ScreenCaptureResult, BridgeClientError>>,
    },
    DecodeMessages {
        encoded_bodies: Vec<String>,
        response: oneshot::Sender<Result<Vec<Option<String>>, BridgeClientError>>,
    },
    PhotoAuthorization {
        response: oneshot::Sender<Result<String, BridgeClientError>>,
    },
    ListPhotos {
        after_created_at: Option<String>,
        after_local_identifier: Option<String>,
        cutoff: String,
        limit: u8,
        response: oneshot::Sender<Result<(String, Vec<PhotoAssetRecord>), BridgeClientError>>,
    },
    ExportPhoto {
        local_identifier: String,
        file_name: uuid::Uuid,
        response: oneshot::Sender<Result<Option<PathBuf>, BridgeClientError>>,
    },
}

#[must_use]
pub fn screen_capture_command_channel() -> (ScreenCaptureCommandHandle, ScreenCaptureCommandReceiver)
{
    let (sender, receiver) = mpsc::channel(8);
    (
        ScreenCaptureCommandHandle { sender },
        ScreenCaptureCommandReceiver { receiver },
    )
}

#[derive(Clone, Debug)]
pub struct BridgeSupervisorConfig {
    executable_path: PathBuf,
    client_config: BridgeClientConfig,
    operation_timeout: Duration,
    backoff_base: Duration,
    backoff_cap: Duration,
    stable_ready: Duration,
}

impl BridgeSupervisorConfig {
    /// Creates a supervisor configuration with exact absolute executable and socket paths.
    ///
    /// # Errors
    ///
    /// Rejects non-absolute paths, parent traversal, a missing/non-executable regular executable,
    /// an untrusted socket parent, and invalid client settings. The caller must ensure the socket
    /// parent is the per-user runtime directory owned by the current user and that existing parent
    /// path components are not replaced with symlinks after validation.
    pub fn new(
        executable_path: impl AsRef<Path>,
        socket_path: impl AsRef<Path>,
        agent_version: impl Into<String>,
    ) -> Result<Self, BridgeClientError> {
        let executable_path = executable_path.as_ref();
        if !executable_path.is_absolute()
            || executable_path.as_os_str().is_empty()
            || executable_path == Path::new("/")
            || has_parent_traversal(executable_path)
        {
            return Err(BridgeClientError::InvalidConfiguration);
        }
        let executable_metadata = fs::symlink_metadata(executable_path)
            .map_err(|_| BridgeClientError::InvalidConfiguration)?;
        if !executable_metadata.file_type().is_file()
            || executable_metadata.permissions().mode() & 0o111 == 0
        {
            return Err(BridgeClientError::InvalidConfiguration);
        }
        let socket_path = socket_path.as_ref();
        let socket_parent = socket_path
            .parent()
            .ok_or(BridgeClientError::InvalidConfiguration)?;
        if socket_parent == Path::new("/") {
            return Err(BridgeClientError::InvalidConfiguration);
        }
        let parent_metadata = fs::symlink_metadata(socket_parent)
            .map_err(|_| BridgeClientError::InvalidConfiguration)?;
        if !parent_metadata.file_type().is_dir() {
            return Err(BridgeClientError::InvalidConfiguration);
        }
        let client_config = BridgeClientConfig::new(socket_path, agent_version)?;
        Ok(Self {
            executable_path: executable_path.to_path_buf(),
            client_config,
            operation_timeout: Duration::from_secs(1),
            backoff_base: DEFAULT_BACKOFF,
            backoff_cap: MAX_BACKOFF,
            stable_ready: DEFAULT_STABLE_READY,
        })
    }

    #[must_use]
    pub fn with_operation_timeout(mut self, timeout: Duration) -> Self {
        if !timeout.is_zero() {
            self.operation_timeout = timeout.min(MAX_OPERATION_TIMEOUT);
            self.client_config = self.client_config.with_timeout(self.operation_timeout);
        }
        self
    }

    #[must_use]
    pub fn with_backoff(mut self, base: Duration, cap: Duration, stable_ready: Duration) -> Self {
        if !base.is_zero() {
            self.backoff_base = base.min(MAX_BACKOFF);
        }
        if !cap.is_zero() {
            self.backoff_cap = cap.min(MAX_BACKOFF).max(self.backoff_base);
        }
        if !stable_ready.is_zero() {
            self.stable_ready = stable_ready.min(MAX_STABLE_READY);
        }
        self
    }
}

#[derive(Debug, Error)]
pub enum BridgeSupervisorError {
    #[error("Bridge process cleanup failed")]
    ProcessCleanup,
}

pub struct BridgeSupervisor {
    config: BridgeSupervisorConfig,
    credential_store: Arc<dyn CredentialStore>,
    statuses: watch::Sender<BridgeStatus>,
    network_observations: Arc<NetworkObservationState>,
    lifecycle_events: Option<mpsc::Sender<PlatformLifecycleEvent>>,
    screen_capture_commands: Option<ScreenCaptureCommandReceiver>,
}

impl BridgeSupervisor {
    #[must_use]
    pub fn new(
        config: BridgeSupervisorConfig,
        credential_store: Arc<dyn CredentialStore>,
        statuses: watch::Sender<BridgeStatus>,
    ) -> Self {
        Self::new_with_network(
            config,
            credential_store,
            statuses,
            Arc::new(NetworkObservationState::default()),
        )
    }

    #[must_use]
    pub fn new_with_network(
        config: BridgeSupervisorConfig,
        credential_store: Arc<dyn CredentialStore>,
        statuses: watch::Sender<BridgeStatus>,
        network_observations: Arc<NetworkObservationState>,
    ) -> Self {
        Self {
            config,
            credential_store,
            statuses,
            network_observations,
            lifecycle_events: None,
            screen_capture_commands: None,
        }
    }

    #[must_use]
    pub fn with_lifecycle_events(
        mut self,
        lifecycle_events: mpsc::Sender<PlatformLifecycleEvent>,
    ) -> Self {
        self.lifecycle_events = Some(lifecycle_events);
        self
    }

    #[must_use]
    pub fn with_screen_capture_commands(mut self, commands: ScreenCaptureCommandReceiver) -> Self {
        self.screen_capture_commands = Some(commands);
        self
    }

    /// Runs the Bridge child lifecycle until cancellation or protocol incompatibility.
    ///
    /// The signed helper executable is started directly with exactly `--socket <absolute-path>`;
    /// its production app wrapper relaunches the server through `LaunchServices` so macOS privacy
    /// permissions are attributed to the signed helper bundle. No shell, command interpolation,
    /// secret environment variable, or inherited standard stream is used.
    ///
    /// # Errors
    ///
    /// Returns only when confirmed socket cleanup fails. Child termination deliberately has no
    /// wall-clock deadline: the supervisor retries kill/wait observations and retains ownership
    /// until `wait` or `try_wait` confirms reap, even if process APIs repeatedly fail.
    #[allow(clippy::too_many_lines)] // Child, bridge, network, and lifecycle signals share one cancellation boundary.
    pub async fn run(
        mut self,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(), BridgeSupervisorError> {
        let mut status = StatusEmitter::new(self.statuses);
        status.emit(BridgeStatus::Disconnected);
        let mut backoff = Backoff::new(self.config.backoff_base, self.config.backoff_cap);

        loop {
            if *shutdown.borrow() {
                return Ok(());
            }
            let Ok(mut child) = spawn_bridge(&self.config) else {
                status.emit(BridgeStatus::Degraded);
                if wait_or_cancel(backoff.next_delay(), &mut shutdown).await {
                    return Ok(());
                }
                continue;
            };
            status.emit(BridgeStatus::Handshaking);

            let connection = connect_until_ready(
                &self.config,
                Arc::clone(&self.credential_store),
                &mut child,
                &mut shutdown,
            )
            .await;
            let mut client = match connection {
                ConnectOutcome::Ready(client) => client,
                ConnectOutcome::Cancelled => {
                    cleanup_child(&mut child, self.config.client_config.socket_path()).await?;
                    return Ok(());
                }
                ConnectOutcome::Incompatible => {
                    cleanup_child(&mut child, self.config.client_config.socket_path()).await?;
                    status.emit(BridgeStatus::Incompatible);
                    return Ok(());
                }
                ConnectOutcome::Failed => {
                    cleanup_child(&mut child, self.config.client_config.socket_path()).await?;
                    status.emit(BridgeStatus::Degraded);
                    if wait_or_cancel(backoff.next_delay(), &mut shutdown).await {
                        return Ok(());
                    }
                    continue;
                }
            };

            status.emit(BridgeStatus::Ready);
            let ready_at = Instant::now();
            let mut network_timer = tokio::time::interval(NETWORK_ENABLE_POLL_INTERVAL);
            network_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut last_network_observation: Option<Instant> = None;
            let mut lifecycle_timer = tokio::time::interval(LIFECYCLE_POLL_INTERVAL);
            lifecycle_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut last_lifecycle_sequence = 0_u64;
            let child_exited = loop {
                tokio::select! {
                    biased;
                    changed = shutdown.changed() => {
                        let _ = changed;
                        cleanup_child(&mut child, self.config.client_config.socket_path()).await?;
                        return Ok(());
                    }
                    result = child.wait() => {
                        if result.is_err() {
                            reap_child(&mut child).await;
                        }
                        break true;
                    }
                    _ = lifecycle_timer.tick(), if self.lifecycle_events.is_some() => {
                        let Some(sender) = self.lifecycle_events.as_ref() else {
                            continue;
                        };
                        match poll_and_forward_lifecycle(&mut client, sender, last_lifecycle_sequence).await {
                            Ok((latest_sequence, observe_network)) => {
                                last_lifecycle_sequence = latest_sequence;
                                if observe_network && self.network_observations.is_enabled() {
                                    match client.observe_network().await {
                                        Ok(observation) => {
                                            self.network_observations.replace(observation);
                                            last_network_observation = Some(Instant::now());
                                        }
                                        Err(error) if !network_observation_error_requires_reconnect(&error) => {
                                            last_network_observation = Some(Instant::now());
                                        }
                                        Err(_) => break false,
                                    }
                                }
                            }
                            Err(()) => break false,
                        }
                    }
                    _ = network_timer.tick() => {
                        if !self.network_observations.is_enabled()
                            || last_network_observation.is_some_and(|last| last.elapsed() < NETWORK_OBSERVATION_INTERVAL)
                        {
                            continue;
                        }
                        match client.observe_network().await {
                            Ok(observation) => {
                                self.network_observations.replace(observation);
                                last_network_observation = Some(Instant::now());
                            }
                            Err(error) if !network_observation_error_requires_reconnect(&error) => {
                                last_network_observation = Some(Instant::now());
                            }
                            Err(_) => break false,
                        }
                    }
                    command = receive_screen_capture_command(&mut self.screen_capture_commands), if self.screen_capture_commands.is_some() => {
                        let Some(command) = command else { continue };
                        match command {
                            ScreenCaptureCommand::Context { response } => {
                                let _ = response.send(client.screen_context().await);
                            }
                            ScreenCaptureCommand::Capture { excluded_bundle_ids, response } => {
                                let _ = response.send(client.capture_screen(&excluded_bundle_ids).await);
                            }
                            ScreenCaptureCommand::DecodeMessages { encoded_bodies, response } => {
                                let _ = response.send(client.decode_message_bodies(&encoded_bodies).await);
                            }
                            ScreenCaptureCommand::PhotoAuthorization { response } => {
                                let _ = response.send(client.photo_authorization().await);
                            }
                            ScreenCaptureCommand::ListPhotos { after_created_at, after_local_identifier, cutoff, limit, response } => {
                                let _ = response.send(client.list_photo_assets(
                                    after_created_at.as_deref(), after_local_identifier.as_deref(), &cutoff, limit,
                                ).await);
                            }
                            ScreenCaptureCommand::ExportPhoto { local_identifier, file_name, response } => {
                                let _ = response.send(client.export_photo_asset(&local_identifier, file_name).await);
                            }
                        }
                    }
                }
            };
            drop(client);
            if child_exited {
                remove_confirmed_socket(self.config.client_config.socket_path())?;
            } else {
                cleanup_child(&mut child, self.config.client_config.socket_path()).await?;
            }
            if ready_at.elapsed() >= self.config.stable_ready {
                backoff.reset();
            }
            if child_exited || !*shutdown.borrow() {
                status.emit(BridgeStatus::Degraded);
                if wait_or_cancel(backoff.next_delay(), &mut shutdown).await {
                    return Ok(());
                }
            }
        }
    }
}

async fn receive_screen_capture_command(
    commands: &mut Option<ScreenCaptureCommandReceiver>,
) -> Option<ScreenCaptureCommand> {
    match commands {
        Some(commands) => commands.receiver.recv().await,
        None => std::future::pending().await,
    }
}

async fn poll_and_forward_lifecycle(
    client: &mut BridgeClient,
    sender: &mpsc::Sender<PlatformLifecycleEvent>,
    after_sequence: u64,
) -> Result<(u64, bool), ()> {
    let (events, latest_sequence) = client
        .poll_lifecycle(after_sequence)
        .await
        .map_err(|_| ())?;
    let observe_network = lifecycle_events_require_network_observation(&events);
    for event in events {
        sender.send(event).await.map_err(|_| ())?;
    }
    Ok((latest_sequence, observe_network))
}

fn lifecycle_events_require_network_observation(events: &[PlatformLifecycleEvent]) -> bool {
    events.iter().any(|event| {
        matches!(
            event.event_type.as_str(),
            "network.offline" | "network.online" | "network.changed"
        )
    })
}

fn network_observation_error_requires_reconnect(error: &BridgeClientError) -> bool {
    !matches!(error, BridgeClientError::InvalidEnvelope)
}

enum ConnectOutcome {
    Ready(BridgeClient),
    Incompatible,
    Failed,
    Cancelled,
}

async fn connect_until_ready(
    config: &BridgeSupervisorConfig,
    credential_store: Arc<dyn CredentialStore>,
    child: &mut Child,
    shutdown: &mut watch::Receiver<bool>,
) -> ConnectOutcome {
    let started = Instant::now();
    loop {
        if *shutdown.borrow() {
            return ConnectOutcome::Cancelled;
        }
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return ConnectOutcome::Failed,
            Ok(None) => {}
        }
        if started.elapsed() >= config.operation_timeout {
            return ConnectOutcome::Failed;
        }

        let remaining = config.operation_timeout.saturating_sub(started.elapsed());
        let connect = BridgeClient::connect_and_handshake(
            config.client_config.clone().with_timeout(remaining),
            Arc::clone(&credential_store),
        );
        let result = tokio::select! {
            biased;
            changed = shutdown.changed() => {
                let _ = changed;
                return ConnectOutcome::Cancelled;
            }
            result = connect => result,
        };
        match result {
            Ok(client) => return ConnectOutcome::Ready(client),
            Err(BridgeClientError::IncompatibleProtocol { .. }) => {
                return ConnectOutcome::Incompatible;
            }
            Err(BridgeClientError::ConnectionFailed)
                if started.elapsed() < config.operation_timeout =>
            {
                if wait_or_cancel(CONNECT_RETRY_INTERVAL, shutdown).await {
                    return ConnectOutcome::Cancelled;
                }
            }
            Err(_) => return ConnectOutcome::Failed,
        }
    }
}

fn spawn_bridge(config: &BridgeSupervisorConfig) -> Result<Child, ()> {
    Command::new(&config.executable_path)
        .arg("--socket")
        .arg(config.client_config.socket_path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| ())
}

async fn cleanup_child(child: &mut Child, socket_path: &Path) -> Result<(), BridgeSupervisorError> {
    reap_child(child).await;
    remove_confirmed_socket(socket_path)
}

trait ReapProcess {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>>;
    fn start_kill(&mut self) -> io::Result<()>;
    fn wait(&mut self) -> impl Future<Output = io::Result<ExitStatus>>;
}

impl ReapProcess for Child {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        Child::try_wait(self)
    }

    fn start_kill(&mut self) -> io::Result<()> {
        let process_id = self
            .id()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        let status = StdCommand::new("/bin/kill")
            .args(["-TERM", "--", &process_id.to_string()])
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other("could not terminate Bridge wrapper"))
        }
    }

    fn wait(&mut self) -> impl Future<Output = io::Result<ExitStatus>> {
        Child::wait(self)
    }
}

async fn reap_child(child: &mut impl ReapProcess) {
    // `kill_on_drop` is a last-resort process fallback, never evidence of reap. The normal
    // SIGTERM path lets the production wrapper forward termination to its LaunchServices child.
    // Once Child is owned, even pathological termination/wait errors keep this loop pending until
    // an explicit wait observation confirms the child has exited and been reaped.
    loop {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        if child.start_kill().is_err() && matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        if child.wait().await.is_ok() || matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        sleep(CHILD_REAP_RETRY_BACKOFF).await;
    }
}

fn remove_confirmed_socket(socket_path: &Path) -> Result<(), BridgeSupervisorError> {
    match fs::symlink_metadata(socket_path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            fs::remove_file(socket_path).map_err(|_| BridgeSupervisorError::ProcessCleanup)
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(BridgeSupervisorError::ProcessCleanup),
    }
}

async fn wait_or_cancel(duration: Duration, shutdown: &mut watch::Receiver<bool>) -> bool {
    if *shutdown.borrow() {
        return true;
    }
    tokio::select! {
        biased;
        changed = shutdown.changed() => {
            let _ = changed;
            true
        }
        () = sleep(duration) => false,
    }
}

struct StatusEmitter {
    sender: watch::Sender<BridgeStatus>,
    last: Option<BridgeStatus>,
}

impl StatusEmitter {
    fn new(sender: watch::Sender<BridgeStatus>) -> Self {
        Self { sender, last: None }
    }

    fn emit(&mut self, status: BridgeStatus) {
        if self.last != Some(status) {
            self.sender.send_replace(status);
            self.last = Some(status);
        }
    }
}

fn has_parent_traversal(path: &Path) -> bool {
    path.components()
        .any(|component| component == Component::ParentDir)
}

struct Backoff {
    base: Duration,
    cap: Duration,
    current: Duration,
}

impl Backoff {
    fn new(base: Duration, cap: Duration) -> Self {
        Self {
            base,
            cap,
            current: base,
        }
    }

    fn next_delay(&mut self) -> Duration {
        let nominal = self.current.min(self.cap).min(MAX_BACKOFF);
        self.current = self.current.saturating_mul(2).min(self.cap);
        let jitter_span = nominal / 4;
        let jitter = if jitter_span.is_zero() {
            Duration::ZERO
        } else {
            let upper = u64::try_from(jitter_span.as_nanos()).unwrap_or(u64::MAX);
            Duration::from_nanos(OsRng.next_u64() % upper)
        };
        nominal.saturating_sub(jitter_span).saturating_add(jitter)
    }

    fn reset(&mut self) {
        self.current = self.base;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        lifecycle_events_require_network_observation, network_observation_error_requires_reconnect,
        reap_child, remove_confirmed_socket, Backoff, BridgeSupervisorConfig, ReapProcess,
        StatusEmitter, MAX_BACKOFF, MAX_OPERATION_TIMEOUT, MAX_STABLE_READY,
    };
    use crate::{BridgeClientError, PlatformLifecycleEvent};
    use pca_domain::BridgeStatus;
    use std::{
        collections::VecDeque,
        fs,
        future::{ready, Future},
        io,
        os::unix::process::ExitStatusExt,
        os::unix::{fs::symlink, fs::PermissionsExt, net::UnixListener},
        process::ExitStatus,
        time::Duration,
    };
    use tokio::sync::watch;
    use uuid::Uuid;

    #[test]
    fn backoff_is_jittered_bounded_and_resettable() {
        let base = Duration::from_secs(4);
        let mut backoff = Backoff::new(base, Duration::from_secs(31));
        let first = backoff.next_delay();
        assert!((Duration::from_secs(3)..=base).contains(&first));

        for _ in 0..20 {
            assert!(backoff.next_delay() <= MAX_BACKOFF);
        }

        backoff.reset();
        let after_reset = backoff.next_delay();
        assert!((Duration::from_secs(3)..=base).contains(&after_reset));
    }

    #[test]
    fn status_updates_are_coalesced_and_survive_a_closed_consumer() {
        let (sender, mut receiver) = watch::channel(BridgeStatus::Disconnected);
        let mut emitter = StatusEmitter::new(sender);
        for status in [
            BridgeStatus::Handshaking,
            BridgeStatus::Ready,
            BridgeStatus::Degraded,
            BridgeStatus::Ready,
        ] {
            emitter.emit(status);
        }
        assert_eq!(*receiver.borrow_and_update(), BridgeStatus::Ready);
        assert!(!receiver.has_changed().expect("watch remains open"));

        drop(receiver);
        emitter.emit(BridgeStatus::Degraded);
        assert_eq!(*emitter.sender.borrow(), BridgeStatus::Degraded);
        assert_eq!(emitter.sender.receiver_count(), 0);
    }

    #[test]
    fn invalid_network_sample_does_not_restart_an_otherwise_healthy_bridge() {
        assert!(!network_observation_error_requires_reconnect(
            &BridgeClientError::InvalidEnvelope
        ));
        assert!(network_observation_error_requires_reconnect(
            &BridgeClientError::Timeout
        ));
    }

    #[test]
    fn network_lifecycle_events_trigger_an_immediate_observation() {
        let event = |event_type: &str| PlatformLifecycleEvent {
            sequence: 1,
            event_id: Uuid::nil(),
            event_type: event_type.to_owned(),
            occurred_at: "2026-08-07T00:00:00Z".to_owned(),
        };

        assert!(lifecycle_events_require_network_observation(&[event(
            "network.changed"
        )]));
        assert!(lifecycle_events_require_network_observation(&[event(
            "network.online"
        )]));
        assert!(lifecycle_events_require_network_observation(&[event(
            "network.offline"
        )]));
        assert!(!lifecycle_events_require_network_observation(&[event(
            "system.wake"
        )]));
    }

    #[test]
    fn confirmed_socket_cleanup_never_unlinks_regular_files_or_symlinks() {
        let directory = tempfile::tempdir().expect("tempdir");
        let socket = directory.path().join("bridge.sock");
        let listener = UnixListener::bind(&socket).expect("bind socket");
        drop(listener);
        remove_confirmed_socket(&socket).expect("remove confirmed socket");
        assert!(!socket.exists());

        fs::write(&socket, "unrelated").expect("regular file");
        remove_confirmed_socket(&socket).expect("preserve regular file");
        assert!(socket.is_file());
        fs::remove_file(&socket).expect("test cleanup");

        let target = directory.path().join("target");
        fs::write(&target, "unrelated").expect("symlink target");
        symlink(&target, &socket).expect("socket-path symlink");
        remove_confirmed_socket(&socket).expect("preserve symlink");
        assert!(fs::symlink_metadata(&socket)
            .expect("symlink metadata")
            .file_type()
            .is_symlink());
    }

    #[test]
    fn extreme_durations_are_clamped_without_instant_overflow() {
        let directory = tempfile::tempdir().expect("tempdir");
        let executable = directory.path().join("bridge");
        fs::write(&executable, "#!/bin/sh\nexit 0\n").expect("fake executable");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
            .expect("executable mode");
        let config = BridgeSupervisorConfig::new(
            &executable,
            directory.path().join("bridge.sock"),
            "0.0.0-s1a",
        )
        .expect("valid config")
        .with_operation_timeout(Duration::MAX)
        .with_backoff(Duration::MAX, Duration::MAX, Duration::MAX);
        assert_eq!(config.operation_timeout, MAX_OPERATION_TIMEOUT);
        assert_eq!(config.backoff_base, MAX_BACKOFF);
        assert_eq!(config.backoff_cap, MAX_BACKOFF);
        assert_eq!(config.stable_ready, MAX_STABLE_READY);
    }

    struct ErrorThenExitProcess {
        try_waits: VecDeque<io::Result<Option<ExitStatus>>>,
        kills: VecDeque<io::Result<()>>,
        waits: VecDeque<io::Result<ExitStatus>>,
    }

    impl ReapProcess for ErrorThenExitProcess {
        fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            self.try_waits.pop_front().expect("scripted try_wait")
        }

        fn start_kill(&mut self) -> io::Result<()> {
            self.kills.pop_front().expect("scripted start_kill")
        }

        fn wait(&mut self) -> impl Future<Output = io::Result<ExitStatus>> {
            ready(self.waits.pop_front().expect("scripted wait"))
        }
    }

    #[tokio::test]
    async fn reap_retries_api_errors_until_exit_is_confirmed() {
        let api_error = || io::Error::other("injected process API failure");
        let mut child = ErrorThenExitProcess {
            try_waits: VecDeque::from([
                Err(api_error()),
                Ok(None),
                Ok(None),
                Ok(Some(ExitStatus::from_raw(0))),
            ]),
            kills: VecDeque::from([Err(api_error())]),
            waits: VecDeque::from([Err(api_error())]),
        };

        reap_child(&mut child).await;

        assert!(child.try_waits.is_empty());
        assert!(child.kills.is_empty());
        assert!(child.waits.is_empty());
    }
}
