use std::{any::Any, fmt};

/// Errors returned by the local durable store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbError {
    /// `SQLite` rejected an operation.
    Sqlite {
        operation: &'static str,
        message: String,
        retryable: bool,
    },
    /// Event JSON could not be serialized.
    Serialization(String),
    /// One communication source key resolved to different immutable message content.
    CommunicationSourceConflict,
    /// The private communication spool root or file could not be opened safely.
    CommunicationSpoolUnavailable,
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
    pub(crate) fn sqlite<E>(operation: &'static str, error: E) -> Self
    where
        E: fmt::Display + 'static,
    {
        if let Some(sqlite_error) = (&error as &dyn Any).downcast_ref::<rusqlite::Error>() {
            if matches!(
                sqlite_error.sqlite_error_code(),
                Some(
                    rusqlite::ffi::ErrorCode::DatabaseCorrupt
                        | rusqlite::ffi::ErrorCode::NotADatabase
                )
            ) {
                return Self::IntegrityCheck {
                    details: vec![format!("{operation}: {sqlite_error}")],
                };
            }
            return Self::Sqlite {
                operation,
                message: sqlite_error.to_string(),
                retryable: sqlite_error_is_retryable(sqlite_error),
            };
        }
        if let Some(io_error) = (&error as &dyn Any).downcast_ref::<std::io::Error>() {
            return Self::Sqlite {
                operation,
                message: io_error.to_string(),
                retryable: matches!(
                    io_error.kind(),
                    std::io::ErrorKind::Interrupted
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                ),
            };
        }
        Self::Sqlite {
            operation,
            message: error.to_string(),
            retryable: false,
        }
    }

    pub(crate) fn retryable(operation: &'static str, error: impl fmt::Display) -> Self {
        Self::Sqlite {
            operation,
            message: error.to_string(),
            retryable: true,
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

    /// Returns whether repeating the same operation can plausibly succeed without replacing the
    /// database owner or repairing durable data.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Sqlite {
                retryable: true,
                ..
            }
        )
    }
}

fn sqlite_error_is_retryable(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(
            rusqlite::ffi::ErrorCode::DatabaseBusy
                | rusqlite::ffi::ErrorCode::DatabaseLocked
                | rusqlite::ffi::ErrorCode::OperationInterrupted
                | rusqlite::ffi::ErrorCode::SystemIoFailure
                | rusqlite::ffi::ErrorCode::DiskFull
                | rusqlite::ffi::ErrorCode::CannotOpen
                | rusqlite::ffi::ErrorCode::FileLockingProtocolFailed
                | rusqlite::ffi::ErrorCode::SchemaChanged
        )
    )
}

impl fmt::Display for DbError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite {
                operation, message, ..
            } => write!(formatter, "{operation}: {message}"),
            Self::Serialization(message) => write!(formatter, "event serialization: {message}"),
            Self::CommunicationSourceConflict => formatter.write_str(
                "communication source key conflicts with different immutable message content",
            ),
            Self::CommunicationSpoolUnavailable => {
                formatter.write_str("private communication spool is unavailable")
            }
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
            &mapped,
            DbError::Sqlite {
                operation: "run integrity check",
                ..
            }
        ));
        assert!(mapped.is_retryable());
    }

    #[test]
    fn corrupt_and_constraint_sqlite_errors_are_terminal() {
        let corrupt = DbError::sqlite(
            "commit event transaction",
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
                Some("secret corrupt page".to_owned()),
            ),
        );
        let constraint = DbError::sqlite(
            "commit event transaction",
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
                Some("secret constraint detail".to_owned()),
            ),
        );

        assert!(matches!(corrupt, DbError::IntegrityCheck { .. }));
        assert!(!corrupt.is_retryable());
        assert!(matches!(constraint, DbError::Sqlite { .. }));
        assert!(!constraint.is_retryable());
        assert!(!DbError::sqlite("validate event", "immutable conflict").is_retryable());
        assert!(DbError::retryable("checkpoint WAL", "database remained busy").is_retryable());
    }
}
