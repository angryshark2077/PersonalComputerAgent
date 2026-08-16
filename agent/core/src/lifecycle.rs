use std::{fmt, sync::Arc, time::Duration};

use pca_bridge_client::PlatformLifecycleEvent;
use pca_db_local::{DbActorHandle, DbError};
use pca_domain::{EventEnvelope, Sensitivity};
use serde_json::Map;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use tokio::{
    sync::{mpsc, oneshot, Mutex, RwLock},
    task::JoinHandle,
    time as tokio_time,
};
use uuid::Uuid;

const EVENT_SOURCE: &str = "runtime.lifecycle";
const OPERATION_TIMEOUT: Duration = Duration::from_secs(10);
const WORKER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug)]
pub(crate) struct CapabilityRefreshError;

pub(crate) trait CapabilityRefresher: Send + Sync {
    fn refresh(&self) -> Result<(), CapabilityRefreshError>;
}

pub(crate) struct NoopCapabilityRefresher;

impl CapabilityRefresher for NoopCapabilityRefresher {
    fn refresh(&self) -> Result<(), CapabilityRefreshError> {
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RuntimeIdentity {
    workspace_id: String,
    device_id: String,
}

impl RuntimeIdentity {
    pub(crate) fn new(workspace_id: impl Into<String>, device_id: impl Into<String>) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            device_id: device_id.into(),
        }
    }
}

#[derive(Debug)]
pub(crate) enum LifecycleError {
    Database(DbError),
    NotAccepting,
    QueueClosed,
    WorkerStopped,
    TimedOut,
    CapabilityRefresh,
    IdentityUnavailable,
    UnsupportedPlatformEvent,
    Clock,
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "lifecycle database operation: {error}"),
            Self::NotAccepting => formatter.write_str("lifecycle side effects are paused"),
            Self::QueueClosed => formatter.write_str("lifecycle queue is closed"),
            Self::WorkerStopped => formatter.write_str("lifecycle worker stopped"),
            Self::TimedOut => formatter.write_str("lifecycle operation timed out"),
            Self::CapabilityRefresh => formatter.write_str("capability refresh failed"),
            Self::IdentityUnavailable => {
                formatter.write_str("paired lifecycle identity unavailable")
            }
            Self::UnsupportedPlatformEvent => {
                formatter.write_str("unsupported platform lifecycle event")
            }
            Self::Clock => formatter.write_str("system time cannot be formatted"),
        }
    }
}

impl std::error::Error for LifecycleError {}

impl From<DbError> for LifecycleError {
    fn from(error: DbError) -> Self {
        Self::Database(error)
    }
}

enum Command {
    Persist {
        event: EventEnvelope,
        checkpoint: bool,
        response: oneshot::Sender<Result<String, LifecycleError>>,
    },
    Checkpoint {
        response: oneshot::Sender<Result<(), LifecycleError>>,
    },
    Stop {
        event: Option<EventEnvelope>,
        response: oneshot::Sender<Result<Option<String>, LifecycleError>>,
    },
}

struct LifecycleGate {
    accepting: bool,
    sleep_preparing: bool,
}

pub(crate) struct LifecycleRuntime {
    sender: mpsc::Sender<Command>,
    gate: Arc<Mutex<LifecycleGate>>,
    identity: Arc<RwLock<Option<RuntimeIdentity>>>,
    capability_refresher: Arc<dyn CapabilityRefresher>,
    worker: JoinHandle<()>,
}

impl LifecycleRuntime {
    pub(crate) fn start(
        database: Arc<DbActorHandle>,
        identity: Option<RuntimeIdentity>,
        capacity: usize,
        capability_refresher: Arc<dyn CapabilityRefresher>,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(capacity.max(1));
        let worker = tokio::spawn(run_worker(database, receiver));
        Self {
            sender,
            gate: Arc::new(Mutex::new(LifecycleGate {
                accepting: true,
                sleep_preparing: false,
            })),
            identity: Arc::new(RwLock::new(identity)),
            capability_refresher,
            worker,
        }
    }

    pub(crate) async fn record_startup(&self) -> Result<String, LifecycleError> {
        self.persist("agent.started", false).await
    }

    pub(crate) async fn record_crash_recovery(&self) -> Result<String, LifecycleError> {
        self.persist("agent.crash_recovered", false).await
    }

