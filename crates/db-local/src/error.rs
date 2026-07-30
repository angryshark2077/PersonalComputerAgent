use std::fmt;

/// Errors returned by the local durable store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbError {
    /// `SQLite` rejected an operation.
    Sqlite {
        operation: &'static str,
        message: String,
    },
    /// Event JSON could not be serialized.
    Serialization(String),
    /// A recorded immutable migration differs from the bundled migration.
    MigrationChecksumMismatch { id: String },
    /// A recorded migration is not in the completed state.
    IncompleteMigration { id: String, status: String },
    /// The database contains a schema newer than this binary supports.
    UnsupportedSchemaVersion { found: u32, max_supported: u32 },
    /// A migration identifier is not a four-digit number.
    InvalidMigrationId(String),
    /// `SQLite` integrity checks did not return `ok`.
    IntegrityCheck { details: Vec<String> },
    /// `SQLite` reported foreign-key violations.
    ForeignKeyCheck { details: Vec<String> },
    /// The database owner thread is no longer accepting work.
    ActorUnavailable,
    /// The database owner thread panicked during initialization or shutdown.
    ActorThreadPanic,
}

impl DbError {
    pub(crate) fn sqlite(operation: &'static str, error: impl fmt::Display) -> Self {
        Self::Sqlite {
            operation,
            message: error.to_string(),
        }
    }

    pub(crate) fn startup_sqlite(operation: &'static str, error: rusqlite::Error) -> Self {
        Self::integrity_sqlite(operation, error)
    }

    pub(crate) fn integrity_sqlite(operation: &'static str, error: rusqlite::Error) -> Self {
        if matches!(
            error.sqlite_error_code(),
            Some(
                rusqlite::ffi::ErrorCode::DatabaseCorrupt | rusqlite::ffi::ErrorCode::NotADatabase
            )
        ) {
            Self::IntegrityCheck {
                details: vec![format!("{operation}: {error}")],
            }
        } else {
            Self::sqlite(operation, error)
        }
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite { operation, message } => write!(formatter, "{operation}: {message}"),
            Self::Serialization(message) => write!(formatter, "event serialization: {message}"),
            Self::MigrationChecksumMismatch { id } => {
                write!(formatter, "migration {id} checksum mismatch")
            }
            Self::IncompleteMigration { id, status } => {
                write!(formatter, "migration {id} has incomplete status {status}")
            }
            Self::UnsupportedSchemaVersion {
                found,
                max_supported,
            } => write!(
                formatter,
                "database schema {found} exceeds supported version {max_supported}"
            ),
            Self::InvalidMigrationId(id) => write!(formatter, "invalid migration id: {id}"),
            Self::IntegrityCheck { details } => {
                write!(
                    formatter,
                    "database integrity check failed: {}",
                    details.join("; ")
                )
            }
            Self::ForeignKeyCheck { details } => write!(
                formatter,
                "database foreign-key check failed: {}",
                details.join("; ")
            ),
            Self::ActorUnavailable => formatter.write_str("database actor unavailable"),
            Self::ActorThreadPanic => formatter.write_str("database actor thread panicked"),
        }
    }
}

impl std::error::Error for DbError {}

#[cfg(test)]
mod tests {
    use super::DbError;

    #[test]
    fn ordinary_integrity_path_sqlite_error_stays_sqlite() {
        let error = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some("database is busy".to_owned()),
        );

        let mapped = DbError::integrity_sqlite("run integrity check", error);

        assert!(matches!(
            mapped,
            DbError::Sqlite {
                operation: "run integrity check",
                ..
            }
        ));
    }
}
