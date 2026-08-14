use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::error::DbError;

pub(crate) const MAX_SUPPORTED_SCHEMA_VERSION: u32 = 13;

struct Migration {
    id: &'static str,
    sql: &'static str,
}

const MIGRATIONS: [Migration; 14] = [
    Migration {
        id: "0000",
        sql: crate::BASELINE_MIGRATION,
    },
    Migration {
        id: "0001",
        sql: crate::S1A_RUNTIME_MIGRATION,
    },
    Migration {
        id: "0002",
        sql: crate::S2_COLLECTOR_STATE_MIGRATION,
    },
    Migration {
        id: "0003",
        sql: crate::S1B_PAIRING_STATE_MIGRATION,
    },
    Migration {
        id: "0004",
        sql: crate::S1B_CLOUD_API_ORIGIN_MIGRATION,
    },
    Migration {
        id: "0005",
        sql: crate::WECHAT_MESSAGES_MIGRATION,
    },
    Migration {
        id: "0006",
        sql: crate::HARDEN_ATTACHMENT_SPOOL_MIGRATION,
    },
    Migration {
        id: "0007",
        sql: crate::EXPAND_GROUP_LIMIT_MIGRATION,
    },
    Migration {
        id: "0008",
        sql: crate::ATTACHMENT_COMPLETION_RETENTION_MIGRATION,
    },
    Migration {
        id: "0009",
        sql: crate::ALLOW_MESSAGE_KIND_SEQUENCE_OVERLAP_MIGRATION,
    },
    Migration {
        id: "0010",
        sql: crate::ADD_FILE_MESSAGES_MIGRATION,
    },
    Migration {
        id: "0011",
        sql: crate::REPAIR_APPLE_MESSAGE_IDEMPOTENCY_MIGRATION,
    },
    Migration {
        id: "0012",
        sql: crate::NORMALIZE_APPLE_MESSAGE_TIMESTAMPS_MIGRATION,
    },
    Migration {
        id: "0013",
        sql: crate::PHOTO_UPLOAD_SPOOL_MIGRATION,
    },
];

pub(crate) fn run(connection: &mut Connection, app_version: &str) -> Result<(), DbError> {
    if migration_ledger_exists(connection)? {
        reject_future_schema(connection)?;
    }

    for migration in &MIGRATIONS {
        apply(connection, migration, app_version)?;
    }
    Ok(())
}

fn migration_ledger_exists(connection: &Connection) -> Result<bool, DbError> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'schema_migrations')",
            [],
            |row| row.get(0),
        )
        .map_err(|error| DbError::startup_sqlite("inspect migration ledger", error))
}

fn reject_future_schema(connection: &Connection) -> Result<(), DbError> {
    let mut statement = connection
        .prepare("SELECT id FROM schema_migrations")
        .map_err(|error| DbError::startup_sqlite("read migration identifiers", error))?;
    let identifiers = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| DbError::startup_sqlite("query migration identifiers", error))?;

    for identifier in identifiers {
        let identifier = identifier
            .map_err(|error| DbError::startup_sqlite("decode migration identifier", error))?;
        if identifier.len() != 4 || !identifier.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(DbError::InvalidMigrationId(identifier));
        }
        let version = identifier
            .parse::<u32>()
            .map_err(|_| DbError::InvalidMigrationId(identifier.clone()))?;
        if version > MAX_SUPPORTED_SCHEMA_VERSION {
            return Err(DbError::UnsupportedSchemaVersion {
                found: version,
                max_supported: MAX_SUPPORTED_SCHEMA_VERSION,
            });
        }
    }
    Ok(())
}

fn apply(
    connection: &mut Connection,
    migration: &Migration,
    app_version: &str,
) -> Result<(), DbError> {
    let checksum = checksum(migration.sql.as_bytes());
    if migration_ledger_exists(connection)? {
        let recorded = connection
            .query_row(
                "SELECT checksum, status FROM schema_migrations WHERE id = ?1",
                [migration.id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(|error| DbError::startup_sqlite("read migration record", error))?;
        if let Some((recorded_checksum, status)) = recorded {
            if recorded_checksum != checksum {
                return Err(DbError::MigrationChecksumMismatch {
                    id: migration.id.to_owned(),
                });
            }
            if status != "completed" {
                return Err(DbError::IncompleteMigration {
                    id: migration.id.to_owned(),
                    status,
                });
            }
            return Ok(());
        }
    }

    let now = now_ms()?;
    let transaction = connection
        .transaction()
        .map_err(|error| DbError::startup_sqlite("start migration transaction", error))?;
    transaction
        .execute_batch(migration.sql)
        .map_err(|error| DbError::startup_sqlite("execute migration", error))?;
    transaction
        .execute(
            "INSERT INTO schema_migrations \
             (id, checksum, app_version, started_at, completed_at, status) \
             VALUES (?1, ?2, ?3, ?4, ?4, 'completed')",
            params![migration.id, checksum, app_version, now],
        )
        .map_err(|error| DbError::startup_sqlite("record migration", error))?;
    transaction
        .commit()
        .map_err(|error| DbError::startup_sqlite("commit migration", error))
}

fn checksum(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn now_ms() -> Result<i64, DbError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| DbError::sqlite("read system clock", error))?;
    i64::try_from(duration.as_millis())
        .map_err(|error| DbError::sqlite("convert system clock", error))
}
