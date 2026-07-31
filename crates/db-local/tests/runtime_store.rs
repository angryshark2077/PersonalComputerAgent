use std::{
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use pca_db_local::{DbActorHandle, DbError, BASELINE_MIGRATION, S1A_RUNTIME_MIGRATION};
use pca_domain::{
    AgentStatus, BridgeStatus, CollectorState, CollectorStatus, EventEnvelope, Sensitivity,
};
use rusqlite::Connection;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

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

fn collector_state(status: CollectorStatus) -> CollectorState {
    CollectorState {
        collector_key: "system".to_owned(),
        collector_version: "0.2.0".to_owned(),
        status,
        desired_config_revision: 7,
        applied_config_revision: 6,
        last_event_at_ms: Some(1_754_013_723_000),
        last_health_at_ms: Some(1_754_013_724_000),
        last_error_code: Some("SYSTEM_SAMPLE_FAILED".to_owned()),
        created_at_ms: 1_754_013_720_000,
        updated_at_ms: 1_754_013_725_000,
    }
}

fn apply_previous_migration_chain(connection: &Connection) {
    for (id, sql) in [
        ("0000", BASELINE_MIGRATION),
        ("0001", S1A_RUNTIME_MIGRATION),
    ] {
        connection.execute_batch(sql).expect("apply old migration");
        let checksum = format!("{:x}", Sha256::digest(sql.as_bytes()));
        connection
            .execute(
                "INSERT INTO schema_migrations (
                    id, checksum, app_version, started_at, completed_at, status
                 ) VALUES (?1, ?2, '0.1.0', 1, 1, 'completed')",
                (id, checksum),
            )
            .expect("record old migration");
    }
}

fn event_and_outbox_rows(connection: &Connection) -> (Vec<Vec<String>>, Vec<Vec<String>>) {
    let events = connection
        .prepare(
            "SELECT event_id, workspace_id, device_id, event_type, source,
                    CAST(schema_version AS TEXT), CAST(occurred_at_ms AS TEXT),
                    CAST(created_at_ms AS TEXT), sensitivity, payload_json,
                    attachment_refs_json, COALESCE(idempotency_key, '')
             FROM events_local ORDER BY event_id",
        )
        .expect("prepare Event snapshot")
        .query_map([], |row| {
            (0..12)
                .map(|index| row.get::<_, String>(index))
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .expect("query Event snapshot")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("read Event snapshot");
    let outbox = connection
        .prepare(
            "SELECT outbox_id, event_id, state, CAST(created_at_ms AS TEXT)
             FROM sync_outbox ORDER BY outbox_id",
        )
        .expect("prepare Outbox snapshot")
        .query_map([], |row| {
            (0..4)
                .map(|index| row.get::<_, String>(index))
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .expect("query Outbox snapshot")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("read Outbox snapshot");
    (events, outbox)
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

    assert_eq!(health.schema_version, 2);
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
            "collector_states",
            "diagnostic_events",
            "events_local",
            "local_meta",
            "schema_migrations",
            "sync_outbox",
        ]
    );
}

#[tokio::test]
async fn collector_state_survives_reopen_but_runtime_status_is_data_not_policy() {
    let (_directory, path) = database_path();
    let db = DbActorHandle::open(&path, "0.2.0")
        .await
        .expect("open database");
    let expected = collector_state(CollectorStatus::Running);
    db.upsert_collector_state(&expected)
        .await
        .expect("persist state");
    db.shutdown().await.expect("close database");

    let db = DbActorHandle::open(&path, "0.2.1")
        .await
        .expect("reopen database");
    assert_eq!(
        db.load_collector_states().await.expect("load state"),
        vec![expected]
    );
}

#[tokio::test]
async fn opening_previous_schema_adds_collector_state_without_changing_event_or_outbox() {
    let (_directory, path) = database_path();
    let connection = Connection::open(&path).expect("open previous database");
    apply_previous_migration_chain(&connection);
    connection
        .execute(
            "INSERT INTO events_local (
                event_id, workspace_id, device_id, event_type, source, schema_version,
                occurred_at_ms, created_at_ms, sensitivity, payload_json,
                attachment_refs_json, idempotency_key
             ) VALUES (
                'event-before-s2', 'workspace-1', 'device-1', 'AGENT_STARTED',
                'runtime', 1, 10, 11, 'normal', '{\"reason\":\"upgrade\"}',
                '[\"attachment-before-s2\"]', 'startup-before-s2'
             )",
            [],
        )
        .expect("insert previous Event");
    connection
        .execute(
            "INSERT INTO sync_outbox (outbox_id, event_id, state, created_at_ms)
             VALUES ('event:event-before-s2', 'event-before-s2', 'pending', 11)",
            [],
        )
        .expect("insert previous Outbox");
    let before = event_and_outbox_rows(&connection);
    drop(connection);

    let db = DbActorHandle::open(&path, "0.2.0")
        .await
        .expect("upgrade previous database");
    assert_eq!(
        db.health().await.expect("upgraded health").schema_version,
        2
    );
    db.shutdown().await.expect("close upgraded database");

    let connection = Connection::open(&path).expect("inspect upgraded database");
    assert_eq!(event_and_outbox_rows(&connection), before);
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE id = '0002'",
                [],
                |row| row.get::<_, u64>(0),
            )
            .expect("count S2 migration"),
        1
    );
}

