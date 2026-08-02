use std::{fs, sync::Arc, time::Duration};

#[cfg(feature = "process-test-hooks")]
use std::{fs::OpenOptions, io::Write, os::unix::fs::OpenOptionsExt, path::Path};

use pca_agent_runtime::{
    CrashMarkerGuard, LocalHeartbeatWriter, RuntimeError, RuntimePaths, RuntimeStateMachine,
    SingleInstanceGuard,
};
use pca_agentd::{
    cloud_control::{
        CloudControlCommands, CloudControlOwner, ControlClient, HttpControlClient,
        PRODUCTION_CLOUD_API_ORIGIN,
    },
    communication::{
        CommunicationAuthorization, CommunicationRuntime, UnavailableCommunicationProviderFactory,
    },
    pairing_ipc::{PairingIpcServer, PairingIpcServerError, PairingSocket},
};
use pca_bridge_client::supervisor::{
    BridgeSupervisor, BridgeSupervisorConfig, BridgeSupervisorError,
};
#[cfg(feature = "process-test-hooks")]
use pca_db_local::ProcessTestHooks;
use pca_db_local::{DbActorHandle, PairingState};
use pca_domain::{AgentStatus, BridgeStatus, RuntimeStatusEnvelope};
use pca_keychain::CredentialStore;
#[cfg(not(feature = "process-test-hooks"))]
use pca_keychain::MacOSKeychainStore;
#[cfg(feature = "process-test-hooks")]
use pca_keychain::{
    CredentialError, BRIDGE_CREDENTIAL_ACCOUNT, BRIDGE_CREDENTIAL_SERVICE,
    BRIDGE_SHARED_SECRET_LENGTH,
};
use reqwest::Url;
use time::{format_description::well_known::Rfc3339, Duration as TimeDuration, OffsetDateTime};
use tokio::{sync::watch, task::JoinHandle};
use uuid::Uuid;

#[cfg(feature = "process-test-hooks")]
use crate::config::ProcessTestFatalCleanupConfig;
use crate::{
    collector_registry::CollectorIdentity,
    config::{CommandConfig, RunConfig},
    lifecycle::{LifecycleRuntime, NoopCapabilityRefresher, RuntimeIdentity},
    system_runtime::SystemRuntimeHandle,
};

pub(crate) const EXIT_USAGE: u8 = 2;
const EXIT_UNHEALTHY: u8 = 1;
const EXIT_UNSUPPORTED: u8 = 3;
const EXIT_ALREADY_RUNNING: u8 = 4;
const EXIT_RUNTIME_FAILURE: u8 = 5;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const HEALTH_FRESHNESS: TimeDuration = TimeDuration::seconds(5);
const LIFECYCLE_CAPACITY: usize = 32;

