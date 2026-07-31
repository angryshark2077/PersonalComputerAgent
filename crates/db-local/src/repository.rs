use pca_domain::{
    AgentStatus, BridgeStatus, CollectorState, CollectorStatus, EventEnvelope, Sensitivity,
};
use rusqlite::{params, Connection};

#[cfg(feature = "process-test-hooks")]
use std::{
    fs::{self, OpenOptions},
    io::Write,
    thread,
    time::{Duration, Instant},
};

#[cfg(feature = "process-test-hooks")]
use crate::actor::ProcessTestHooks;
use crate::{migrations::MAX_SUPPORTED_SCHEMA_VERSION, DbError, DbHealth};

pub(crate) fn append_event_with_outbox(
    connection: &mut Connection,
    event: &EventEnvelope,
    #[cfg(feature = "process-test-hooks")] process_test_hooks: Option<&ProcessTestHooks>,
) -> Result<(), DbError> {
    let payload_json = serde_json::to_string(&event.payload)
        .map_err(|error| DbError::Serialization(error.to_string()))?;
    let attachment_refs_json = serde_json::to_string(&event.attachment_refs)
        .map_err(|error| DbError::Serialization(error.to_string()))?;
    let outbox_id = format!("event:{}", event.event_id);
    let transaction = connection
        .transaction()
        .map_err(|error| DbError::sqlite("start event transaction", error))?;
    transaction
        .execute(
            "INSERT INTO events_local (
                event_id, workspace_id, device_id, event_type, source,
                schema_version, occurred_at_ms, created_at_ms, sensitivity,
                payload_json, attachment_refs_json, idempotency_key
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6,
                CAST(unixepoch(?7, 'subsec') * 1000 AS INTEGER),
                CAST(unixepoch(?8, 'subsec') * 1000 AS INTEGER),
                ?9, ?10, ?11, ?12
             ) ON CONFLICT(event_id) DO NOTHING",
            params![
                event.event_id,
                event.workspace_id,
                event.device_id,
                event.event_type,
                event.source,
                event.schema_version,
                event.occurred_at,
                event.created_at,
                sensitivity_name(event.sensitivity),
                payload_json,
                attachment_refs_json,
                event.idempotency_key,
            ],
        )
        .map_err(|error| DbError::sqlite("insert local event", error))?;
    #[cfg(feature = "process-test-hooks")]
    if let Some(hooks) = process_test_hooks {
        wait_at_process_test_barrier(hooks)?;
    }
    transaction
        .execute(
            "INSERT INTO sync_outbox (outbox_id, event_id, state, created_at_ms)
             VALUES (
                ?1, ?2, 'pending',
                CAST(unixepoch(?3, 'subsec') * 1000 AS INTEGER)
             ) ON CONFLICT DO NOTHING",
            params![outbox_id, event.event_id, event.created_at],
        )
        .map_err(|error| DbError::sqlite("insert event outbox", error))?;
    transaction
        .commit()
        .map_err(|error| DbError::sqlite("commit event transaction", error))
}

#[cfg(feature = "process-test-hooks")]
fn wait_at_process_test_barrier(hooks: &ProcessTestHooks) -> Result<(), DbError> {
    let mut ready = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&hooks.event_inserted_ready)
        .map_err(|error| DbError::sqlite("create process test barrier", error))?;
    ready
        .write_all(b"event-inserted\n")
        .and_then(|()| ready.sync_all())
        .map_err(|error| DbError::sqlite("publish process test barrier", error))?;

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match fs::symlink_metadata(&hooks.event_inserted_release) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                return Ok(())
            }
            Ok(_) => {
                return Err(DbError::sqlite(
                    "wait at process test barrier",
                    "release must be a regular file",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(DbError::sqlite("wait at process test barrier", error));
            }
        }
        if Instant::now() >= deadline {
            return Err(DbError::sqlite(
                "wait at process test barrier",
                "release was not observed within ten seconds",
            ));
        }
        thread::sleep(Duration::from_millis(5));
    }
}

pub(crate) fn set_agent_state(
    connection: &Connection,
    agent_status: AgentStatus,
    bridge_status: BridgeStatus,
    local_healthy: bool,
    updated_at_ms: i64,
) -> Result<(), DbError> {
    connection
        .execute(
            "INSERT INTO agent_state (
                singleton_id, agent_status, bridge_status, local_healthy, updated_at_ms
             ) VALUES (1, ?1, ?2, ?3, ?4)
             ON CONFLICT(singleton_id) DO UPDATE SET
                agent_status = excluded.agent_status,
                bridge_status = excluded.bridge_status,
                local_healthy = excluded.local_healthy,
                updated_at_ms = excluded.updated_at_ms",
            params![
                agent_status_name(agent_status),
                bridge_status_name(bridge_status),
                local_healthy,
                updated_at_ms,
            ],
        )
        .map(|_| ())
        .map_err(|error| DbError::sqlite("set agent state", error))
}

