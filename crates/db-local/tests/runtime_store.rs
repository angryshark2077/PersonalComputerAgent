use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use pca_db_local::{DbActorHandle, DbError};
use pca_domain::{AgentStatus, BridgeStatus, EventEnvelope, Sensitivity};
use rusqlite::Connection;
use serde_json::{Map, Value};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TempDirectory(PathBuf);

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn database_path() -> (TempDirectory, PathBuf) {
    let identifier = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let directory =
        std::env::temp_dir().join(format!("pca-db-local-{}-{identifier}", std::process::id()));
    std::fs::create_dir(&directory).expect("temporary directory");
    let path = directory.join("agent.sqlite3");
    (TempDirectory(directory), path)
}

fn event(event_id: &str) -> EventEnvelope {
    let mut payload = Map::new();
    payload.insert("reason".to_owned(), Value::String("test".to_owned()));

    EventEnvelope {
        event_id: event_id.to_owned(),
        workspace_id: "workspace-1".to_owned(),
        device_id: "device-1".to_owned(),
        event_type: "AGENT_STARTED".to_owned(),
        source: "runtime".to_owned(),
        schema_version: 1,
        occurred_at: "2026-07-31T01:02:03.456Z".to_owned(),
        created_at: "2026-07-31T01:02:04.567Z".to_owned(),
        sensitivity: Sensitivity::Normal,
        payload,
        attachment_refs: vec!["attachment-1".to_owned()],
        idempotency_key: Some("startup-1".to_owned()),
    }
}

fn schema(connection: &Connection) -> Vec<(String, String, String)> {
    let mut statement = connection
        .prepare(
            "SELECT type, name, sql FROM sqlite_schema \
             WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
        )
        .expect("prepare schema query");
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("query schema")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("read schema")
}

#[tokio::test]
async fn empty_database_is_migrated_and_reports_healthy() {
    let (_directory, path) = database_path();

    let db = DbActorHandle::open(&path, "0.1.0")
        .await
        .expect("open empty database");
    let health = db.health().await.expect("database health");

    assert_eq!(health.schema_version, 1);
    assert!(health.integrity_ok);
    assert!(health.foreign_keys_ok);
    let connection = Connection::open(&path).expect("inspect migrated database");
    let tables = connection
        .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
        .expect("prepare table query")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query tables")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("read tables");
    assert_eq!(
        tables,
        vec![
            "agent_state",
            "diagnostic_events",
            "events_local",
            "local_meta",
            "schema_migrations",
            "sync_outbox",
        ]
    );
}

#[tokio::test]
async fn replaying_migrations_does_not_change_schema() {
    let (_directory, path) = database_path();
    let db = DbActorHandle::open(&path, "0.1.0")
        .await
        .expect("first open");
    drop(db);
    let before = schema(&Connection::open(&path).expect("inspect first schema"));

    let db = DbActorHandle::open(&path, "0.1.1")
        .await
        .expect("reopen migrated database");
    drop(db);
    let after = schema(&Connection::open(&path).expect("inspect replayed schema"));

    assert_eq!(after, before);
}

#[tokio::test]
async fn duplicate_event_is_idempotent_for_event_and_outbox() {
    let (_directory, path) = database_path();
    let db = DbActorHandle::open(&path, "0.1.0")
        .await
        .expect("open database");
    let event = event("event-duplicate");

    db.append_event_with_outbox(&event)
        .await
        .expect("first append");
    db.append_event_with_outbox(&event)
        .await
        .expect("idempotent append");

    assert_eq!(
        db.count_event_and_outbox(&event.event_id)
            .await
            .expect("count durable rows"),
        (1, 1)
    );
}

#[tokio::test]
async fn outbox_insert_failure_rolls_back_event() {
    let (_directory, path) = database_path();
    let db = DbActorHandle::open(&path, "0.1.0")
        .await
        .expect("initialize database");
    drop(db);
    let connection = Connection::open(&path).expect("open test setup connection");
    connection
        .execute_batch(
            "CREATE TRIGGER reject_outbox_insert \
             BEFORE INSERT ON sync_outbox \
             BEGIN SELECT RAISE(ABORT, 'forced outbox failure'); END;",
        )
        .expect("install real SQLite failure trigger");
    drop(connection);
    let db = DbActorHandle::open(&path, "0.1.0")
        .await
        .expect("reopen database");
    let event = event("event-rollback");

    let result = db.append_event_with_outbox(&event).await;

    assert!(matches!(result, Err(DbError::Sqlite { .. })));
    assert_eq!(
        db.count_event_and_outbox(&event.event_id)
            .await
            .expect("count rolled back rows"),
        (0, 0)
    );
}