pub(crate) async fn execute(command: CommandConfig) -> u8 {
    match command {
        CommandConfig::Run(config) => match run(&config).await {
            Ok(()) => 0,
            Err(AppError::AlreadyRunning) => {
                eprintln!("pca-agentd: already running");
                EXIT_ALREADY_RUNNING
            }
            Err(AppError::Failure(report)) => {
                report.log();
                EXIT_RUNTIME_FAILURE
            }
        },
        CommandConfig::Health(paths) => health(&paths),
        CommandConfig::PrepareSleep => {
            eprintln!(
                "pca-agentd: live prepare-sleep control is unsupported by Bridge protocol v1"
            );
            EXIT_UNSUPPORTED
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum FailureStage {
    Paths,
    CrashMarker,
    DatabaseOpen,
    DatabaseHealth,
    State,
    Lifecycle,
    SystemCollector,
    CommunicationCollector,
    Heartbeat,
    Signal,
    PairingConfiguration,
    ControlConfiguration,
    #[cfg(feature = "process-test-hooks")]
    BridgeConfiguration,
    #[cfg(feature = "process-test-hooks")]
    InjectedHeartbeat,
    LifecycleCleanup,
    SystemCollectorCleanup,
    CommunicationCollectorCleanup,
    BridgeCleanup,
    PairingCleanup,
    ControlCleanup,
    Checkpoint,
    FinalStatus,
    DatabaseOwnership,
    DatabaseShutdown,
    #[cfg(feature = "process-test-hooks")]
    CleanupEvidence,
}

impl FailureStage {
    const fn code(self) -> &'static str {
        match self {
            Self::Paths => "paths",
            Self::CrashMarker => "crash_marker",
            Self::DatabaseOpen => "database_open",
            Self::DatabaseHealth => "database_health",
            Self::State => "state",
            Self::Lifecycle => "lifecycle",
            Self::SystemCollector => "system_collector",
            Self::CommunicationCollector => "communication_collector",
            Self::Heartbeat => "heartbeat",
            Self::Signal => "signal",
            Self::PairingConfiguration => "pairing_configuration",
            Self::ControlConfiguration => "control_configuration",
            #[cfg(feature = "process-test-hooks")]
            Self::BridgeConfiguration => "bridge_configuration",
            #[cfg(feature = "process-test-hooks")]
            Self::InjectedHeartbeat => "injected_heartbeat",
            Self::LifecycleCleanup => "lifecycle_cleanup",
            Self::SystemCollectorCleanup => "system_collector_cleanup",
            Self::CommunicationCollectorCleanup => "communication_collector_cleanup",
            Self::BridgeCleanup => "bridge_cleanup",
            Self::PairingCleanup => "pairing_cleanup",
            Self::ControlCleanup => "control_cleanup",
            Self::Checkpoint => "checkpoint",
            Self::FinalStatus => "final_status",
            Self::DatabaseOwnership => "database_ownership",
            Self::DatabaseShutdown => "database_shutdown",
            #[cfg(feature = "process-test-hooks")]
            Self::CleanupEvidence => "cleanup_evidence",
        }
    }
}

#[derive(Debug)]
struct FailureReport {
    primary: FailureStage,
    cleanup: Vec<FailureStage>,
}

impl FailureReport {
    fn log(&self) {
        eprint!("pca-agentd: runtime failure stage={}", self.primary.code());
        if self.cleanup.is_empty() {
            eprintln!(" cleanup=none");
        } else {
            eprint!(" cleanup=");
            for (index, stage) in self.cleanup.iter().enumerate() {
                if index > 0 {
                    eprint!(",");
                }
                eprint!("{}", stage.code());
            }
            eprintln!();
        }
    }
}

#[derive(Debug)]
enum AppError {
    AlreadyRunning,
    Failure(FailureReport),
}

impl AppError {
    fn primary(stage: FailureStage) -> Self {
        Self::Failure(FailureReport {
            primary: stage,
            cleanup: Vec::new(),
        })
    }
}

struct RuntimeResources {
    database: Option<Arc<DbActorHandle>>,
    lifecycle: Option<LifecycleRuntime>,
    system_runtime: Option<SystemRuntimeHandle>,
    communication_runtime: Option<CommunicationRuntime>,
    bridge_shutdown: Option<watch::Sender<bool>>,
    bridge_task: Option<JoinHandle<Result<(), BridgeSupervisorError>>>,
    pairing_shutdown: Option<watch::Sender<bool>>,
    pairing_task: Option<JoinHandle<Result<(), PairingIpcServerError>>>,
    control: Option<CloudControlOwner>,
    heartbeat: Option<LocalHeartbeatWriter>,
    state: RuntimeStateMachine,
    schema_version: Option<u32>,
}

impl RuntimeResources {
    fn new(database: DbActorHandle) -> Self {
        Self {
            database: Some(Arc::new(database)),
            lifecycle: None,
            system_runtime: None,
            communication_runtime: None,
            bridge_shutdown: None,
            bridge_task: None,
            pairing_shutdown: None,
            pairing_task: None,
            control: None,
            heartbeat: None,
            state: RuntimeStateMachine::starting(),
            schema_version: None,
        }
    }

    fn database(&self) -> &DbActorHandle {
        self.database
            .as_deref()
            .expect("database exists until cleanup")
    }

    #[allow(clippy::too_many_lines)] // Startup and signal handling deliberately share one ordered lifecycle.
    async fn run_until_signal(
        &mut self,
        config: &RunConfig,
        recovered_from_crash: bool,
    ) -> Result<(), FailureStage> {
        let health = self
            .database()
            .health()
            .await
            .map_err(|_| FailureStage::DatabaseHealth)?;
        self.schema_version = Some(health.schema_version);

        #[cfg(not(feature = "process-test-hooks"))]
        let credential_store: Arc<dyn CredentialStore> = Arc::new(MacOSKeychainStore);
        #[cfg(feature = "process-test-hooks")]
        let credential_store: Arc<dyn CredentialStore> = Arc::new(ProcessTestCredentialStore);
        let (bridge_status_sender, mut bridge_status_receiver) =
            watch::channel(BridgeStatus::Disconnected);
        let (pairing_state_sender, mut pairing_state_receiver) = watch::channel(false);
        let communication_authorization = CommunicationAuthorization::new();
        let (control, control_commands) = CloudControlOwner::start(
            Arc::clone(
                self.database
                    .as_ref()
                    .expect("database exists until cleanup"),
            ),
            pairing_state_sender.clone(),
            communication_authorization.clone(),
        );
        // A build with process hooks has no production Keychain identity; keeping those harnesses
        // explicitly unpaired prevents test-only identities from becoming a release input.
        let pairing_valid = if cfg!(feature = "process-test-hooks") {
            false
        } else {
            start_paired_control(Arc::clone(&credential_store), &control_commands)
                .await
                .ok()
                .unwrap_or(false)
        };
        self.control = Some(control);
        let (bridge_shutdown_sender, bridge_shutdown_receiver) = watch::channel(false);
        let bridge_task = start_bridge(
            config,
            Arc::clone(&credential_store),
            bridge_status_sender.clone(),
            bridge_shutdown_receiver,
        );
        #[cfg(feature = "process-test-hooks")]
        if bridge_task.is_err() && config.process_test_fatal_cleanup.is_some() {
            return Err(FailureStage::BridgeConfiguration);
        }
        self.bridge_task = bridge_task.ok();
        self.bridge_shutdown = Some(bridge_shutdown_sender);
        let (pairing_shutdown_sender, pairing_shutdown_receiver) = watch::channel(false);
        self.pairing_task = Some(
            start_pairing_server(
                config,
                Arc::clone(
                    self.database
                        .as_ref()
                        .expect("database exists until cleanup"),
                ),
                credential_store,
                control_commands,
                pairing_shutdown_receiver,
            )
            .await?,
        );
        self.pairing_shutdown = Some(pairing_shutdown_sender);

        transition_startup_agent_state(&mut self.state, pairing_valid)
            .map_err(|_| FailureStage::State)?;
        if self.bridge_task.is_none() {
            set_bridge_status(&mut self.state, BridgeStatus::Degraded)
                .map_err(|_| FailureStage::State)?;
            bridge_status_sender.send_replace(BridgeStatus::Degraded);
        }

        self.lifecycle = Some(LifecycleRuntime::start(
            Arc::clone(
                self.database
                    .as_ref()
                    .expect("database exists until cleanup"),
            ),
            RuntimeIdentity::new("local-unpaired", "local-device"),
            LIFECYCLE_CAPACITY,
            Arc::new(NoopCapabilityRefresher),
        ));
        let lifecycle = self
            .lifecycle
            .as_ref()
            .expect("lifecycle was just installed");
        if recovered_from_crash {
            lifecycle
                .record_crash_recovery()
                .await
                .map_err(|_| FailureStage::Lifecycle)?;
        }
        lifecycle
            .record_startup()
            .await
            .map_err(|_| FailureStage::Lifecycle)?;

        self.start_system_collector(config, pairing_valid).await?;
        self.communication_runtime = Some(
            CommunicationRuntime::start_authorized(
                Arc::clone(
                    self.database
                        .as_ref()
                        .expect("database exists until cleanup"),
                ),
                config.paths.database_file.clone(),
                Arc::new(UnavailableCommunicationProviderFactory),
                communication_authorization.clone(),
            )
            .await
            .map_err(|_| FailureStage::CommunicationCollector)?,
        );

        #[cfg(feature = "process-test-hooks")]
        if let Some(hook) = &config.process_test_fatal_cleanup {
            await_fatal_cleanup_release(hook, &mut bridge_status_receiver).await?;
            return Err(FailureStage::InjectedHeartbeat);
        }

        self.heartbeat = Some(LocalHeartbeatWriter::new(&config.paths.status_file));
        self.persist_status()
            .await
            .map_err(|_| FailureStage::Heartbeat)?;

        let mut heartbeat_timer = tokio::time::interval(HEARTBEAT_INTERVAL);
        heartbeat_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        heartbeat_timer.tick().await;
        let mut interrupt =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .map_err(|_| FailureStage::Signal)?;
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .map_err(|_| FailureStage::Signal)?;

        loop {
            tokio::select! {
                _ = interrupt.recv() => return Ok(()),
                _ = terminate.recv() => return Ok(()),
                _ = heartbeat_timer.tick() => {
                    self.persist_status().await.map_err(|_| FailureStage::Heartbeat)?;
                }
                changed = bridge_status_receiver.changed() => {
                    if changed.is_ok() {
                        let next = *bridge_status_receiver.borrow_and_update();
                        set_bridge_status(&mut self.state, next)
                            .map_err(|_| FailureStage::State)?;
                        self.persist_status().await.map_err(|_| FailureStage::Heartbeat)?;
                    }
                }
                changed = pairing_state_receiver.changed() => {
                    if changed.is_ok() {
                        let paired = *pairing_state_receiver.borrow_and_update();
                        transition_pairing_runtime_state(
                            &mut self.state,
                            paired,
                        )
                        .map_err(|_| FailureStage::State)?;
                        self.restart_system_collector(config, paired).await?;
                        self.persist_status().await.map_err(|_| FailureStage::Heartbeat)?;
                    }
                }
            }
        }
    }

    async fn start_system_collector(
        &mut self,
        config: &RunConfig,
        paired: bool,
    ) -> Result<(), FailureStage> {
        let identity = if paired {
            collector_identity_from_pairing_state(
                self.database()
                    .load_pairing_state()
                    .await
                    .map_err(|_| FailureStage::SystemCollector)?
                    .as_ref(),
            )
        } else {
            None
        };
        self.system_runtime = Some(
            SystemRuntimeHandle::start(
                Arc::clone(
                    self.database
                        .as_ref()
                        .expect("database exists until cleanup"),
                ),
                identity.or_else(|| config.collector_identity()),
                config.paths.data_dir.clone(),
            )
            .await
            .map_err(|_| FailureStage::SystemCollector)?,
        );
        Ok(())
    }

    async fn restart_system_collector(
        &mut self,
        config: &RunConfig,
        paired: bool,
    ) -> Result<(), FailureStage> {
        if let Some(runtime) = self.system_runtime.take() {
            runtime
                .shutdown()
                .await
                .map_err(|_| FailureStage::SystemCollectorCleanup)?;
        }
        self.start_system_collector(config, paired).await
    }

    async fn persist_status(&self) -> Result<(), FailureStage> {
        let heartbeat = self.heartbeat.as_ref().ok_or(FailureStage::Heartbeat)?;
        let schema_version = self.schema_version.ok_or(FailureStage::Heartbeat)?;
        persist_runtime_status(self.database(), heartbeat, self.state, schema_version)
            .await
            .map_err(|_| FailureStage::Heartbeat)
    }

    async fn cleanup(mut self, clean_shutdown: bool) -> Vec<FailureStage> {
        let mut failures = Vec::new();

        if let Some(communication_runtime) = self.communication_runtime.take() {
            if communication_runtime.shutdown().await.is_err() {
                failures.push(FailureStage::CommunicationCollectorCleanup);
            }
        }

        if let Some(system_runtime) = self.system_runtime.take() {
            if system_runtime.shutdown().await.is_err() {
                failures.push(FailureStage::SystemCollectorCleanup);
            }
        }

        if let Some(lifecycle) = self.lifecycle.take() {
            let result = if clean_shutdown {
                lifecycle.stop_and_drain().await.map(|_| ())
            } else {
                lifecycle.abort_and_drain().await
            };
            if result.is_err() {
                failures.push(FailureStage::LifecycleCleanup);
            }
        }

        if let Some(shutdown) = self.bridge_shutdown.take() {
            shutdown.send_replace(true);
        }
        if stop_bridge(self.bridge_task.take()).await.is_err() {
            failures.push(FailureStage::BridgeCleanup);
        }

        if let Some(shutdown) = self.pairing_shutdown.take() {
            shutdown.send_replace(true);
        }
        if stop_pairing_server(self.pairing_task.take()).await.is_err() {
            failures.push(FailureStage::PairingCleanup);
        }

        if let Some(control) = self.control.take() {
            if control.shutdown().await.is_err() {
                failures.push(FailureStage::ControlCleanup);
            }
        }

        if self.database().checkpoint().await.is_err() {
            failures.push(FailureStage::Checkpoint);
        }

        if clean_shutdown {
            let state_result = self
                .state
                .transition_agent(AgentStatus::Stopped)
                .and_then(|()| set_bridge_status(&mut self.state, BridgeStatus::Stopped));
            if state_result.is_err() {
                failures.push(FailureStage::State);
            } else if self.persist_status().await.is_err() {
                failures.push(FailureStage::FinalStatus);
            }
        }

        // The heartbeat interval is local to `run_until_signal`; it has already been dropped, so
        // no writer can race this final status. Dropping the writer releases the remaining handle.
        self.heartbeat.take();
        let database = self.database.take().expect("database exists until cleanup");
        match Arc::try_unwrap(database) {
            Ok(database) => {
                if database.shutdown().await.is_err() {
                    failures.push(FailureStage::DatabaseShutdown);
                }
            }
            Err(_) => failures.push(FailureStage::DatabaseOwnership),
        }
        failures
    }
}

async fn run(config: &RunConfig) -> Result<(), AppError> {
    config
        .paths
        .create_securely()
        .map_err(|_| AppError::primary(FailureStage::Paths))?;
    let _instance = match SingleInstanceGuard::acquire(&config.paths.lock_file) {
        Ok(guard) => guard,
        Err(RuntimeError::AlreadyRunning) => return Err(AppError::AlreadyRunning),
        Err(_) => return Err(AppError::primary(FailureStage::Paths)),
    };

    eprintln!("pca-agentd: starting local runtime");
    let crash_marker = CrashMarkerGuard::activate(&config.paths.crash_marker_file)
        .map_err(|_| AppError::primary(FailureStage::CrashMarker))?;
    let recovered_from_crash = crash_marker.previous_exit_was_unclean();
    let database = match open_database(config).await {
        Ok(database) => database,
        Err(stage) => {
            std::mem::forget(crash_marker);
            return Err(AppError::primary(stage));
        }
    };

    let mut resources = RuntimeResources::new(database);
    let primary_result = resources
        .run_until_signal(config, recovered_from_crash)
        .await;
    let clean_shutdown = primary_result.is_ok();
    let cleanup = resources.cleanup(clean_shutdown).await;

    #[cfg(feature = "process-test-hooks")]
    let cleanup = {
        let mut cleanup = cleanup;
        if cleanup.is_empty() {
            if let Some(hook) = &config.process_test_fatal_cleanup {
                if write_process_test_cleanup_complete(&hook.cleanup_complete).is_err() {
                    cleanup.push(FailureStage::CleanupEvidence);
                }
            }
        }
        cleanup
    };

    if clean_shutdown && cleanup.is_empty() {
        crash_marker
            .complete_cleanly()
            .map_err(|_| AppError::primary(FailureStage::CrashMarker))?;
        eprintln!("pca-agentd: stopped local runtime");
        Ok(())
    } else {
        std::mem::forget(crash_marker);
        Err(AppError::Failure(FailureReport {
            primary: primary_result
                .err()
                .unwrap_or(FailureStage::LifecycleCleanup),
            cleanup,
        }))
    }
}

async fn open_database(config: &RunConfig) -> Result<DbActorHandle, FailureStage> {
    #[cfg(feature = "process-test-hooks")]
    if config.process_test_barrier.is_some() || config.process_test_collector_barrier.is_some() {
        let hooks = match (
            config.process_test_barrier.as_ref(),
            config.process_test_collector_barrier.as_ref(),
        ) {
            (Some(event), Some(collector)) => {
                ProcessTestHooks::new(event.ready.clone(), event.release.clone()).and_then(
                    |hooks| {
                        hooks.with_collector_commit_barrier(
                            collector.ready.clone(),
                            collector.release.clone(),
                        )
                    },
                )
            }
            (Some(event), None) => {
                ProcessTestHooks::new(event.ready.clone(), event.release.clone())
            }
            (None, Some(collector)) => ProcessTestHooks::collector_commit(
                collector.ready.clone(),
                collector.release.clone(),
            ),
            (None, None) => unreachable!("barrier presence checked"),
        }
        .map_err(|_| FailureStage::DatabaseOpen)?;
        return DbActorHandle::open_with_process_test_hooks(
            &config.paths.database_file,
            app_version(),
            hooks,
        )
        .await
        .map_err(|_| FailureStage::DatabaseOpen);
    }
    DbActorHandle::open(&config.paths.database_file, app_version())
        .await
        .map_err(|_| FailureStage::DatabaseOpen)
}

fn start_bridge(
    config: &RunConfig,
    credential_store: Arc<dyn CredentialStore>,
    statuses: watch::Sender<BridgeStatus>,
    shutdown: watch::Receiver<bool>,
) -> Result<JoinHandle<Result<(), BridgeSupervisorError>>, ()> {
    let bridge_config = BridgeSupervisorConfig::new(
        &config.bridge_executable,
        &config.paths.socket_file,
        app_version(),
    )
    .map_err(|_| ())?;
    #[cfg(feature = "process-test-hooks")]
    let bridge_config = if config.process_test_fatal_cleanup.is_some() {
        bridge_config.with_operation_timeout(Duration::from_secs(10))
    } else {
        bridge_config
    };
    let supervisor = BridgeSupervisor::new(bridge_config, credential_store, statuses);
    Ok(tokio::spawn(supervisor.run(shutdown)))
}

#[cfg(feature = "process-test-hooks")]
struct ProcessTestCredentialStore;

#[cfg(feature = "process-test-hooks")]
impl CredentialStore for ProcessTestCredentialStore {
    fn load(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, CredentialError> {
        if service != BRIDGE_CREDENTIAL_SERVICE || account != BRIDGE_CREDENTIAL_ACCOUNT {
            return Err(CredentialError::UnsupportedIdentity);
        }
        Ok(Some(vec![0x5a; BRIDGE_SHARED_SECRET_LENGTH]))
    }

    fn store(&self, _: &str, _: &str, _: &[u8]) -> Result<(), CredentialError> {
        Err(CredentialError::OperationFailed)
    }

    fn delete(&self, _: &str, _: &str) -> Result<(), CredentialError> {
        Err(CredentialError::OperationFailed)
    }
}

async fn stop_bridge(
    bridge_task: Option<JoinHandle<Result<(), BridgeSupervisorError>>>,
) -> Result<(), FailureStage> {
    if let Some(task) = bridge_task {
        // Never abort a supervisor that owns a child: it must finish its kill-and-wait reap path.
        task.await
            .map_err(|_| FailureStage::BridgeCleanup)?
            .map_err(|_| FailureStage::BridgeCleanup)?;
    }
    Ok(())
}

async fn start_pairing_server(
    config: &RunConfig,
    database: Arc<DbActorHandle>,
    credential_store: Arc<dyn CredentialStore>,
    control_commands: CloudControlCommands,
    shutdown: watch::Receiver<bool>,
) -> Result<JoinHandle<Result<(), PairingIpcServerError>>, FailureStage> {
    let socket = PairingSocket::bind(&config.paths.pairing_socket_file)
        .await
        .map_err(|_| FailureStage::PairingConfiguration)?;
    let server = PairingIpcServer::new(socket, database, credential_store, control_commands);
    Ok(tokio::spawn(server.serve(shutdown)))
}

async fn start_paired_control(
    credential_store: Arc<dyn CredentialStore>,
    control_commands: &CloudControlCommands,
) -> Result<bool, FailureStage> {
    let origin =
        Url::parse(PRODUCTION_CLOUD_API_ORIGIN).map_err(|_| FailureStage::ControlConfiguration)?;
    let client = HttpControlClient::new(origin).map_err(|_| FailureStage::ControlConfiguration)?;
    control_commands
        .replace_from_keychain(credential_store, Arc::new(client) as Arc<dyn ControlClient>)
        .await
        .map_err(|_| FailureStage::ControlConfiguration)
}

fn collector_identity_from_pairing_state(
    state: Option<&PairingState>,
) -> Option<CollectorIdentity> {
    let state = state?;
    let workspace_id = Uuid::parse_str(&state.workspace_id).ok()?;
    let device_id = Uuid::parse_str(&state.device_id).ok()?;
    if workspace_id.is_nil() || device_id.is_nil() {
        return None;
    }
    Some(CollectorIdentity {
        workspace_id,
        device_id,
    })
}

fn transition_startup_agent_state(
    state: &mut RuntimeStateMachine,
    pairing_valid: bool,
) -> Result<(), RuntimeError> {
    state.transition_agent(AgentStatus::Unpaired)?;
    if pairing_valid {
        state.transition_agent(AgentStatus::Running)?;
    }
    Ok(())
}

fn transition_pairing_runtime_state(
    state: &mut RuntimeStateMachine,
    paired: bool,
) -> Result<(), RuntimeError> {
    match (state.agent_status(), paired) {
        (AgentStatus::Unpaired | AgentStatus::Degraded, true) => {
            state.transition_agent(AgentStatus::Running)
        }
        (AgentStatus::Unpaired, false) | (AgentStatus::Running, true) => Ok(()),
        (AgentStatus::Running | AgentStatus::WaitingPermission | AgentStatus::Degraded, false) => {
            state.transition_agent(AgentStatus::Unpaired)
        }
        _ => Err(RuntimeError::IllegalAgentTransition {
            from: state.agent_status(),
            to: if paired {
                AgentStatus::Running
            } else {
                AgentStatus::Unpaired
            },
        }),
    }
}

async fn stop_pairing_server(
    pairing_task: Option<JoinHandle<Result<(), PairingIpcServerError>>>,
) -> Result<(), FailureStage> {
    if let Some(task) = pairing_task {
        task.await
            .map_err(|_| FailureStage::PairingCleanup)?
            .map_err(|_| FailureStage::PairingCleanup)?;
    }
    Ok(())
}

async fn persist_runtime_status(
    database: &DbActorHandle,
    heartbeat: &LocalHeartbeatWriter,
    state: RuntimeStateMachine,
    schema_version: u32,
) -> Result<(), FailureStage> {
    let now = OffsetDateTime::now_utc();
    let heartbeat_at = now.format(&Rfc3339).map_err(|_| FailureStage::Heartbeat)?;
    let updated_at_ms = i64::try_from(now.unix_timestamp_nanos() / 1_000_000)
        .map_err(|_| FailureStage::Heartbeat)?;
    database
        .set_agent_state(
            state.agent_status(),
            state.bridge_status(),
            true,
            updated_at_ms,
        )
        .await
        .map_err(|_| FailureStage::Heartbeat)?;
    heartbeat
        .write(&RuntimeStatusEnvelope {
            agent_status: state.agent_status(),
            bridge_status: state.bridge_status(),
            local_healthy: true,
            heartbeat_at,
            process_id: std::process::id(),
            app_version: app_version().to_owned(),
            schema_version,
        })
        .map_err(|_| FailureStage::Heartbeat)
}

fn app_version() -> &'static str {
    option_env!("PCA_APP_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

fn set_bridge_status(
    state: &mut RuntimeStateMachine,
    next: BridgeStatus,
) -> Result<(), RuntimeError> {
    if state.bridge_status() == next {
        return Ok(());
    }
    match next {
        BridgeStatus::Disconnected | BridgeStatus::Stopped => state.transition_bridge(next),
        BridgeStatus::Handshaking => {
            if state.bridge_status() == BridgeStatus::Ready {
                state.transition_bridge(BridgeStatus::Degraded)?;
            }
            state.transition_bridge(BridgeStatus::Handshaking)
        }
        BridgeStatus::Ready => {
            set_bridge_status(state, BridgeStatus::Handshaking)?;
            state.transition_bridge(BridgeStatus::Ready)
        }
        BridgeStatus::Degraded => {
            if matches!(
                state.bridge_status(),
                BridgeStatus::Disconnected | BridgeStatus::Incompatible
            ) {
                set_bridge_status(state, BridgeStatus::Handshaking)?;
            }
            state.transition_bridge(BridgeStatus::Degraded)
        }
        BridgeStatus::Incompatible => {
            set_bridge_status(state, BridgeStatus::Handshaking)?;
            state.transition_bridge(BridgeStatus::Incompatible)
        }
    }
}

fn health(paths: &RuntimePaths) -> u8 {
    let Ok(bytes) = fs::read(&paths.status_file) else {
        eprintln!("pca-agentd: local health unavailable");
        return EXIT_UNHEALTHY;
    };
    let Ok(status) = serde_json::from_slice::<RuntimeStatusEnvelope>(&bytes) else {
        eprintln!("pca-agentd: local health invalid");
        return EXIT_UNHEALTHY;
    };
    let Ok(heartbeat_at) = OffsetDateTime::parse(&status.heartbeat_at, &Rfc3339) else {
        eprintln!("pca-agentd: local health invalid");
        return EXIT_UNHEALTHY;
    };
    let age = OffsetDateTime::now_utc() - heartbeat_at;
    let active = matches!(
        status.agent_status,
        AgentStatus::Unpaired
            | AgentStatus::WaitingPermission
            | AgentStatus::Running
            | AgentStatus::Degraded
    );
    if !status.local_healthy || !active || age < TimeDuration::ZERO || age > HEALTH_FRESHNESS {
        eprintln!("pca-agentd: local health stale or unhealthy");
        return EXIT_UNHEALTHY;
    }
    match serde_json::to_string(&status) {
        Ok(json) => {
            println!("{json}");
            0
        }
        Err(_) => EXIT_UNHEALTHY,
    }
}

#[cfg(feature = "process-test-hooks")]
async fn await_fatal_cleanup_release(
    hook: &ProcessTestFatalCleanupConfig,
    statuses: &mut watch::Receiver<BridgeStatus>,
) -> Result<(), FailureStage> {
    wait_for_bridge_spawn(statuses)
        .await
        .map_err(|()| FailureStage::InjectedHeartbeat)?;
    wait_for_process_test_file(&hook.bridge_pid_ready)
        .await
        .map_err(|()| FailureStage::InjectedHeartbeat)?;
    write_process_test_file(&hook.armed, b"armed\n")
        .map_err(|()| FailureStage::InjectedHeartbeat)?;
    wait_for_process_test_file(&hook.release)
        .await
        .map_err(|()| FailureStage::InjectedHeartbeat)
}

#[cfg(feature = "process-test-hooks")]
async fn wait_for_bridge_spawn(statuses: &mut watch::Receiver<BridgeStatus>) -> Result<(), ()> {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if *statuses.borrow_and_update() == BridgeStatus::Handshaking {
                return Ok(());
            }
            statuses.changed().await.map_err(|_| ())?;
        }
    })
    .await
    .map_err(|_| ())?
}

#[cfg(feature = "process-test-hooks")]
async fn wait_for_process_test_file(path: &Path) -> Result<(), ()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                return Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) | Err(_) => return Err(()),
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[cfg(feature = "process-test-hooks")]
fn write_process_test_cleanup_complete(path: &Path) -> Result<(), ()> {
    write_process_test_file(path, b"bridge-reaped-db-shutdown\n")
}