#[tokio::test]
async fn database_and_wal_files_are_owner_read_write_only() {
    let (_directory, path) = database_path();

    let _db = DbActorHandle::open(&path, "0.1.0")
        .await
        .expect("open database");

    for runtime_file in [
        path.clone(),
        path.with_extension("sqlite3-wal"),
        path.with_extension("sqlite3-shm"),
    ] {
        let mode = std::fs::metadata(&runtime_file)
            .unwrap_or_else(|error| panic!("inspect {}: {error}", runtime_file.display()))
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode,
            0o600,
            "{} must be owner read/write only",
            runtime_file.display()
        );
    }
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
async fn duplicate_append_repairs_missing_outbox_with_stable_id() {
    let (_directory, path) = database_path();
    let db = DbActorHandle::open(&path, "0.1.0")
        .await
        .expect("open database");
    let event = event("event-repair-outbox");
    db.append_event_with_outbox(&event)
        .await
        .expect("initial append");
    let connection = Connection::open(&path).expect("open repair setup connection");
    connection
        .execute(
            "DELETE FROM sync_outbox WHERE event_id = ?1",
            [&event.event_id],
        )
        .expect("delete only Outbox row");

    db.append_event_with_outbox(&event)
        .await
        .expect("repair missing Outbox row");

    assert_eq!(
        db.count_event_and_outbox(&event.event_id)
            .await
            .expect("count repaired rows"),
        (1, 1)
    );
    let outbox_id = connection
        .query_row(
            "SELECT outbox_id FROM sync_outbox WHERE event_id = ?1",
            [&event.event_id],
            |row| row.get::<_, String>(0),
        )
        .expect("read stable Outbox identifier");
    assert_eq!(outbox_id, "event:event-repair-outbox");
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
             VALUES ('0003', 'future', '9.0.0', 1, 1, 'completed')",
            [],
        )
        .expect("record future migration");
    drop(connection);

    let result = DbActorHandle::open(&path, "0.1.0").await;

    assert!(matches!(
        result,
        Err(DbError::UnsupportedSchemaVersion {
            found: 3,
            max_supported: 2
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

    assert_eq!(health.schema_version, 2);
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

#[tokio::test]
async fn drop_does_not_block_on_a_cancelled_locked_request() {
    let (_directory, path) = database_path();
    let db = Arc::new(
        DbActorHandle::open(&path, "0.1.0")
            .await
            .expect("open database"),
    );
    let lock = Connection::open(&path).expect("open locking connection");
    lock.execute_batch("BEGIN IMMEDIATE")
        .expect("hold SQLite write lock");
    let request_db = Arc::clone(&db);
    let request = tokio::spawn(async move {
        request_db
            .append_event_with_outbox(&event("event-cancel-drop"))
            .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    request.abort();
    let _ = request.await;
    let started = Instant::now();

    drop(db);

    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_millis(250), "elapsed: {elapsed:?}");
    lock.execute_batch("ROLLBACK").expect("release write lock");
    tokio::time::sleep(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn async_shutdown_skips_cancelled_queued_requests_and_joins_owner() {
    let (_directory, path) = database_path();
    let db = Arc::new(
        DbActorHandle::open(&path, "0.1.0")
            .await
            .expect("open database"),
    );
    let lock = Connection::open(&path).expect("open locking connection");
    lock.execute_batch("BEGIN IMMEDIATE")
        .expect("hold SQLite write lock");
    let mut requests = Vec::new();
    for index in 0..16 {
        let request_db = Arc::clone(&db);
        requests.push(tokio::spawn(async move {
            request_db
                .append_event_with_outbox(&event(&format!("event-cancel-{index}")))
                .await
        }));
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    for request in &requests {
        request.abort();
    }
    for request in requests {
        let _ = request.await;
    }
    lock.execute_batch("ROLLBACK").expect("release write lock");
    let db = Arc::try_unwrap(db).unwrap_or_else(|_| panic!("request handles released"));

    db.shutdown().await.expect("join database owner thread");

    let connection = Connection::open(&path).expect("inspect canceled requests");
    let event_count = connection
        .query_row(
            "SELECT COUNT(*) FROM events_local WHERE event_id LIKE 'event-cancel-%'",
            [],
            |row| row.get::<_, u64>(0),
        )
        .expect("count canceled events");
    assert!(
        event_count <= 1,
        "canceled requests wrote {event_count} events"
    );
}