    pub(crate) async fn update_identity(&self, identity: Option<RuntimeIdentity>) {
        *self.identity.write().await = identity;
    }

    pub(crate) async fn has_identity(&self) -> bool {
        self.identity.read().await.is_some()
    }

    pub(crate) async fn record_platform_event(
        &self,
        event: &PlatformLifecycleEvent,
    ) -> Result<Option<String>, LifecycleError> {
        match event.event_type.as_str() {
            "system.sleep" => {
                let identity = self.current_identity().await?;
                let mut gate = self.gate.lock().await;
                if !gate.accepting {
                    return Ok(None);
                }
                gate.accepting = false;
                gate.sleep_preparing = false;
                let envelope = lifecycle_event_at(&identity, event)?;
                let result = send_persist(&self.sender, envelope, true).await?;
                drop(gate);
                Ok(Some(result))
            }
            "system.wake" => {
                let identity = self.current_identity().await?;
                let mut gate = self.gate.lock().await;
                let result = async {
                    let envelope = lifecycle_event_at(&identity, event)?;
                    let event_id = send_persist(&self.sender, envelope, false).await?;
                    self.capability_refresher
                        .refresh()
                        .map_err(|_| LifecycleError::CapabilityRefresh)?;
                    Ok(event_id)
                }
                .await;
                gate.accepting = true;
                gate.sleep_preparing = false;
                result.map(Some)
            }
            "network.offline" | "network.online" | "network.changed" => {
                let gate = self.gate.lock().await;
                if !gate.accepting || gate.sleep_preparing {
                    return Ok(None);
                }
                let identity = self.current_identity().await?;
                let envelope = lifecycle_event_at(&identity, event)?;
                let event_id = send_persist(&self.sender, envelope, false).await?;
                drop(gate);
                Ok(Some(event_id))
            }
            _ => Err(LifecycleError::UnsupportedPlatformEvent),
        }
    }

