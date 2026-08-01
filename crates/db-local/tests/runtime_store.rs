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
    AgentStatus, BridgeStatus, CollectorState, CollectorStatus, EventCommit, EventEnvelope,
    Sensitivity,
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

fn system_metric_event(event_id: &str) -> EventEnvelope {
    let mut payload = Map::new();
    payload.insert(
        "metric_group".to_owned(),
        Value::String("cpu_memory".to_owned()),
    );
    payload.insert("sample_window_ms".to_owned(), Value::from(30_000));
    payload.insert("logical_cpu_count".to_owned(), Value::from(10));
    payload.insert(
        "host".to_owned(),
        serde_json::json!({
            "cpu_usage_percent": 12.34,
            "memory_total_bytes": 34_359_738_368_u64,
            "memory_used_bytes": 17_179_869_184_u64,
        }),
    );
    payload.insert(
        "agent".to_owned(),
        serde_json::json!({ "cpu_usage_percent": 0.42, "memory_resident_bytes": 73_400_320_u64 }),
    );
    EventEnvelope {
        event_id: event_id.to_owned(),
        workspace_id: "01983333-7333-8333-8333-333333333333".to_owned(),
        device_id: "01982222-7222-8222-8222-222222222222".to_owned(),
        event_type: "system.metric_sampled".to_owned(),
        source: "system".to_owned(),
        schema_version: 1,
        occurred_at: "2026-08-02T00:00:00Z".to_owned(),
        created_at: "2026-08-02T00:00:00Z".to_owned(),
        sensitivity: Sensitivity::Normal,
        payload,
        attachment_refs: Vec::new(),
        idempotency_key: Some(format!("system:{event_id}")),
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

fn collector_commit(event_ids: &[&str], state: CollectorState) -> EventCommit {
    EventCommit::try_new(
        event_ids.iter().map(|event_id| event(event_id)).collect(),
        Some(state),
    )
    .expect("valid Collector commit")
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

    assert_eq!(health.schema_version, 4);
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
            "pairing_state",
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
async fn opening_previous_schema_adds_new_state_tables_without_changing_event_or_outbox() {
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
        4
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
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE id = '0004'",
                [],
                |row| row.get::<_, u64>(0),
            )
            .expect("count Cloud origin migration"),
        1
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE id = '0003'",
                [],
                |row| row.get::<_, u64>(0),
            )
            .expect("count S1B migration"),
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
async fn collector_commit_persists_event_outbox_and_state_idempotently() {
    let (_directory, path) = database_path();
    let db = DbActorHandle::open(&path, "0.2.0")
        .await
        .expect("open database");
    let expected_state = collector_state(CollectorStatus::Running);
    let event_ids = ["collector-commit-metric", "collector-commit-health"];
    let commit = collector_commit(&event_ids, expected_state.clone());

    db.commit_events(&commit)
        .await
        .expect("first Collector commit");
    db.commit_events(&commit)
        .await
        .expect("idempotent Collector commit");

    for event_id in event_ids {
        assert_eq!(
            db.count_event_and_outbox(event_id)
                .await
                .expect("count committed rows"),
            (1, 1)
        );
    }
    assert_eq!(
        db.load_collector_states().await.expect("load final state"),
        vec![expected_state]
    );
}

#[tokio::test]
async fn collector_commit_rejects_existing_event_id_when_any_immutable_field_differs() {
    let (_directory, path) = database_path();
    let db = DbActorHandle::open(&path, "0.2.0")
        .await
        .expect("open database");
    let prior_state = collector_state(CollectorStatus::Initializing);
    db.upsert_collector_state(&prior_state)
        .await
        .expect("persist prior state");
    let mut cases = Vec::new();
    for (case, mutate) in [
        ("workspace", 0_u8),
        ("device", 1),
        ("type", 2),
        ("source", 3),
        ("schema", 4),
        ("occurred", 5),
        ("created", 6),
        ("sensitivity", 7),
        ("payload", 8),
        ("attachments", 9),
        ("idempotency", 10),
    ] {
        let event_id = format!("collector-conflict-{case}");
        let original = event(&event_id);
        let mut conflicting = original.clone();
        match mutate {
            0 => conflicting.workspace_id = "workspace-2".to_owned(),
            1 => conflicting.device_id = "device-2".to_owned(),
            2 => conflicting.event_type = "SYSTEM_METRIC_SAMPLED".to_owned(),
            3 => conflicting.source = "system".to_owned(),
            4 => conflicting.schema_version = 2,
            5 => conflicting.occurred_at = "2026-07-31T01:02:05.456Z".to_owned(),
            6 => conflicting.created_at = "2026-07-31T01:02:06.567Z".to_owned(),
            7 => conflicting.sensitivity = Sensitivity::High,
            8 => {
                conflicting
                    .payload
                    .insert("reason".to_owned(), Value::String("different".to_owned()));
            }
            9 => conflicting.attachment_refs = vec!["attachment-2".to_owned()],
            10 => conflicting.idempotency_key = Some("startup-2".to_owned()),
            _ => unreachable!("all immutable Event fields covered"),
        }
        cases.push((case, original, conflicting));
    }

    for (case, original, conflicting) in cases {
        db.append_event_with_outbox(&original)
            .await
            .unwrap_or_else(|error| panic!("seed {case}: {error}"));
        let before = event_and_outbox_rows(&Connection::open(&path).expect("open before snapshot"));
        let commit = EventCommit::try_new(
            vec![conflicting],
            Some(collector_state(CollectorStatus::Running)),
        )
        .expect("valid conflicting commit");

        let result = db.commit_events(&commit).await;

        assert!(
            matches!(result, Err(DbError::Sqlite { .. })),
            "{case} conflict unexpectedly succeeded"
        );
        let after = event_and_outbox_rows(&Connection::open(&path).expect("open after snapshot"));
        assert_eq!(after, before, "{case} conflict changed Event or Outbox");
        assert_eq!(
            db.load_collector_states()
                .await
                .expect("load preserved state"),
            vec![prior_state.clone()],
            "{case} conflict advanced Collector state"
        );
    }
}

#[tokio::test]
async fn collector_commit_rejects_inconsistent_stable_outbox_and_rolls_back() {
    let (_directory, path) = database_path();
    let db = DbActorHandle::open(&path, "0.2.0")
        .await
        .expect("open database");
    let prior_state = collector_state(CollectorStatus::Initializing);
    db.upsert_collector_state(&prior_state)
        .await
        .expect("persist prior state");
    db.append_event_with_outbox(&event("collector-outbox-wrong-entity"))
        .await
        .expect("seed wrong Outbox entity");
    let connection = Connection::open(&path).expect("open Outbox corruption setup");
    connection
        .execute(
            "UPDATE sync_outbox SET outbox_id = 'event:collector-outbox-target'
             WHERE event_id = 'collector-outbox-wrong-entity'",
            [],
        )
        .expect("point stable Outbox ID at wrong Event");
    drop(connection);
    let before = event_and_outbox_rows(&Connection::open(&path).expect("open before snapshot"));
    let commit = collector_commit(
        &["collector-outbox-target"],
        collector_state(CollectorStatus::Running),
    );

    let result = db.commit_events(&commit).await;

    assert!(matches!(result, Err(DbError::Sqlite { .. })));
    let after = event_and_outbox_rows(&Connection::open(&path).expect("open after snapshot"));
    assert_eq!(after, before);
    assert_eq!(
        db.load_collector_states()
            .await
            .expect("load preserved state"),
        vec![prior_state]
    );
}

#[tokio::test]
async fn collector_commit_rejects_existing_outbox_with_different_created_time() {
    let (_directory, path) = database_path();
    let db = DbActorHandle::open(&path, "0.2.0")
        .await
        .expect("open database");
    let prior_state = collector_state(CollectorStatus::Initializing);
    db.upsert_collector_state(&prior_state)
        .await
        .expect("persist prior state");
    let existing = event("collector-outbox-created-conflict");
    db.append_event_with_outbox(&existing)
        .await
        .expect("seed Event and Outbox");
    Connection::open(&path)
        .expect("open Outbox corruption setup")
        .execute(
            "UPDATE sync_outbox SET created_at_ms = created_at_ms + 1
             WHERE event_id = ?1",
            [&existing.event_id],
        )
        .expect("change stable Outbox creation time");
    let before = event_and_outbox_rows(&Connection::open(&path).expect("open before snapshot"));
    let commit = EventCommit::try_new(
        vec![existing],
        Some(collector_state(CollectorStatus::Running)),
    )
    .expect("valid retry commit");

    let result = db.commit_events(&commit).await;

    assert!(matches!(result, Err(DbError::Sqlite { .. })));
    let after = event_and_outbox_rows(&Connection::open(&path).expect("open after snapshot"));
    assert_eq!(after, before);
    assert_eq!(
        db.load_collector_states()
            .await
            .expect("load preserved state"),
        vec![prior_state]
    );
}

#[tokio::test]
async fn collector_commit_rejects_different_events_with_duplicate_id_in_one_batch() {
    let (_directory, path) = database_path();
    let db = DbActorHandle::open(&path, "0.2.0")
        .await
        .expect("open database");
    let prior_state = collector_state(CollectorStatus::Initializing);
    db.upsert_collector_state(&prior_state)
        .await
        .expect("persist prior state");
    let first = event("collector-batch-duplicate-conflict");
    let mut conflicting = first.clone();
    conflicting
        .payload
        .insert("reason".to_owned(), Value::String("different".to_owned()));
    let commit = EventCommit::try_new(
        vec![first, conflicting],
        Some(collector_state(CollectorStatus::Running)),
    )
    .expect("valid bounded commit");

    let result = db.commit_events(&commit).await;

    assert!(matches!(result, Err(DbError::Sqlite { .. })));
    assert_eq!(
        db.count_event_and_outbox("collector-batch-duplicate-conflict")
            .await
            .expect("count rolled back duplicate"),
        (0, 0)
    );
    assert_eq!(
        db.load_collector_states()
            .await
            .expect("load preserved state"),
        vec![prior_state]
    );
}

#[tokio::test]
async fn collector_commit_accepts_identical_events_with_duplicate_id_in_one_batch() {
    let (_directory, path) = database_path();
    let db = DbActorHandle::open(&path, "0.2.0")
        .await
        .expect("open database");
    let duplicate = event("collector-batch-identical-duplicate");
    let expected_state = collector_state(CollectorStatus::Running);
    let commit = EventCommit::try_new(
        vec![duplicate.clone(), duplicate],
        Some(expected_state.clone()),
    )
    .expect("valid bounded commit");

    db.commit_events(&commit)
        .await
        .expect("collapse identical duplicate Events");

    assert_eq!(
        db.count_event_and_outbox("collector-batch-identical-duplicate")
            .await
            .expect("count idempotent duplicate"),
        (1, 1)
    );
    assert_eq!(
        db.load_collector_states().await.expect("load final state"),
        vec![expected_state]
    );
}

#[tokio::test]
async fn collector_commit_state_failure_rolls_back_events_outbox_and_prior_state() {
    let (_directory, path) = database_path();
    let db = DbActorHandle::open(&path, "0.2.0")
        .await
        .expect("open database");
    let prior_state = collector_state(CollectorStatus::Initializing);
    db.upsert_collector_state(&prior_state)
        .await
        .expect("persist prior state");
    let connection = Connection::open(&path).expect("open failure setup connection");
    connection
        .execute_batch(
            "CREATE TRIGGER reject_collector_state_upsert \
             BEFORE INSERT ON collector_states \
             BEGIN SELECT RAISE(ABORT, 'forced Collector state failure'); END;",
        )
        .expect("install real SQLite failure trigger");
    drop(connection);
    let event_ids = [
        "collector-commit-rollback-metric",
        "collector-commit-rollback-health",
    ];
    let commit = collector_commit(&event_ids, collector_state(CollectorStatus::Running));

    let result = db.commit_events(&commit).await;

    assert!(matches!(result, Err(DbError::Sqlite { .. })));
    for event_id in event_ids {
        assert_eq!(
            db.count_event_and_outbox(event_id)
                .await
                .expect("count rolled back rows"),
            (0, 0)
        );
    }
    assert_eq!(
        db.load_collector_states()
            .await
            .expect("load preserved prior state"),
        vec![prior_state]
    );
}

#[tokio::test]
async fn active_depth_excludes_only_acked_rows() {
    let (_directory, path) = database_path();
    let db = DbActorHandle::open(&path, "0.2.0")
        .await
        .expect("open database");
    for state in ["pending", "sending", "acked", "conflict", "dead_letter"] {
        db.append_event_with_outbox(&event(&format!("outbox-{state}")))
            .await
            .expect("seed Outbox row");
    }
    let connection = Connection::open(&path).expect("open Outbox setup connection");
    for state in ["sending", "acked", "conflict", "dead_letter"] {
        connection
            .execute(
                "UPDATE sync_outbox SET state = ?1 WHERE event_id = ?2",
                (state, format!("outbox-{state}")),
            )
            .expect("set Outbox state");
    }

    assert_eq!(db.active_outbox_depth().await.expect("active depth"), 4);
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
    assert!(elapsed < Duration::from_secs(12), "elapsed: {elapsed:?}");
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
             VALUES ('0005', 'future', '9.0.0', 1, 1, 'completed')",
            [],
        )
        .expect("record future migration");
    drop(connection);

    let result = DbActorHandle::open(&path, "0.1.0").await;

    assert!(matches!(
        result,
        Err(DbError::UnsupportedSchemaVersion {
            found: 5,
            max_supported: 4
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

    assert_eq!(health.schema_version, 4);
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

#[tokio::test]
async fn system_sync_batch_excludes_lifecycle_events_and_acks_only_accepted_system_events() {
    let (_directory, path) = database_path();
    let db = DbActorHandle::open(&path, "0.1.0")
        .await
        .expect("open database");
    let system = system_metric_event("01986666-7666-8666-8666-666666666666");
    db.append_event_with_outbox(&system)
        .await
        .expect("persist system event");
    db.append_event_with_outbox(&event("lifecycle-event"))
        .await
        .expect("persist lifecycle event");

    let pending = db
        .load_pending_system_events(20)
        .await
        .expect("load pending system events");
    assert_eq!(pending, vec![system.clone()]);

    db.acknowledge_system_events(std::slice::from_ref(&system.event_id))
        .await
        .expect("acknowledge accepted system event");
    assert!(db
        .load_pending_system_events(20)
        .await
        .expect("reload pending system events")
        .is_empty());
    assert_eq!(db.active_outbox_depth().await.expect("outbox depth"), 1);
}
