use crate::{
    collector_registry::{CollectorIdentity, CollectorRegistry, RegistryUpdate},
    event_factory::{health_event, metric_event, status_event},
    event_sink::DbEventSink,
};
use ::time::OffsetDateTime;
use pca_db_local::{DbActorHandle, DbError};
use pca_domain::{CollectorState, DomainError, EventCommit, EventSink, SystemMetricSample};
use pca_system_collector::{
    start_sampler, start_system_collector, SysinfoMetricsSource, SystemCollectorHandle,
    SystemMetricsSource, SystemObservation, SystemSampleError, RETRY_DELAYS,
};
use std::{
    fmt,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::watch,
    task::JoinHandle,
    time::{self, MissedTickBehavior},
};
use uuid::Uuid;

const OBSERVATION_CAPACITY: usize = 8;
const OUTBOX_MONITOR_INTERVAL: Duration = Duration::from_secs(30);
const DATABASE_DEADLINE: Duration = Duration::from_secs(5);

pub(crate) struct SystemRuntimeHandle {
    shutdown: Option<watch::Sender<bool>>,
    worker: Option<JoinHandle<Result<(), SystemRuntimeError>>>,
}

#[derive(Debug)]
pub(crate) enum SystemRuntimeError {
    Database(DbError),
    Domain(DomainError),
    Collector(SystemSampleError),
    Clock,
    WorkerStopped,
}

impl fmt::Display for SystemRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "system runtime database: {error}"),
            Self::Domain(error) => write!(formatter, "system runtime domain: {error}"),
            Self::Collector(error) => write!(formatter, "system runtime collector: {error}"),
            Self::Clock => formatter.write_str("system runtime clock unavailable"),
            Self::WorkerStopped => formatter.write_str("system runtime worker stopped"),
        }
    }
}

impl std::error::Error for SystemRuntimeError {}

impl SystemRuntimeHandle {
    pub(crate) async fn start(
        database: Arc<DbActorHandle>,
        identity: Option<CollectorIdentity>,
        data_dir: PathBuf,
    ) -> Result<Self, SystemRuntimeError> {
        let Some(identity) = identity else {
            return start_disabled(database).await;
        };
        let source = SysinfoMetricsSource::new(data_dir).map_err(SystemRuntimeError::Collector)?;
        let sink = Arc::new(DbEventSink::new(Arc::clone(&database)));
        Self::start_with_source_and_sink(database, Some(identity), source, sink).await
    }

    #[cfg(test)]
    pub(crate) async fn start_with_source<S: SystemMetricsSource>(
        database: Arc<DbActorHandle>,
        identity: Option<CollectorIdentity>,
        source: S,
    ) -> Result<Self, SystemRuntimeError> {
        let sink = Arc::new(DbEventSink::new(Arc::clone(&database)));
        Self::start_with_source_and_sink(database, identity, source, sink).await
    }

    async fn start_with_source_and_sink<S, E>(
        database: Arc<DbActorHandle>,
        identity: Option<CollectorIdentity>,
        source: S,
        sink: Arc<E>,
    ) -> Result<Self, SystemRuntimeError>
    where
        S: SystemMetricsSource,
        E: EventSink + 'static,
    {
        if identity.is_none() {
            drop(source);
            return start_disabled(database).await;
        }
        start_paired(
            database,
            identity.expect("identity was checked"),
            source,
            sink,
        )
        .await
    }

    pub(crate) async fn shutdown(mut self) -> Result<(), SystemRuntimeError> {
        if let Some(shutdown) = self.shutdown.take() {
            shutdown.send_replace(true);
        }
        match self.worker.take() {
            Some(worker) => worker
                .await
                .map_err(|_| SystemRuntimeError::WorkerStopped)?,
            None => Ok(()),
        }
    }
}