pub(crate) fn load_collector_states(
    connection: &Connection,
) -> Result<Vec<CollectorState>, DbError> {
    let mut statement = connection
        .prepare(
            "SELECT collector_key, status, version, desired_revision, applied_revision,
                    last_event_at_ms, last_health_at_ms, last_error_code,
                    created_at_ms, updated_at_ms
             FROM collector_states
             ORDER BY collector_key",
        )
        .map_err(|error| DbError::sqlite("prepare Collector state query", error))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, u64>(3)?,
                row.get::<_, u64>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, i64>(9)?,
            ))
        })
        .map_err(|error| DbError::sqlite("query Collector states", error))?;

    rows.map(|row| {
        let (
            collector_key,
            status,
            collector_version,
            desired_config_revision,
            applied_config_revision,
            last_event_at_ms,
            last_health_at_ms,
            last_error_code,
            created_at_ms,
            updated_at_ms,
        ) = row.map_err(|error| DbError::sqlite("read Collector state", error))?;
        Ok(CollectorState {
            collector_key,
            collector_version,
            status: collector_status(&status)?,
            desired_config_revision,
            applied_config_revision,
            last_event_at_ms,
            last_health_at_ms,
            last_error_code,
            created_at_ms,
            updated_at_ms,
        })
    })
    .collect()
}

pub(crate) fn upsert_collector_state_in(
    connection: &Connection,
    state: &CollectorState,
) -> Result<(), DbError> {
    connection
        .execute(
            "INSERT INTO collector_states (
                collector_key, status, version, desired_revision, applied_revision,
                last_event_at_ms, last_health_at_ms, last_error_code,
                created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(collector_key) DO UPDATE SET
                status = excluded.status,
                version = excluded.version,
                desired_revision = excluded.desired_revision,
                applied_revision = excluded.applied_revision,
                last_event_at_ms = excluded.last_event_at_ms,
                last_health_at_ms = excluded.last_health_at_ms,
                last_error_code = excluded.last_error_code,
                created_at_ms = excluded.created_at_ms,
                updated_at_ms = excluded.updated_at_ms",
            params![
                state.collector_key,
                collector_status_name(state.status),
                state.collector_version,
                state.desired_config_revision,
                state.applied_config_revision,
                state.last_event_at_ms,
                state.last_health_at_ms,
                state.last_error_code,
                state.created_at_ms,
                state.updated_at_ms,
            ],
        )
        .map(|_| ())
        .map_err(|error| DbError::sqlite("upsert Collector state", error))
}

pub(crate) fn count_event_and_outbox(
    connection: &Connection,
    event_id: &str,
) -> Result<(u64, u64), DbError> {
    connection
        .query_row(
            "SELECT
                (SELECT COUNT(*) FROM events_local WHERE event_id = ?1),
                (SELECT COUNT(*) FROM sync_outbox WHERE event_id = ?1)",
            [event_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|error| DbError::sqlite("count event and outbox", error))
}

pub(crate) fn health(connection: &Connection) -> Result<DbHealth, DbError> {
    let integrity_details = integrity_check(connection)?;
    if integrity_details.as_slice() != ["ok"] {
        return Err(DbError::IntegrityCheck {
            details: integrity_details,
        });
    }
    let foreign_key_details = foreign_key_check(connection)?;
    if !foreign_key_details.is_empty() {
        return Err(DbError::ForeignKeyCheck {
            details: foreign_key_details,
        });
    }
    let schema_version = connection
        .query_row(
            "SELECT COALESCE(MAX(CAST(id AS INTEGER)), 0)
             FROM schema_migrations WHERE status = 'completed'",
            [],
            |row| row.get(0),
        )
        .map_err(|error| DbError::startup_sqlite("read schema version", error))?;
    if schema_version > MAX_SUPPORTED_SCHEMA_VERSION {
        return Err(DbError::UnsupportedSchemaVersion {
            found: schema_version,
            max_supported: MAX_SUPPORTED_SCHEMA_VERSION,
        });
    }
    Ok(DbHealth {
        schema_version,
        integrity_ok: true,
        foreign_keys_ok: true,
    })
}

pub(crate) fn smoke_queries(connection: &Connection) -> Result<(), DbError> {
    for query in [
        "SELECT key, value FROM local_meta LIMIT 1",
        "SELECT singleton_id, agent_status, bridge_status, local_healthy, updated_at_ms
         FROM agent_state LIMIT 1",
        "SELECT collector_key, status, version, desired_revision, applied_revision,
                last_event_at_ms, last_health_at_ms, last_error_code,
                created_at_ms, updated_at_ms
         FROM collector_states LIMIT 1",
        "SELECT event_id, workspace_id, device_id, event_type, source, schema_version,
                occurred_at_ms, created_at_ms, sensitivity, payload_json,
                attachment_refs_json, idempotency_key
         FROM events_local LIMIT 1",
        "SELECT outbox_id, event_id, state, created_at_ms FROM sync_outbox LIMIT 1",
        "SELECT diagnostic_id, occurred_at_ms, level, code, redacted_json
         FROM diagnostic_events LIMIT 1",
    ] {
        let mut statement = connection
            .prepare(query)
            .map_err(|error| DbError::startup_sqlite("run database smoke query", error))?;
        statement
            .exists([])
            .map_err(|error| DbError::startup_sqlite("run database smoke query", error))?;
    }
    Ok(())
}

pub(crate) fn checkpoint(connection: &Connection) -> Result<(), DbError> {
    let (busy, _, _) = connection
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((
                row.get::<_, u32>(0)?,
                row.get::<_, u32>(1)?,
                row.get::<_, u32>(2)?,
            ))
        })
        .map_err(|error| DbError::sqlite("checkpoint WAL", error))?;
    if busy == 0 {
        Ok(())
    } else {
        Err(DbError::sqlite("checkpoint WAL", "database remained busy"))
    }
}

