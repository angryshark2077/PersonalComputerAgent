use std::{fs, sync::Arc, time::Duration};

use pca_agent_runtime::{
    CrashMarkerGuard, LocalHeartbeatWriter, RuntimeError, RuntimePaths, RuntimeStateMachine,
    SingleInstanceGuard,
};
use pca_bridge_client::supervisor::{BridgeSupervisor, BridgeSupervisorConfig};
use pca_db_local::DbActorHandle;
#[cfg(feature = "process-test-hooks")]
use pca_db_local::ProcessTestHooks;
use pca_domain::{AgentStatus, BridgeStatus, RuntimeStatusEnvelope};
use pca_keychain::MacOSKeychainStore;
use time::{format_description::well_known::Rfc3339, Duration as TimeDuration, OffsetDateTime};
use tokio::{sync::watch, task::JoinHandle};

use crate::{
    config::{CommandConfig, RunConfig},
    lifecycle::{LifecycleRuntime, RuntimeIdentity},
};

pub(crate) const EXIT_USAGE: i32 = 2;
const EXIT_UNHEALTHY: i32 = 1;
const EXIT_UNSUPPORTED: i32 = 3;
const EXIT_ALREADY_RUNNING: i32 = 4;
const EXIT_RUNTIME_FAILURE: i32 = 5;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const HEALTH_FRESHNESS: TimeDuration = TimeDuration::seconds(5);
const LIFECYCLE_CAPACITY: usize = 32;