#[cfg(feature = "process-test-hooks")]
fn write_process_test_file(path: &Path, contents: &[u8]) -> Result<(), ()> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| ())?;
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector_registry::CollectorIdentity;
    use pca_db_local::PairingState;
    use uuid::Uuid;

    #[test]
    fn paired_startup_transitions_through_unpaired_before_running() {
        let mut state = RuntimeStateMachine::starting();

        transition_startup_agent_state(&mut state, true).expect("paired startup is legal");

        assert_eq!(state.agent_status(), AgentStatus::Running);
    }

    #[test]
    fn pairing_runtime_transition_marks_a_running_agent_unpaired_after_revocation() {
        let mut state = RuntimeStateMachine::starting();
        transition_startup_agent_state(&mut state, true).expect("paired startup is legal");

        transition_pairing_runtime_state(&mut state, false)
            .expect("revocation transitions the runtime to unpaired");

        assert_eq!(state.agent_status(), AgentStatus::Unpaired);
    }

    #[test]
    fn system_collector_identity_comes_only_from_a_valid_paired_device() {
        let paired = PairingState::paired(
            "01982222-7222-8222-8222-222222222222",
            "01983333-7333-8333-8333-333333333333",
            "keychain://pca/device/current",
            1,
            PRODUCTION_CLOUD_API_ORIGIN,
        );

        assert_eq!(
            collector_identity_from_pairing_state(Some(&paired)),
            Some(CollectorIdentity {
                workspace_id: Uuid::parse_str("01983333-7333-8333-8333-333333333333")
                    .expect("workspace UUID"),
                device_id: Uuid::parse_str("01982222-7222-8222-8222-222222222222")
                    .expect("device UUID"),
            }),
        );

        let invalid = PairingState {
            device_id: Uuid::nil().to_string(),
            ..paired
        };
        assert_eq!(collector_identity_from_pairing_state(Some(&invalid)), None);
        assert_eq!(collector_identity_from_pairing_state(None), None);
    }
}
