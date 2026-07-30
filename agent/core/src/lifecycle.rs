use std::{fmt, sync::Arc};

use pca_db_local::{DbActorHandle, DbError};
use pca_domain::{EventEnvelope, Sensitivity};
use serde_json::Map;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use tokio::{
    sync::{mpsc, oneshot, Mutex},
    task::JoinHandle,
};
use uuid::Uuid;

const EVENT_SOURCE: &str = "runtime.lifecycle";

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
    Clock,
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "lifecycle database operation: {error}"),
            Self::NotAccepting => formatter.write_str("lifecycle side effects are paused"),
            Self::QueueClosed => formatter.write_str("lifecycle queue is closed"),
            Self::WorkerStopped => formatter.write_str("lifecycle worker stopped"),
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
    Stop {
        event: EventEnvelope,
        response: oneshot::Sender<Result<String, LifecycleError>>,
    },
}

pub(crate) struct LifecycleRuntime {
    sender: mpsc::Sender<Command>,
    accepting: Arc<Mutex<bool>>,
    identity: RuntimeIdentity,
    worker: JoinHandle<()>,
}

impl LifecycleRuntime {
    pub(crate) fn start(
        database: Arc<DbActorHandle>,
        identity: RuntimeIdentity,
        capacity: usize,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(capacity.max(1));
        let worker = tokio::spawn(run_worker(database, receiver));
        Self {
            sender,
            accepting: Arc::new(Mutex::new(true)),
            identity,
            worker,
        }
    }

    pub(crate) async fn record_startup(&self) -> Result<String, LifecycleError> {
        self.persist("AGENT_STARTED", false).await
    }

    pub(crate) async fn record_crash_recovery(&self) -> Result<String, LifecycleError> {
        self.persist("AGENT_CRASH_RECOVERED", false).await
    }

    /// Stops new lifecycle side effects, drains earlier queue items, records sleep, and checkpoints.
    #[allow(
        dead_code,
        reason = "typed boundary awaits a frozen authenticated sleep wire"
    )]
    pub(crate) async fn prepare_sleep(&self) -> Result<String, LifecycleError> {
        let mut accepting = self.accepting.lock().await;
        if !*accepting {
            return Err(LifecycleError::NotAccepting);
        }
        *accepting = false;
        let event = lifecycle_event(&self.identity, "SYSTEM_SLEEP")?;
        let result = send_persist(&self.sender, event, true).await;
        drop(accepting);
        result
    }

    /// Records a wake after an internal sleep preparation and resumes lifecycle side effects.
    #[allow(
        dead_code,
        reason = "typed boundary awaits a frozen authenticated wake wire"
    )]
    pub(crate) async fn wake(&self) -> Result<String, LifecycleError> {
        let mut accepting = self.accepting.lock().await;
        if *accepting {
            return Err(LifecycleError::NotAccepting);
        }
        let event = lifecycle_event(&self.identity, "SYSTEM_WAKE")?;
        let result = send_persist(&self.sender, event, false).await;
        if result.is_ok() {
            *accepting = true;
        }
        result
    }

    /// Rejects new producers, drains queued work, records clean stop, and joins the worker.
    pub(crate) async fn stop_and_drain(self) -> Result<String, LifecycleError> {
        let mut accepting = self.accepting.lock().await;
        *accepting = false;
        let event = lifecycle_event(&self.identity, "AGENT_STOPPED")?;
        let (response_sender, response_receiver) = oneshot::channel();
        self.sender
            .send(Command::Stop {
                event,
                response: response_sender,
            })
            .await
            .map_err(|_| LifecycleError::QueueClosed)?;
        drop(accepting);
        let result = response_receiver
            .await
            .map_err(|_| LifecycleError::WorkerStopped)?;
        drop(self.sender);
        self.worker
            .await
            .map_err(|_| LifecycleError::WorkerStopped)?;
        result
    }

    async fn persist(
        &self,
        event_type: &'static str,
        checkpoint: bool,
    ) -> Result<String, LifecycleError> {
        let accepting = self.accepting.lock().await;
        if !*accepting {
            return Err(LifecycleError::NotAccepting);
        }
        let event = lifecycle_event(&self.identity, event_type)?;
        let response = send_persist(&self.sender, event, checkpoint).await;
        drop(accepting);
        response
    }
}

async fn send_persist(
    sender: &mpsc::Sender<Command>,
    event: EventEnvelope,
    checkpoint: bool,
) -> Result<String, LifecycleError> {
    let (response_sender, response_receiver) = oneshot::channel();
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
            Command::Stop { event, response } => {
                receiver.close();
                while let Some(queued) = receiver.recv().await {
                    if let Command::Persist {
                        event,
                        checkpoint,
                        response,
                    } = queued
                    {
                        let result = persist(&database, &event, checkpoint).await;
                        let _ = response.send(result.map(|()| event.event_id));
                    }
                }
                let result = persist(&database, &event, false).await;
                let _ = response.send(result.map(|()| event.event_id));
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use pca_db_local::DbActorHandle;

    use super::{LifecycleError, LifecycleRuntime, RuntimeIdentity};

    #[tokio::test]
    async fn typed_prepare_sleep_and_wake_persist_atomic_lifecycle_pairs() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let database = Arc::new(
            DbActorHandle::open(&directory.path().join("agent.sqlite3"), "0.0.0")
                .await
                .expect("open database"),
        );
        let runtime = LifecycleRuntime::start(
            Arc::clone(&database),
            RuntimeIdentity::new("local-workspace", "local-device"),
            2,
        );

        runtime.record_startup().await.expect("record startup");
        let sleep_id = runtime.prepare_sleep().await.expect("prepare sleep");
        assert!(matches!(
            runtime.record_startup().await,
            Err(LifecycleError::NotAccepting)
        ));
        let wake_id = runtime.wake().await.expect("record wake");
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
}