fn integrity_check(connection: &Connection) -> Result<Vec<String>, DbError> {
    let mut statement = connection
        .prepare("PRAGMA integrity_check")
        .map_err(|error| DbError::integrity_sqlite("prepare integrity check", error))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| DbError::integrity_sqlite("query integrity check", error))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| DbError::integrity_sqlite("read integrity check row", error))
}

fn foreign_key_check(connection: &Connection) -> Result<Vec<String>, DbError> {
    let mut statement = connection
        .prepare("PRAGMA foreign_key_check")
        .map_err(|error| DbError::ForeignKeyCheck {
            details: vec![error.to_string()],
        })?;
    let rows = statement
        .query_map([], |row| {
            Ok(format!(
                "table={} rowid={:?} parent={} foreign_key={}",
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?
            ))
        })
        .map_err(|error| DbError::ForeignKeyCheck {
            details: vec![error.to_string()],
        })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| DbError::ForeignKeyCheck {
            details: vec![error.to_string()],
        })
}

const fn sensitivity_name(sensitivity: Sensitivity) -> &'static str {
    match sensitivity {
        Sensitivity::Public => "public",
        Sensitivity::Normal => "normal",
        Sensitivity::Medium => "medium",
        Sensitivity::High => "high",
        Sensitivity::Secret => "secret",
    }
}

const fn collector_status_name(status: CollectorStatus) -> &'static str {
    match status {
        CollectorStatus::Disabled => "disabled",
        CollectorStatus::PermissionRequired => "permission_required",
        CollectorStatus::Initializing => "initializing",
        CollectorStatus::Running => "running",
        CollectorStatus::Paused => "paused",
        CollectorStatus::Degraded => "degraded",
        CollectorStatus::Unsupported => "unsupported",
        CollectorStatus::Error => "error",
    }
}

fn collector_status(status: &str) -> Result<CollectorStatus, DbError> {
    match status {
        "disabled" => Ok(CollectorStatus::Disabled),
        "permission_required" => Ok(CollectorStatus::PermissionRequired),
        "initializing" => Ok(CollectorStatus::Initializing),
        "running" => Ok(CollectorStatus::Running),
        "paused" => Ok(CollectorStatus::Paused),
        "degraded" => Ok(CollectorStatus::Degraded),
        "unsupported" => Ok(CollectorStatus::Unsupported),
        "error" => Ok(CollectorStatus::Error),
        _ => Err(DbError::sqlite(
            "decode Collector state",
            format!("unknown Collector status {status}"),
        )),
    }
}

const fn agent_status_name(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Unpaired => "unpaired",
        AgentStatus::Initializing => "initializing",
        AgentStatus::WaitingPermission => "waiting_permission",
        AgentStatus::Running => "running",
        AgentStatus::Degraded => "degraded",
        AgentStatus::Sleeping => "sleeping",
        AgentStatus::Updating => "updating",
        AgentStatus::Repair => "repair",
        AgentStatus::Stopped => "stopped",
    }
}

const fn bridge_status_name(status: BridgeStatus) -> &'static str {
    match status {
        BridgeStatus::Disconnected => "disconnected",
        BridgeStatus::Handshaking => "handshaking",
        BridgeStatus::Ready => "ready",
        BridgeStatus::Degraded => "degraded",
        BridgeStatus::Incompatible => "incompatible",
        BridgeStatus::Stopped => "stopped",
    }
}