async fn start_disabled(
    database: Arc<DbActorHandle>,
) -> Result<SystemRuntimeHandle, SystemRuntimeError> {
    let prior = load_system_state(&database).await?;
    let now_ms = clock_now_ms()?;
    let (_, update) = CollectorRegistry::restore(prior, false, 0, now_ms);
    database
        .upsert_collector_state(&update.state)
        .await
        .map_err(SystemRuntimeError::Database)?;
    Ok(SystemRuntimeHandle {
        shutdown: None,
        worker: None,
    })
}

async fn start_paired<S, E>(
    database: Arc<DbActorHandle>,
    identity: CollectorIdentity,
    source: S,
    sink: Arc<E>,
) -> Result<SystemRuntimeHandle, SystemRuntimeError>
where
    S: SystemMetricsSource,
    E: EventSink + 'static,
{
    if identity.workspace_id.is_nil() || identity.device_id.is_nil() {
        return Err(SystemRuntimeError::Domain(DomainError::new(
            "COLLECTOR_DEGRADED",
            "collector identity must use non-nil UUIDs",
            false,
        )));
    }
    let prior = load_system_state(&database).await?;
    let outbox_depth = database
        .active_outbox_depth()
        .await
        .map_err(SystemRuntimeError::Database)?;
    let now_ms = clock_now_ms()?;
    let (registry, initial) = CollectorRegistry::restore(prior, true, outbox_depth, now_ms);
    let pending = pending_from_update(&identity, &initial, None, now_ms)?;
    let sink: Arc<dyn EventSink> = sink;
    let (shutdown, shutdown_receiver) = watch::channel(false);
    let worker = tokio::spawn(run_paired(
        database,
        identity,
        source,
        sink,
        registry,
        pending,
        shutdown_receiver,
    ));
    Ok(SystemRuntimeHandle {
        shutdown: Some(shutdown),
        worker: Some(worker),
    })
}

async fn load_system_state(
    database: &DbActorHandle,
) -> Result<Option<CollectorState>, SystemRuntimeError> {
    database
        .load_collector_states()
        .await
        .map_err(SystemRuntimeError::Database)
        .map(|states| {
            states
                .into_iter()
                .find(|state| state.collector_key == "system")
        })
}

async fn run_paired<S: SystemMetricsSource>(
    database: Arc<DbActorHandle>,
    identity: CollectorIdentity,
    source: S,
    sink: Arc<dyn EventSink>,
    mut registry: CollectorRegistry,
    initial: PendingWrite,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), SystemRuntimeError> {
    if persist_pending(
        &database,
        &identity,
        sink.as_ref(),
        &mut registry,
        initial,
        None,
        &mut shutdown,
    )
    .await?
        == PersistOutcome::Shutdown
    {
        return Ok(());
    }

    let (collector, mut observations) =
        start_system_collector(start_sampler(source), OBSERVATION_CAPACITY);
    let mut outbox_monitor = time::interval(OUTBOX_MONITOR_INTERVAL);
    outbox_monitor.set_missed_tick_behavior(MissedTickBehavior::Skip);
    outbox_monitor.tick().await;

    loop {
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow_and_update() {
                    return stop_collector(collector).await;
                }
            }
            observation = observations.recv() => {
                let Some(observation) = observation else {
                    stop_collector(collector).await?;
                    return Err(SystemRuntimeError::WorkerStopped);
                };
                if handle_observation(
                    &database,
                    &identity,
                    sink.as_ref(),
                    &mut registry,
                    observation,
                    &collector,
                    &mut shutdown,
                ).await? == PersistOutcome::Shutdown {
                    return stop_collector(collector).await;
                }
            }
            _ = outbox_monitor.tick() => {
                if handle_outbox_monitor(
                    &database,
                    &identity,
                    sink.as_ref(),
                    &mut registry,
                    &collector,
                    &mut shutdown,
                ).await? == PersistOutcome::Shutdown {
                    return stop_collector(collector).await;
                }
            }
        }
    }
}