pub(crate) async fn execute(command: CommandConfig) -> i32 {
    match command {
        CommandConfig::Run(config) => match run(config).await {
            Ok(()) => 0,
            Err(AppError::AlreadyRunning) => {
                eprintln!("pca-agentd: already running");
                EXIT_ALREADY_RUNNING
            }
            Err(_) => {
                eprintln!("pca-agentd: runtime failure");
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

#[derive(Debug)]
enum AppError {
    AlreadyRunning,
    Runtime,
}

impl<T> From<T> for AppError
where
    T: std::error::Error,
{
    fn from(_: T) -> Self {
        Self::Runtime
    }
}

async fn run(config: RunConfig) -> Result<(), AppError> {
    config.paths.create_securely()?;
    let _instance = match SingleInstanceGuard::acquire(&config.paths.lock_file) {
        Ok(guard) => guard,
        Err(RuntimeError::AlreadyRunning) => return Err(AppError::AlreadyRunning),
        Err(error) => return Err(error.into()),
    };

    eprintln!("pca-agentd: starting local runtime");
    let crash_marker = CrashMarkerGuard::activate(&config.paths.crash_marker_file)?;
    let recovered_from_crash = crash_marker.previous_exit_was_unclean();

    let active_result = run_active(&config, recovered_from_crash).await;
    let database = match active_result {
        Ok(database) => database,
        Err(error) => {
            std::mem::forget(crash_marker);
            return Err(error);
        }
    };
    crash_marker.complete_cleanly()?;
    database.shutdown().await?;
    eprintln!("pca-agentd: stopped local runtime");
    Ok(())
}

async fn run_active(
    config: &RunConfig,
    recovered_from_crash: bool,
) -> Result<DbActorHandle, AppError> {
    #[cfg(feature = "process-test-hooks")]
    let database = if let Some(ref barrier) = config.process_test_barrier {
        let hooks = ProcessTestHooks::new(barrier.ready.clone(), barrier.release.clone())?;
        DbActorHandle::open_with_process_test_hooks(
            &config.paths.database_file,
            env!("CARGO_PKG_VERSION"),
            hooks,
        )
        .await?
    } else {
        DbActorHandle::open(&config.paths.database_file, env!("CARGO_PKG_VERSION")).await?
    };
    #[cfg(not(feature = "process-test-hooks"))]
    let database =
        DbActorHandle::open(&config.paths.database_file, env!("CARGO_PKG_VERSION")).await?;
    let database_health = database.health().await?;
    let database = Arc::new(database);

    let credential_store = Arc::new(MacOSKeychainStore);
    let (bridge_status_sender, mut bridge_status_receiver) =
        watch::channel(BridgeStatus::Disconnected);
    let (bridge_shutdown_sender, bridge_shutdown_receiver) = watch::channel(false);
    let bridge_task = start_bridge(
        config,
        credential_store,
        bridge_status_sender.clone(),
        bridge_shutdown_receiver,
    );

    let mut state = RuntimeStateMachine::starting();
    state.transition_agent(AgentStatus::Unpaired)?;
    if bridge_task.is_none() {
        set_bridge_status(&mut state, BridgeStatus::Degraded)?;
        bridge_status_sender.send_replace(BridgeStatus::Degraded);
    }

    let lifecycle = LifecycleRuntime::start(
        Arc::clone(&database),
        RuntimeIdentity::new("local-unpaired", "local-device"),
        LIFECYCLE_CAPACITY,
    );
    if recovered_from_crash {
        lifecycle.record_crash_recovery().await?;
    }
    lifecycle.record_startup().await?;

    let heartbeat = LocalHeartbeatWriter::new(&config.paths.status_file);
    persist_runtime_status(&database, &heartbeat, state, database_health.schema_version).await?;

    let mut heartbeat_timer = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    heartbeat_timer.tick().await;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    loop {
        tokio::select! {
            _ = interrupt.recv() => break,
            _ = terminate.recv() => break,
            _ = heartbeat_timer.tick() => {
                persist_runtime_status(
                    &database,
                    &heartbeat,
                    state,
                    database_health.schema_version,
                ).await?;
            }
            changed = bridge_status_receiver.changed() => {
                if changed.is_ok() {
                    let next = *bridge_status_receiver.borrow_and_update();
                    set_bridge_status(&mut state, next)?;
                    persist_runtime_status(
                        &database,
                        &heartbeat,
                        state,
                        database_health.schema_version,
                    ).await?;
                }
            }
        }
    }

    lifecycle.stop_and_drain().await?;
    bridge_shutdown_sender.send_replace(true);
    stop_bridge(bridge_task).await?;
    database.checkpoint().await?;
    state.transition_agent(AgentStatus::Stopped)?;
    set_bridge_status(&mut state, BridgeStatus::Stopped)?;
    persist_runtime_status(&database, &heartbeat, state, database_health.schema_version).await?;
    let database = Arc::try_unwrap(database).map_err(|_| AppError::Runtime)?;
    Ok(database)
}

fn start_bridge(
    config: &RunConfig,
    credential_store: Arc<MacOSKeychainStore>,
    statuses: watch::Sender<BridgeStatus>,
    shutdown: watch::Receiver<bool>,
) -> Option<JoinHandle<Result<(), pca_bridge_client::supervisor::BridgeSupervisorError>>> {
    let bridge_config = BridgeSupervisorConfig::new(
        &config.bridge_executable,
        &config.paths.socket_file,
        env!("CARGO_PKG_VERSION"),
    )
    .ok()?;
    let supervisor = BridgeSupervisor::new(bridge_config, credential_store, statuses);
    Some(tokio::spawn(supervisor.run(shutdown)))
}

async fn stop_bridge(
    bridge_task: Option<
        JoinHandle<Result<(), pca_bridge_client::supervisor::BridgeSupervisorError>>,
    >,
) -> Result<(), AppError> {
    if let Some(mut task) = bridge_task {
        if let Ok(result) = tokio::time::timeout(Duration::from_secs(3), &mut task).await {
            result.map_err(|_| AppError::Runtime)??;
        } else {
            task.abort();
            let _ = task.await;
            return Err(AppError::Runtime);
        }
    }
    Ok(())
}

async fn persist_runtime_status(
    database: &DbActorHandle,
    heartbeat: &LocalHeartbeatWriter,
    state: RuntimeStateMachine,
    schema_version: u32,
) -> Result<(), AppError> {
    let now = OffsetDateTime::now_utc();
    let heartbeat_at = now.format(&Rfc3339).map_err(|_| AppError::Runtime)?;
    let updated_at_ms =
        i64::try_from(now.unix_timestamp_nanos() / 1_000_000).map_err(|_| AppError::Runtime)?;
    database
        .set_agent_state(
            state.agent_status(),
            state.bridge_status(),
            true,
            updated_at_ms,
        )
        .await?;
    heartbeat.write(&RuntimeStatusEnvelope {
        agent_status: state.agent_status(),
        bridge_status: state.bridge_status(),
        local_healthy: true,
        heartbeat_at,
        process_id: std::process::id(),
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
        schema_version,
    })?;
    Ok(())
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

fn health(paths: &RuntimePaths) -> i32 {
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