    /// Stops new lifecycle side effects, drains earlier queue items, and checkpoints.
    ///
    /// The Bridge remains the sole source of the durable `system.sleep` event.  Keeping the
    /// prepare phase event-free prevents a timed-out request from committing a second sleep after
    /// the Bridge records its original power notification.
    #[allow(
        dead_code,
        reason = "typed boundary awaits a frozen authenticated sleep wire"
    )]
    pub(crate) async fn prepare_sleep(&self) -> Result<(), LifecycleError> {
        let _ = self.current_identity().await?;
        let mut gate = self.gate.lock().await;
        if !gate.accepting || gate.sleep_preparing {
            return Err(LifecycleError::NotAccepting);
        }
        gate.sleep_preparing = true;
        let result = send_checkpoint(&self.sender).await;
        if result.is_err() {
            gate.sleep_preparing = false;
        }
        drop(gate);
        result
    }

    /// Reopens lifecycle side effects when the Agent could not finish its bounded sleep prepare.
    pub(crate) async fn abort_sleep_preparation(&self) {
        let mut gate = self.gate.lock().await;
        gate.accepting = true;
        gate.sleep_preparing = false;
    }

    /// Records a wake after an internal sleep preparation and resumes lifecycle side effects.
    #[allow(
        dead_code,
        reason = "typed boundary awaits a frozen authenticated wake wire"
    )]
    pub(crate) async fn wake(&self) -> Result<String, LifecycleError> {
        let mut gate = self.gate.lock().await;
        if gate.accepting && !gate.sleep_preparing {
            return Err(LifecycleError::NotAccepting);
        }
        let identity = self.current_identity().await?;
        let result = async {
            let event = lifecycle_event(&identity, "system.wake")?;
            let event_id = send_persist(&self.sender, event, false).await?;
            self.capability_refresher
                .refresh()
                .map_err(|_| LifecycleError::CapabilityRefresh)?;
            Ok(event_id)
        }
        .await;
        gate.accepting = true;
        gate.sleep_preparing = false;
        result
    }

    /// Rejects new producers, drains queued work, and records clean stop when paired.
    pub(crate) async fn stop_and_drain(self) -> Result<Option<String>, LifecycleError> {
        self.finish(Some("agent.stopped")).await
    }

    pub(crate) async fn abort_and_drain(self) -> Result<(), LifecycleError> {
        self.finish(None).await.map(|_| ())
    }

    async fn finish(
        self,
        event_type: Option<&'static str>,
    ) -> Result<Option<String>, LifecycleError> {
        {
            let mut gate = self.gate.lock().await;
            gate.accepting = false;
            gate.sleep_preparing = false;
        }
        let mut first_error = None;
        let identity = self.identity.read().await.clone();
        let event = match event_type.and_then(|kind| {
            identity
                .as_ref()
                .map(|identity| lifecycle_event(identity, kind))
        }) {
            Some(Ok(event)) => Some(event),
            Some(Err(error)) => {
                first_error = Some(error);
                None
            }
            None => None,
        };
        let (response_sender, response_receiver) = oneshot::channel();
        let result = tokio_time::timeout(OPERATION_TIMEOUT, async {
            self.sender
                .send(Command::Stop {
                    event,
                    response: response_sender,
                })
                .await
                .map_err(|_| LifecycleError::QueueClosed)?;
            response_receiver
                .await
                .map_err(|_| LifecycleError::WorkerStopped)?
        })
        .await
        .unwrap_or(Err(LifecycleError::TimedOut));
        drop(self.sender);
        let mut worker = self.worker;
        let Ok(worker_result) = tokio_time::timeout(WORKER_SHUTDOWN_TIMEOUT, &mut worker).await
        else {
            worker.abort();
            let _ = worker.await;
            first_error.get_or_insert(LifecycleError::TimedOut);
            return Err(first_error.expect("timeout records lifecycle error"));
        };

        let event_id = match result {
            Ok(event_id) => event_id,
            Err(error) => {
                first_error.get_or_insert(error);
                None
            }
        };
        if worker_result.is_err() {
            first_error.get_or_insert(LifecycleError::WorkerStopped);
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(event_id),
        }
    }

    async fn persist(
        &self,
        event_type: &'static str,
        checkpoint: bool,
    ) -> Result<String, LifecycleError> {
        let gate = self.gate.lock().await;
        if !gate.accepting || gate.sleep_preparing {
            return Err(LifecycleError::NotAccepting);
        }
        let identity = self.current_identity().await?;
        let event = lifecycle_event(&identity, event_type)?;
        let response = send_persist(&self.sender, event, checkpoint).await;
        drop(gate);
        response
    }

    async fn current_identity(&self) -> Result<RuntimeIdentity, LifecycleError> {
        self.identity
            .read()
            .await
            .clone()
            .ok_or(LifecycleError::IdentityUnavailable)
    }
}

async fn send_persist(
    sender: &mpsc::Sender<Command>,
    event: EventEnvelope,
    checkpoint: bool,
) -> Result<String, LifecycleError> {
    let (response_sender, response_receiver) = oneshot::channel();
    tokio_time::timeout(OPERATION_TIMEOUT, async {
        sender
            .send(Command::Persist {
                event,
                checkpoint,
                response: response_sender,
            })
            .await
            .map_err(|_| LifecycleError::QueueClosed)?;
        response_receiver
            .await
            .map_err(|_| LifecycleError::WorkerStopped)?
    })
    .await
    .unwrap_or(Err(LifecycleError::TimedOut))
}

async fn send_checkpoint(sender: &mpsc::Sender<Command>) -> Result<(), LifecycleError> {
    let (response_sender, response_receiver) = oneshot::channel();
    tokio_time::timeout(OPERATION_TIMEOUT, async {
        sender
            .send(Command::Checkpoint {
                response: response_sender,
            })
            .await
            .map_err(|_| LifecycleError::QueueClosed)?;
        response_receiver
            .await
            .map_err(|_| LifecycleError::WorkerStopped)?
    })
    .await
    .unwrap_or(Err(LifecycleError::TimedOut))
}