async fn handle_observation(
    database: &DbActorHandle,
    identity: &CollectorIdentity,
    sink: &dyn EventSink,
    registry: &mut CollectorRegistry,
    observation: SystemObservation,
    collector: &SystemCollectorHandle,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<PersistOutcome, SystemRuntimeError> {
    let (update, sample) = match observation {
        SystemObservation::Sampled {
            sample,
            observed_at_ms,
        } => {
            let update = registry.record_sample(&sample, observed_at_ms);
            (update, Some((sample, observed_at_ms)))
        }
        SystemObservation::Failed {
            group,
            error,
            observed_at_ms,
        } => {
            let update = registry.record_failure(group, &error, observed_at_ms);
            (update, None)
        }
    };
    collector.set_suppressed(update.sampling_suppressed);
    let occurred_at_ms = sample.as_ref().map_or_else(
        || update.state.updated_at_ms,
        |(_, observed_at_ms)| *observed_at_ms,
    );
    let pending = pending_from_update(
        identity,
        &update,
        sample.as_ref().map(|(sample, _)| sample),
        occurred_at_ms,
    )?;
    persist_pending(
        database,
        identity,
        sink,
        registry,
        pending,
        Some(collector),
        shutdown,
    )
    .await
}

async fn handle_outbox_monitor(
    database: &DbActorHandle,
    identity: &CollectorIdentity,
    sink: &dyn EventSink,
    registry: &mut CollectorRegistry,
    collector: &SystemCollectorHandle,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<PersistOutcome, SystemRuntimeError> {
    let update = if let Ok(depth) = database.active_outbox_depth().await {
        let was_suppressed = registry.sampling_suppressed();
        let update = registry.apply_outbox_depth(depth, clock_now_ms()?);
        if update.transition.is_none() && update.sampling_suppressed == was_suppressed {
            return Ok(PersistOutcome::Persisted);
        }
        update
    } else {
        registry.record_persistence_failure(clock_now_ms()?)
    };
    collector.set_suppressed(update.sampling_suppressed);
    let pending = pending_from_update(identity, &update, None, update.state.updated_at_ms)?;
    persist_pending(
        database,
        identity,
        sink,
        registry,
        pending,
        Some(collector),
        shutdown,
    )
    .await
}

async fn stop_collector(collector: SystemCollectorHandle) -> Result<(), SystemRuntimeError> {
    collector
        .shutdown()
        .await
        .map_err(SystemRuntimeError::Collector)
}

#[derive(Clone)]
enum PendingWrite {
    Events(EventCommit),
    State(CollectorState),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PersistOutcome {
    Persisted,
    Shutdown,
}

async fn persist_pending(
    database: &DbActorHandle,
    identity: &CollectorIdentity,
    sink: &dyn EventSink,
    registry: &mut CollectorRegistry,
    mut pending: PendingWrite,
    collector: Option<&SystemCollectorHandle>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<PersistOutcome, SystemRuntimeError> {
    let mut retry_index = 0_usize;
    loop {
        if *shutdown.borrow() {
            return Ok(PersistOutcome::Shutdown);
        }
        if write_pending(database, sink, pending.clone()).await.is_ok() {
            if registry.persistence_failed() {
                let recovery = registry.record_persistence_recovery(clock_now_ms()?);
                if let Some(collector) = collector {
                    collector.set_suppressed(recovery.sampling_suppressed);
                }
                pending =
                    pending_from_update(identity, &recovery, None, recovery.state.updated_at_ms)?;
                retry_index = 0;
                continue;
            }
            return Ok(PersistOutcome::Persisted);
        }

        if !registry.persistence_failed() {
            let failed = registry.record_persistence_failure(clock_now_ms()?);
            if let Some(collector) = collector {
                collector.set_suppressed(true);
            }
            pending =
                add_persistence_failure(identity, pending, &failed, failed.state.updated_at_ms)?;
        }

        let delay = RETRY_DELAYS[retry_index.min(RETRY_DELAYS.len() - 1)];
        retry_index = (retry_index + 1).min(RETRY_DELAYS.len() - 1);
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow_and_update() {
                    return Ok(PersistOutcome::Shutdown);
                }
            }
            () = time::sleep(delay) => {}
        }
    }
}

async fn write_pending(
    database: &DbActorHandle,
    sink: &dyn EventSink,
    pending: PendingWrite,
) -> Result<(), ()> {
    match pending {
        PendingWrite::Events(commit) => sink.commit(commit).await.map_err(|_| ()),
        PendingWrite::State(state) => {
            time::timeout(DATABASE_DEADLINE, database.upsert_collector_state(&state))
                .await
                .map_err(|_| ())?
                .map_err(|_| ())
        }
    }
}

fn add_persistence_failure(
    identity: &CollectorIdentity,
    pending: PendingWrite,
    failed: &RegistryUpdate,
    occurred_at_ms: i64,
) -> Result<PendingWrite, SystemRuntimeError> {
    let Some(transition) = failed.transition.as_ref() else {
        return replace_final_state(pending, failed.state.clone());
    };
    let now = time_from_ms(occurred_at_ms)?;
    let event = status_event(identity, transition, Uuid::new_v4(), now, now)
        .map_err(SystemRuntimeError::Domain)?;
    match pending {
        PendingWrite::Events(commit) => {
            let mut events = commit.events().to_vec();
            events.push(event);
            EventCommit::try_new(events, Some(failed.state.clone()))
                .map(PendingWrite::Events)
                .map_err(SystemRuntimeError::Domain)
        }
        PendingWrite::State(_) => EventCommit::try_new(vec![event], Some(failed.state.clone()))
            .map(PendingWrite::Events)
            .map_err(SystemRuntimeError::Domain),
    }
}

fn replace_final_state(
    pending: PendingWrite,
    state: CollectorState,
) -> Result<PendingWrite, SystemRuntimeError> {
    match pending {
        PendingWrite::Events(commit) => EventCommit::try_new(commit.events().to_vec(), Some(state))
            .map(PendingWrite::Events)
            .map_err(SystemRuntimeError::Domain),
        PendingWrite::State(_) => Ok(PendingWrite::State(state)),
    }
}

fn pending_from_update(
    identity: &CollectorIdentity,
    update: &RegistryUpdate,
    sample: Option<&SystemMetricSample>,
    occurred_at_ms: i64,
) -> Result<PendingWrite, SystemRuntimeError> {
    let occurred_at = time_from_ms(occurred_at_ms)?;
    let created_at = OffsetDateTime::now_utc();
    let mut events = Vec::with_capacity(3);
    if let Some(sample) = sample {
        events.push(
            metric_event(identity, sample, Uuid::new_v4(), occurred_at, created_at)
                .map_err(SystemRuntimeError::Domain)?,
        );
    }
    if let Some(transition) = update.transition.as_ref() {
        events.push(
            status_event(
                identity,
                transition,
                Uuid::new_v4(),
                occurred_at,
                created_at,
            )
            .map_err(SystemRuntimeError::Domain)?,
        );
    }
    if let Some(change) = update.health_change.as_ref() {
        events.push(
            health_event(identity, change, Uuid::new_v4(), occurred_at, created_at)
                .map_err(SystemRuntimeError::Domain)?,
        );
    }
    if events.is_empty() {
        Ok(PendingWrite::State(update.state.clone()))
    } else {
        EventCommit::try_new(events, Some(update.state.clone()))
            .map(PendingWrite::Events)
            .map_err(SystemRuntimeError::Domain)
    }
}

fn clock_now_ms() -> Result<i64, SystemRuntimeError> {
    let milliseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| SystemRuntimeError::Clock)?
        .as_millis();
    i64::try_from(milliseconds).map_err(|_| SystemRuntimeError::Clock)
}

fn time_from_ms(milliseconds: i64) -> Result<OffsetDateTime, SystemRuntimeError> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(milliseconds) * 1_000_000)
        .map_err(|_| SystemRuntimeError::Clock)
}