#[tokio::test]
async fn locked_database_fails_after_bounded_busy_timeout() {
    let (_directory, path) = database_path();
    let db = DbActorHandle::open(&path, "0.1.0")
        .await
        .expect("open database");
    let lock = Connection::open(&path).expect("open locking connection");
    lock.execute_batch("BEGIN IMMEDIATE")
        .expect("hold SQLite write lock");
    let event = event("event-locked");
    let started = Instant::now();

    let result = db.append_event_with_outbox(&event).await;

    let elapsed = started.elapsed();
    assert!(matches!(result, Err(DbError::Sqlite { .. })));
    assert!(elapsed >= Duration::from_secs(4), "elapsed: {elapsed:?}");
    assert!(elapsed < Duration::from_secs(8), "elapsed: {elapsed:?}");
    lock.execute_batch("ROLLBACK").expect("release write lock");
    assert_eq!(
        db.count_event_and_outbox(&event.event_id)
            .await
            .expect("count locked append"),
        (0, 0)
    );
}

#[tokio::test]
async fn integrity_failure_is_rejected_on_open() {
    let (_directory, path) = database_path();
    let db = DbActorHandle::open(&path, "0.1.0")
        .await
        .expect("initialize database");
    drop(db);
    let connection = Connection::open(&path).expect("open corruption setup connection");
    connection
        .execute_batch(
            "PRAGMA writable_schema = ON; \
             UPDATE sqlite_schema SET rootpage = 999999 \
             WHERE name = 'idx_events_local_occurred_at'; \
             PRAGMA writable_schema = OFF;",
        )
        .expect("corrupt an index root page");
    drop(connection);

    let result = DbActorHandle::open(&path, "0.1.0").await;

    match result {
        Err(DbError::IntegrityCheck { .. }) => {}
        Err(error) => panic!("expected integrity failure, got {error:?}"),
        Ok(_) => panic!("corrupted database unexpectedly opened"),
    }
}

#[tokio::test]
async fn unsupported_future_schema_version_is_rejected() {
    let (_directory, path) = database_path();
    let db = DbActorHandle::open(&path, "0.1.0")
        .await
        .expect("initialize database");
    drop(db);
    let connection = Connection::open(&path).expect("open future schema setup connection");
    connection
        .execute(
            "INSERT INTO schema_migrations \
             (id, checksum, app_version, started_at, completed_at, status) \
             VALUES ('0002', 'future', '9.0.0', 1, 1, 'completed')",
            [],
        )
        .expect("record future migration");
    drop(connection);

    let result = DbActorHandle::open(&path, "0.1.0").await;

    assert!(matches!(
        result,
        Err(DbError::UnsupportedSchemaVersion {
            found: 2,
            max_supported: 1
        })
    ));
}

#[tokio::test]
async fn agent_state_health_and_checkpoint_use_actor_requests() {
    let (_directory, path) = database_path();
    let db = DbActorHandle::open(&path, "0.1.0")
        .await
        .expect("open database");

    db.set_agent_state(
        AgentStatus::Running,
        BridgeStatus::Degraded,
        true,
        1_754_000_000_000,
    )
    .await
    .expect("persist agent state");
    db.checkpoint().await.expect("checkpoint WAL");
    let health = db.health().await.expect("health after checkpoint");

    assert_eq!(health.schema_version, 1);
    let connection = Connection::open(&path).expect("inspect agent state");
    let state = connection
        .query_row(
            "SELECT agent_status, bridge_status, local_healthy, updated_at_ms \
             FROM agent_state WHERE singleton_id = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .expect("read agent state");
    assert_eq!(
        state,
        (
            "running".to_owned(),
            "degraded".to_owned(),
            1,
            1_754_000_000_000
        )
    );
}