async fn run_worker(database: Arc<DbActorHandle>, mut receiver: mpsc::Receiver<Command>) {
    while let Some(command) = receiver.recv().await {
        match command {
            Command::Persist {
                event,
                checkpoint,
                response,
            } => {
                let result = persist(&database, &event, checkpoint).await;
                let _ = response.send(result.map(|()| event.event_id));
            }
            Command::Checkpoint { response } => {
                let _ = response.send(database.checkpoint().await.map_err(LifecycleError::from));
            }
            Command::Stop { event, response } => {
                receiver.close();
                while let Some(queued) = receiver.recv().await {
                    match queued {
                        Command::Persist {
                            event,
                            checkpoint,
                            response,
                        } => {
                            let result = persist(&database, &event, checkpoint).await;
                            let _ = response.send(result.map(|()| event.event_id));
                        }
                        Command::Checkpoint { response } => {
                            let _ = response
                                .send(database.checkpoint().await.map_err(LifecycleError::from));
                        }
                        Command::Stop { response, .. } => {
                            let _ = response.send(Err(LifecycleError::QueueClosed));
                        }
                    }
                }
                let result = match event {
                    Some(event) => persist(&database, &event, false)
                        .await
                        .map(|()| Some(event.event_id)),
                    None => Ok(None),
                };
                let _ = response.send(result);
                break;
            }
        }
    }
}

async fn persist(
    database: &DbActorHandle,
    event: &EventEnvelope,
    checkpoint: bool,
) -> Result<(), LifecycleError> {
    database.append_event_with_outbox(event).await?;
    if checkpoint {
        database.checkpoint().await?;
    }
    Ok(())
}

fn lifecycle_event(
    identity: &RuntimeIdentity,
    event_type: &'static str,
) -> Result<EventEnvelope, LifecycleError> {
    let event_id = Uuid::new_v4().hyphenated().to_string();
    let timestamp = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|_| LifecycleError::Clock)?;
    Ok(EventEnvelope {
        event_id: event_id.clone(),
        workspace_id: identity.workspace_id.clone(),
        device_id: identity.device_id.clone(),
        event_type: event_type.to_owned(),
        source: EVENT_SOURCE.to_owned(),
        schema_version: 1,
        occurred_at: timestamp.clone(),
        created_at: timestamp,
        sensitivity: Sensitivity::Normal,
        payload: Map::new(),
        attachment_refs: Vec::new(),
        idempotency_key: Some(format!("lifecycle:{event_id}")),
    })
}

