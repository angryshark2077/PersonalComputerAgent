use std::{path::Path, thread, time::Duration};

use pca_domain::{AgentStatus, BridgeStatus, EventEnvelope};
use rusqlite::Connection;
use tokio::sync::{mpsc, oneshot};

use crate::{migrations, repository, DbError, DbHealth};

const REQUEST_CAPACITY: usize = 64;

enum Request {
    AppendEvent {
        event: Box<EventEnvelope>,
        response: oneshot::Sender<Result<(), DbError>>,
    },
    SetAgentState {
        agent_status: AgentStatus,
        bridge_status: BridgeStatus,
        local_healthy: bool,
        updated_at_ms: i64,
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
}

/// Async handle to the single thread that owns the local `SQLite` connection.
pub struct DbActorHandle {
    requests: Option<mpsc::Sender<Request>>,
    owner_thread: Option<thread::JoinHandle<()>>,
}

impl DbActorHandle {
    /// Opens and verifies a local database before accepting requests.
    ///
    /// # Errors
    ///
    /// Returns a migration, integrity, configuration, or actor startup error.
    pub async fn open(path: &Path, app_version: &str) -> Result<Self, DbError> {
        let (request_sender, request_receiver) = mpsc::channel(REQUEST_CAPACITY);
        let (startup_sender, startup_receiver) = oneshot::channel();
        let path = path.to_owned();
        let app_version = app_version.to_owned();
        let owner_thread = thread::Builder::new()
            .name("pca-sqlite-owner".to_owned())
            .spawn(move || {
                let result = open_connection(&path, &app_version);
                match result {
                    Ok(connection) => {
                        let _ = startup_sender.send(Ok(()));
                        run(connection, request_receiver);
                    }
                    Err(error) => {
                        let _ = startup_sender.send(Err(error));
                    }
                }
            })
            .map_err(|error| DbError::sqlite("spawn database owner thread", error))?;

        match startup_receiver.await {
            Ok(Ok(())) => Ok(Self {
                requests: Some(request_sender),
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
            let _ = owner_thread.join();
        }
    }
}

async fn receive<T>(receiver: oneshot::Receiver<Result<T, DbError>>) -> Result<T, DbError> {
    receiver.await.map_err(|_| DbError::ActorUnavailable)?
}

fn open_connection(path: &Path, app_version: &str) -> Result<Connection, DbError> {
    let mut connection =
        Connection::open(path).map_err(|error| DbError::startup_sqlite("open database", error))?;
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

fn run(mut connection: Connection, mut requests: mpsc::Receiver<Request>) {
    while let Some(request) = requests.blocking_recv() {
        match request {
            Request::AppendEvent { event, response } => {
                let _ = response.send(repository::append_event_with_outbox(
                    &mut connection,
                    &event,
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
            Request::Health { response } => {
                let _ = response.send(repository::health(&connection));
            }
            Request::Checkpoint { response } => {
                let _ = response.send(repository::checkpoint(&connection));
            }
            Request::CountEventAndOutbox { event_id, response } => {
                let _ = response.send(repository::count_event_and_outbox(&connection, &event_id));
            }
        }
    }
}