#[cfg(test)]
mod tests {
    use super::SystemRuntimeHandle;
    use crate::collector_registry::CollectorIdentity;
    use pca_db_local::DbActorHandle;
    use pca_domain::{
        AgentCpuMemory, CpuMemorySample, DiskSample, DiskScope, DomainError, EventCommit,
        EventSink, EventSinkFuture, HostCpuMemory,
    };
    use pca_system_collector::{SystemMetricsSource, SystemSampleError};
    use rusqlite::Connection;
    use std::{
        path::Path,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        },
        time::Duration,
    };
    use tempfile::TempDir;
    use uuid::Uuid;

    #[derive(Clone)]
    struct FakeControls {
        cpu_calls: Arc<AtomicUsize>,
        disk_calls: Arc<AtomicUsize>,
        cpu_failures_remaining: Arc<AtomicUsize>,
    }

    impl FakeControls {
        fn new() -> Self {
            Self {
                cpu_calls: Arc::new(AtomicUsize::new(0)),
                disk_calls: Arc::new(AtomicUsize::new(0)),
                cpu_failures_remaining: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn with_cpu_failures(times: usize) -> Self {
            let controls = Self::new();
            controls
                .cpu_failures_remaining
                .store(times, Ordering::SeqCst);
            controls
        }

        fn source(&self) -> FakeSource {
            FakeSource {
                controls: self.clone(),
            }
        }

        fn cpu_calls(&self) -> usize {
            self.cpu_calls.load(Ordering::SeqCst)
        }

        fn disk_calls(&self) -> usize {
            self.disk_calls.load(Ordering::SeqCst)
        }
    }

    struct FakeSource {
        controls: FakeControls,
    }

    impl SystemMetricsSource for FakeSource {
        fn sample_cpu_memory(&mut self) -> Result<CpuMemorySample, SystemSampleError> {
            self.controls.cpu_calls.fetch_add(1, Ordering::SeqCst);
            if self
                .controls
                .cpu_failures_remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                return Err(SystemSampleError {
                    kind: pca_system_collector::SystemSampleErrorKind::Retryable,
                    code: "SYSTEM_TEST_RETRYABLE",
                    message: "retry the deterministic CPU sample".to_owned(),
                });
            }
            Ok(CpuMemorySample::try_new(
                30_000,
                8,
                HostCpuMemory::try_new(25.0, 16_000, 8_000).expect("host fixture"),
                AgentCpuMemory::try_new(2.0, 128).expect("agent fixture"),
            )
            .expect("CPU fixture"))
        }

        fn sample_disk(&mut self) -> Result<DiskSample, SystemSampleError> {
            self.controls.disk_calls.fetch_add(1, Ordering::SeqCst);
            Ok(DiskSample::try_new(
                DiskScope::PcaDataVolume,
                8_589_934_592,
                4_294_967_296,
                50.0,
                false,
                2_147_483_648,
                None,
            )
            .expect("disk fixture"))
        }
    }

    #[derive(Default)]
    struct RecordingSink {
        attempts: Mutex<Vec<EventCommit>>,
        durable: Mutex<Vec<EventCommit>>,
        failures_remaining: AtomicUsize,
    }

    struct DirectDbSink {
        database: Arc<DbActorHandle>,
    }

    impl EventSink for DirectDbSink {
        fn commit(&self, commit: EventCommit) -> EventSinkFuture<'_> {
            Box::pin(async move {
                self.database.commit_events(&commit).await.map_err(|_| {
                    DomainError::new(
                        "COLLECTOR_DEGRADED",
                        "collector persistence unavailable",
                        true,
                    )
                })
            })
        }
    }

    impl RecordingSink {
        fn failing(times: usize) -> Self {
            Self {
                failures_remaining: AtomicUsize::new(times),
                ..Self::default()
            }
        }

        fn attempt_ids(&self) -> Vec<Vec<String>> {
            self.attempts
                .lock()
                .expect("attempts")
                .iter()
                .map(event_ids)
                .collect()
        }

        fn durable_ids(&self) -> Vec<Vec<String>> {
            self.durable
                .lock()
                .expect("durable")
                .iter()
                .map(event_ids)
                .collect()
        }
    }

    impl EventSink for RecordingSink {
        fn commit(&self, commit: EventCommit) -> EventSinkFuture<'_> {
            Box::pin(async move {
                self.attempts.lock().expect("attempts").push(commit.clone());
                if self
                    .failures_remaining
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                        remaining.checked_sub(1)
                    })
                    .is_ok()
                {
                    return Err(DomainError::new(
                        "COLLECTOR_DEGRADED",
                        "fixture persistence unavailable",
                        true,
                    ));
                }
                self.durable.lock().expect("durable").push(commit);
                Ok(())
            })
        }
    }

    fn event_ids(commit: &EventCommit) -> Vec<String> {
        commit
            .events()
            .iter()
            .map(|event| event.event_id.clone())
            .collect()
    }

    fn identity() -> CollectorIdentity {
        CollectorIdentity {
            workspace_id: Uuid::parse_str("91b1d43c-f018-45e0-8cee-2c702d66d258")
                .expect("workspace UUID"),
            device_id: Uuid::parse_str("50e57743-760b-4aba-b7d1-5f4689c3efaa")
                .expect("device UUID"),
        }
    }

    async fn open_database() -> (TempDir, Arc<DbActorHandle>) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = DbActorHandle::open(&directory.path().join("agent.sqlite3"), "test")
            .await
            .expect("open database");
        (directory, Arc::new(database))
    }

    async fn yield_until(mut condition: impl FnMut() -> bool) {
        for _ in 0..20_000 {
            if condition() {
                return;
            }
            tokio::task::yield_now().await;
            std::thread::yield_now();
        }
        panic!("condition did not become true");
    }

    async fn close_database(database: Arc<DbActorHandle>) {
        Arc::try_unwrap(database)
            .unwrap_or_else(|_| panic!("runtime retained a database reference"))
            .shutdown()
            .await
            .expect("shutdown database");
    }

    fn assert_two_hour_database(path: &Path) {
        let connection = Connection::open(path).expect("open for assertions");
        let metric_count = |group: &str| {
            connection
                .query_row(
                    "SELECT COUNT(*) FROM events_local
                     WHERE event_type = 'system.metric_sampled'
                       AND json_extract(payload_json, '$.metric_group') = ?1",
                    [group],
                    |row| row.get::<_, u64>(0),
                )
                .expect("metric count")
        };
        assert_eq!(metric_count("cpu_memory"), 241);
        assert_eq!(metric_count("disk"), 25);
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM events_local
                     WHERE event_type = 'collector.status_changed'",
                    [],
                    |row| row.get::<_, u64>(0),
                )
                .expect("status event count"),
            2
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM events_local
                     WHERE event_type = 'system.health_changed'",
                    [],
                    |row| row.get::<_, u64>(0),
                )
                .expect("health event count"),
            0
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT status || ':' || desired_revision || ':' || applied_revision
                     FROM collector_states WHERE collector_key = 'system'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("running collector state"),
            "running:0:0"
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM sync_outbox AS outbox
                     LEFT JOIN events_local AS event ON event.event_id = outbox.event_id
                     WHERE event.event_id IS NULL",
                    [],
                    |row| row.get::<_, u64>(0),
                )
                .expect("orphan count"),
            0
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM (
                         SELECT event_id FROM events_local GROUP BY event_id HAVING COUNT(*) > 1
                     )",
                    [],
                    |row| row.get::<_, u64>(0),
                )
                .expect("duplicate count"),
            0
        );
        let event_count = connection
            .query_row("SELECT COUNT(*) FROM events_local", [], |row| {
                row.get::<_, u64>(0)
            })
            .expect("event count");
        let outbox_count = connection
            .query_row("SELECT COUNT(*) FROM sync_outbox", [], |row| {
                row.get::<_, u64>(0)
            })
            .expect("outbox count");
        assert_eq!(event_count, outbox_count);
    }

    #[tokio::test(start_paused = true)]
    async fn unpaired_persists_disabled_without_sampling_or_events() {
        let (directory, database) = open_database().await;

        let runtime =
            SystemRuntimeHandle::start(Arc::clone(&database), None, directory.path().to_path_buf())
                .await
                .expect("start disabled runtime");
        runtime.shutdown().await.expect("shutdown runtime");
        let controls = FakeControls::new();
        let injected =
            SystemRuntimeHandle::start_with_source(Arc::clone(&database), None, controls.source())
                .await
                .expect("start disabled injected runtime");
        injected
            .shutdown()
            .await
            .expect("shutdown injected runtime");
        assert_eq!(controls.cpu_calls(), 0);
        assert_eq!(controls.disk_calls(), 0);

        close_database(database).await;

        let connection =
            Connection::open(directory.path().join("agent.sqlite3")).expect("open for assertions");
        assert_eq!(
            connection
                .query_row(
                    "SELECT status FROM collector_states WHERE collector_key = 'system'",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .expect("disabled state"),
            "disabled"
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM events_local
                     WHERE event_type LIKE 'system.%'
                        OR event_type = 'collector.status_changed'",
                    [],
                    |row| row.get::<_, u64>(0)
                )
                .expect("event count"),
            0
        );
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM sync_outbox", [], |row| {
                    row.get::<_, u64>(0)
                })
                .expect("outbox count"),
            0
        );
    }

    #[tokio::test(start_paused = true)]
    async fn persistence_retry_suppresses_samples_and_reuses_the_rebuilt_commit() {
        let (_directory, database) = open_database().await;
        let controls = FakeControls::new();
        let sink = Arc::new(RecordingSink::failing(2));

        let runtime = SystemRuntimeHandle::start_with_source_and_sink(
            Arc::clone(&database),
            Some(identity()),
            controls.source(),
            sink.clone(),
        )
        .await
        .expect("start paired runtime");
        yield_until(|| sink.attempt_ids().len() == 1).await;
        assert_eq!(controls.cpu_calls(), 0);
        assert_eq!(controls.disk_calls(), 0);

        tokio::time::advance(Duration::from_secs(30)).await;
        yield_until(|| sink.attempt_ids().len() == 2).await;
        assert_eq!(controls.cpu_calls(), 0);
        assert_eq!(controls.disk_calls(), 0);

        tokio::time::advance(Duration::from_secs(30) * 2).await;
        yield_until(|| sink.attempt_ids().len() >= 4).await;
        let attempts = sink.attempt_ids();
        assert_eq!(attempts[0].len(), 1);
        assert_eq!(&attempts[1][..attempts[0].len()], attempts[0].as_slice());
        assert_eq!(attempts[1], attempts[2]);
        assert_eq!(
            sink.durable_ids()
                .iter()
                .filter(|ids| **ids == attempts[2])
                .count(),
            1
        );
        yield_until(|| controls.cpu_calls() >= 1 && controls.disk_calls() >= 1).await;

        runtime.shutdown().await.expect("shutdown runtime");
        close_database(database).await;
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_sampling_failure_uses_state_only_without_advancing_last_event() {
        let (directory, database) = open_database().await;
        let controls = FakeControls::with_cpu_failures(2);
        let runtime = SystemRuntimeHandle::start_with_source_and_sink(
            Arc::clone(&database),
            Some(identity()),
            controls.source(),
            Arc::new(DirectDbSink {
                database: Arc::clone(&database),
            }),
        )
        .await
        .expect("start paired runtime");
        let observer =
            Connection::open(directory.path().join("agent.sqlite3")).expect("open observer");
        yield_until(|| {
            controls.cpu_calls() == 1
                && controls.disk_calls() == 1
                && observer
                    .query_row("SELECT COUNT(*) FROM events_local", [], |row| {
                        row.get::<_, u64>(0)
                    })
                    .expect("initial event count")
                    == 3
        })
        .await;
        let initial = observer
            .query_row(
                "SELECT last_event_at_ms, last_health_at_ms, updated_at_ms
                 FROM collector_states WHERE collector_key = 'system'",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .expect("initial failed state");
        std::thread::sleep(Duration::from_millis(2));

        tokio::time::advance(Duration::from_secs(30)).await;
        yield_until(|| {
            controls.cpu_calls() == 2
                && observer
                    .query_row(
                        "SELECT updated_at_ms FROM collector_states
                         WHERE collector_key = 'system'",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .expect("updated state")
                    > initial.2
        })
        .await;
        let repeated = observer
            .query_row(
                "SELECT last_event_at_ms, last_health_at_ms, status
                 FROM collector_states WHERE collector_key = 'system'",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .expect("repeated failed state");
        assert_eq!(repeated.0, initial.0);
        assert!(repeated.1 > initial.1);
        assert_eq!(repeated.2, "degraded");
        assert_eq!(
            observer
                .query_row("SELECT COUNT(*) FROM events_local", [], |row| {
                    row.get::<_, u64>(0)
                })
                .expect("final event count"),
            3
        );

        drop(observer);
        runtime.shutdown().await.expect("shutdown runtime");
        close_database(database).await;
    }

    #[tokio::test(start_paused = true)]
    async fn two_virtual_offline_hours_have_exact_cadence_and_atomic_outbox_rows() {
        let (directory, database) = open_database().await;
        let controls = FakeControls::new();
        let runtime = SystemRuntimeHandle::start_with_source_and_sink(
            Arc::clone(&database),
            Some(identity()),
            controls.source(),
            Arc::new(DirectDbSink {
                database: Arc::clone(&database),
            }),
        )
        .await
        .expect("start paired runtime");
        yield_until(|| controls.cpu_calls() == 1 && controls.disk_calls() == 1).await;
        let observer =
            Connection::open(directory.path().join("agent.sqlite3")).expect("open observer");
        let durable_metrics = |group: &str| {
            observer
                .query_row(
                    "SELECT COUNT(*) FROM events_local
                     WHERE event_type = 'system.metric_sampled'
                       AND json_extract(payload_json, '$.metric_group') = ?1",
                    [group],
                    |row| row.get::<_, usize>(0),
                )
                .expect("observe metric count")
        };
        yield_until(|| durable_metrics("cpu_memory") == 1 && durable_metrics("disk") == 1).await;

        for step in 1..=240 {
            tokio::time::advance(Duration::from_secs(30)).await;
            let expected_cpu = step + 1;
            let expected_disk = step / 10 + 1;
            let mut ready = false;
            for _ in 0..20_000 {
                if controls.cpu_calls() == expected_cpu
                    && controls.disk_calls() == expected_disk
                    && durable_metrics("cpu_memory") == expected_cpu
                    && durable_metrics("disk") == expected_disk
                {
                    ready = true;
                    break;
                }
                tokio::task::yield_now().await;
                std::thread::yield_now();
            }
            assert!(
                ready,
                "step {step}: expected cpu={expected_cpu} disk={expected_disk}, got cpu={} disk={}",
                controls.cpu_calls(),
                controls.disk_calls()
            );
            for _ in 0..32 {
                tokio::task::yield_now().await;
                std::thread::yield_now();
            }
        }
        yield_until(|| controls.cpu_calls() == 241 && controls.disk_calls() == 25).await;
        drop(observer);
        runtime.shutdown().await.expect("shutdown runtime");
        close_database(database).await;

        assert_two_hour_database(&directory.path().join("agent.sqlite3"));
    }
}