fn lifecycle_event_at(
    identity: &RuntimeIdentity,
    platform_event: &PlatformLifecycleEvent,
) -> Result<EventEnvelope, LifecycleError> {
    let created_at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|_| LifecycleError::Clock)?;
    let event_id = platform_event.event_id.hyphenated().to_string();
    Ok(EventEnvelope {
        event_id: event_id.clone(),
        workspace_id: identity.workspace_id.clone(),
        device_id: identity.device_id.clone(),
        event_type: platform_event.event_type.clone(),
        source: EVENT_SOURCE.to_owned(),
        schema_version: 1,
        occurred_at: platform_event.occurred_at.clone(),
        created_at,
        sensitivity: Sensitivity::Normal,
        payload: Map::new(),
        attachment_refs: Vec::new(),
        idempotency_key: Some(format!("lifecycle:{event_id}")),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex as StdMutex};

    use pca_bridge_client::PlatformLifecycleEvent;
    use pca_db_local::DbActorHandle;
    use uuid::Uuid;

    use super::{
        lifecycle_event, send_persist, CapabilityRefreshError, CapabilityRefresher, Command,
        LifecycleError, LifecycleRuntime, RuntimeIdentity, OPERATION_TIMEOUT,
    };

    struct RecordingRefresher {
        calls: Arc<StdMutex<Vec<&'static str>>>,
        database_path: Option<std::path::PathBuf>,
        fail: bool,
    }

    impl CapabilityRefresher for RecordingRefresher {
        fn refresh(&self) -> Result<(), CapabilityRefreshError> {
            let call = if let Some(path) = &self.database_path {
                let connection = rusqlite::Connection::open(path).expect("inspect refresh order");
                let wake_pairs: u64 = connection
                    .query_row(
                        "SELECT COUNT(*) FROM events_local e JOIN sync_outbox o ON o.event_id = e.event_id WHERE e.event_type = 'system.wake'",
                        [],
                        |row| row.get(0),
                    )
                    .expect("count wake pair during refresh");
                assert_eq!(wake_pairs, 1, "wake Event+Outbox must precede refresh");
                "refresh_after_wake_pair"
            } else {
                "refresh"
            };
            self.calls.lock().expect("refresh calls").push(call);
            if self.fail {
                Err(CapabilityRefreshError)
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn bridge_sleep_after_prepare_persists_one_atomic_lifecycle_pair() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let database = Arc::new(
            DbActorHandle::open(&directory.path().join("agent.sqlite3"), "0.0.0")
                .await
                .expect("open database"),
        );
        let runtime = LifecycleRuntime::start(
            Arc::clone(&database),
            Some(RuntimeIdentity::new("local-workspace", "local-device")),
            2,
            Arc::new(RecordingRefresher {
                calls: Arc::new(StdMutex::new(Vec::new())),
                database_path: None,
                fail: false,
            }),
        );

        runtime.record_startup().await.expect("record startup");
        runtime.prepare_sleep().await.expect("prepare sleep");
        assert!(matches!(
            runtime.record_startup().await,
            Err(LifecycleError::NotAccepting)
        ));
        let sleep_id = runtime
            .record_platform_event(&PlatformLifecycleEvent {
                sequence: 1,
                event_id: Uuid::new_v4(),
                event_type: "system.sleep".to_owned(),
                occurred_at: "2026-08-14T08:00:00Z".to_owned(),
            })
            .await
            .expect("record bridge sleep")
            .expect("sleep event is accepted after preparation");
        let wake_id = runtime
            .record_platform_event(&PlatformLifecycleEvent {
                sequence: 2,
                event_id: Uuid::new_v4(),
                event_type: "system.wake".to_owned(),
                occurred_at: "2026-08-14T09:00:00Z".to_owned(),
            })
            .await
            .expect("record bridge wake")
            .expect("wake event is accepted");
        runtime.stop_and_drain().await.expect("drain lifecycle");

        assert_eq!(
            database
                .count_event_and_outbox(&sleep_id)
                .await
                .expect("count sleep pair"),
            (1, 1)
        );
        assert_eq!(
            database
                .count_event_and_outbox(&wake_id)
                .await
                .expect("count wake pair"),
            (1, 1)
        );
    }

    #[tokio::test]
    async fn aborted_sleep_preparation_accepts_the_bridge_sleep_and_wake_pair() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let path = directory.path().join("agent.sqlite3");
        let database = Arc::new(
            DbActorHandle::open(&path, "0.0.0")
                .await
                .expect("open database"),
        );
        let runtime = LifecycleRuntime::start(
            database,
            Some(RuntimeIdentity::new("local-workspace", "local-device")),
            2,
            Arc::new(RecordingRefresher {
                calls: Arc::new(StdMutex::new(Vec::new())),
                database_path: None,
                fail: false,
            }),
        );

        runtime.prepare_sleep().await.expect("prepare sleep");
        runtime.abort_sleep_preparation().await;
        let sleep_id = runtime
            .record_platform_event(&PlatformLifecycleEvent {
                sequence: 1,
                event_id: Uuid::new_v4(),
                event_type: "system.sleep".to_owned(),
                occurred_at: "2026-08-14T08:00:00Z".to_owned(),
            })
            .await
            .expect("record bridge sleep after aborted preparation")
            .expect("sleep event is accepted");
        let wake_id = runtime
            .record_platform_event(&PlatformLifecycleEvent {
                sequence: 2,
                event_id: Uuid::new_v4(),
                event_type: "system.wake".to_owned(),
                occurred_at: "2026-08-14T09:00:00Z".to_owned(),
            })
            .await
            .expect("record bridge wake after aborted preparation")
            .expect("wake event is accepted");
        runtime.abort_and_drain().await.expect("drain lifecycle");

        let connection = rusqlite::Connection::open(path).expect("inspect lifecycle pairs");
        for event_id in [sleep_id, wake_id] {
            let pair_count: u64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM events_local e JOIN sync_outbox o ON o.event_id = e.event_id WHERE e.event_id = ?1",
                    [event_id],
                    |row| row.get(0),
                )
                .expect("count durable lifecycle event and outbox pair");
            assert_eq!(pair_count, 1);
        }
    }

    #[tokio::test]
    async fn platform_lifecycle_event_uses_current_paired_identity_and_wire_timestamp() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let path = directory.path().join("agent.sqlite3");
        let database = Arc::new(
            DbActorHandle::open(&path, "0.0.0")
                .await
                .expect("open database"),
        );
        let runtime = LifecycleRuntime::start(
            Arc::clone(&database),
            None,
            2,
            Arc::new(RecordingRefresher {
                calls: Arc::new(StdMutex::new(Vec::new())),
                database_path: None,
                fail: false,
            }),
        );
        runtime
            .update_identity(Some(RuntimeIdentity::new(
                "01983333-7333-8333-8333-333333333333",
                "01982222-7222-8222-8222-222222222222",
            )))
            .await;
        let event_id = Uuid::new_v4();
        runtime
            .record_platform_event(&PlatformLifecycleEvent {
                sequence: 1,
                event_id,
                event_type: "network.changed".to_owned(),
                occurred_at: "2026-08-04T15:00:00Z".to_owned(),
            })
            .await
            .expect("record network transition");
        runtime.abort_and_drain().await.expect("drain lifecycle");

        let connection = rusqlite::Connection::open(path).expect("inspect database");
        let stored: (String, String, String, String) = connection
            .query_row(
                "SELECT workspace_id, device_id, event_type, occurred_at_ms FROM events_local WHERE event_id = ?1",
                [event_id.hyphenated().to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get::<_, i64>(3)?.to_string())),
            )
            .expect("platform lifecycle event exists");
        assert_eq!(stored.0, "01983333-7333-8333-8333-333333333333");
        assert_eq!(stored.1, "01982222-7222-8222-8222-222222222222");
        assert_eq!(stored.2, "network.changed");
        assert_eq!(stored.3, "1785855600000");
    }

    #[tokio::test]
    async fn unpaired_runtime_drains_cleanly_without_emitting_a_stop_event() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let path = directory.path().join("agent.sqlite3");
        let database = Arc::new(
            DbActorHandle::open(&path, "0.0.0")
                .await
                .expect("open database"),
        );
        let runtime = LifecycleRuntime::start(
            database,
            None,
            2,
            Arc::new(RecordingRefresher {
                calls: Arc::new(StdMutex::new(Vec::new())),
                database_path: None,
                fail: false,
            }),
        );

        assert_eq!(
            runtime
                .stop_and_drain()
                .await
                .expect("unpaired lifecycle drains"),
            None
        );
        let connection = rusqlite::Connection::open(path).expect("inspect database");
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM events_local", [], |row| row
                    .get::<_, u64>(0))
                .expect("count lifecycle events"),
            0
        );
    }

    #[tokio::test]
    async fn unpaired_sleep_does_not_leave_lifecycle_paused_after_pairing() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let database = Arc::new(
            DbActorHandle::open(&directory.path().join("agent.sqlite3"), "0.0.0")
                .await
                .expect("open database"),
        );
        let runtime = LifecycleRuntime::start(
            database,
            None,
            2,
            Arc::new(RecordingRefresher {
                calls: Arc::new(StdMutex::new(Vec::new())),
                database_path: None,
                fail: false,
            }),
        );
        assert!(matches!(
            runtime
                .record_platform_event(&PlatformLifecycleEvent {
                    sequence: 1,
                    event_id: Uuid::new_v4(),
                    event_type: "system.sleep".to_owned(),
                    occurred_at: "2026-08-04T15:00:00Z".to_owned(),
                })
                .await,
            Err(LifecycleError::IdentityUnavailable)
        ));
        runtime
            .update_identity(Some(RuntimeIdentity::new(
                "01983333-7333-8333-8333-333333333333",
                "01982222-7222-8222-8222-222222222222",
            )))
            .await;
        let recorded = runtime
            .record_platform_event(&PlatformLifecycleEvent {
                sequence: 2,
                event_id: Uuid::new_v4(),
                event_type: "network.changed".to_owned(),
                occurred_at: "2026-08-04T15:01:00Z".to_owned(),
            })
            .await
            .expect("record network event after pairing");
        assert!(recorded.is_some());
        runtime.abort_and_drain().await.expect("drain lifecycle");
    }

    #[tokio::test]
    async fn unknown_platform_event_is_not_misclassified_as_missing_identity() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let database = Arc::new(
            DbActorHandle::open(&directory.path().join("agent.sqlite3"), "0.0.0")
                .await
                .expect("open database"),
        );
        let runtime = LifecycleRuntime::start(
            database,
            None,
            2,
            Arc::new(RecordingRefresher {
                calls: Arc::new(StdMutex::new(Vec::new())),
                database_path: None,
                fail: false,
            }),
        );

        let result = runtime
            .record_platform_event(&PlatformLifecycleEvent {
                sequence: 1,
                event_id: Uuid::new_v4(),
                event_type: "system.future".to_owned(),
                occurred_at: "2026-08-17T00:00:00Z".to_owned(),
            })
            .await;

        assert!(matches!(
            result,
            Err(LifecycleError::UnsupportedPlatformEvent)
        ));
        runtime.abort_and_drain().await.expect("drain lifecycle");
    }

    #[tokio::test]
    async fn wake_refreshes_capabilities_after_event_and_reopens_after_refresh_error() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let path = directory.path().join("agent.sqlite3");
        let database = Arc::new(
            DbActorHandle::open(&path, "0.0.0")
                .await
                .expect("open database"),
        );
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let runtime = LifecycleRuntime::start(
            Arc::clone(&database),
            Some(RuntimeIdentity::new("local-workspace", "local-device")),
            2,
            Arc::new(RecordingRefresher {
                calls: Arc::clone(&calls),
                database_path: Some(path.clone()),
                fail: true,
            }),
        );

        runtime.prepare_sleep().await.expect("prepare sleep");
        let wake = runtime.wake().await;

        assert!(matches!(wake, Err(LifecycleError::CapabilityRefresh)));
        assert_eq!(
            *calls.lock().expect("refresh calls"),
            vec!["refresh_after_wake_pair"]
        );
        runtime
            .record_startup()
            .await
            .expect("refresh failure must not leave lifecycle paused");
        let connection = rusqlite::Connection::open(&path).expect("inspect wake event ordering");
        let wake_pairs: u64 = connection
            .query_row(
                "SELECT COUNT(*) FROM events_local e JOIN sync_outbox o ON o.event_id = e.event_id WHERE e.event_type = 'system.wake'",
                [],
                |row| row.get(0),
            )
            .expect("count wake pair before refresh error");
        assert_eq!(
            wake_pairs, 1,
            "wake Event+Outbox precedes capability refresh"
        );
        runtime.abort_and_drain().await.expect("abort lifecycle");
    }

    #[tokio::test]
    async fn wake_refreshes_capabilities_even_when_the_sleep_event_was_missed() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let database = Arc::new(
            DbActorHandle::open(&directory.path().join("agent.sqlite3"), "0.0.0")
                .await
                .expect("open database"),
        );
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let runtime = LifecycleRuntime::start(
            database,
            Some(RuntimeIdentity::new("local-workspace", "local-device")),
            2,
            Arc::new(RecordingRefresher {
                calls: Arc::clone(&calls),
                database_path: None,
                fail: false,
            }),
        );

        let recorded = runtime
            .record_platform_event(&PlatformLifecycleEvent {
                sequence: 1,
                event_id: Uuid::new_v4(),
                event_type: "system.wake".to_owned(),
                occurred_at: "2026-08-14T08:00:00Z".to_owned(),
            })
            .await
            .expect("record standalone wake");

        assert!(recorded.is_some());
        assert_eq!(*calls.lock().expect("refresh calls"), vec!["refresh"]);
        runtime.abort_and_drain().await.expect("abort lifecycle");
    }

    #[tokio::test(start_paused = true)]
    async fn lifecycle_queue_wait_has_a_hard_deadline() {
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        let (stop_sender, _stop_receiver) = tokio::sync::oneshot::channel();
        sender
            .send(Command::Stop {
                event: None,
                response: stop_sender,
            })
            .await
            .expect("fill lifecycle queue");
        let event = lifecycle_event(
            &RuntimeIdentity::new("workspace", "device"),
            "agent.started",
        )
        .expect("lifecycle fixture");

        let started = tokio::time::Instant::now();
        let result = send_persist(&sender, event, false).await;

        assert!(matches!(result, Err(LifecycleError::TimedOut)));
        assert_eq!(started.elapsed(), OPERATION_TIMEOUT);
        drop(receiver);
    }
}
