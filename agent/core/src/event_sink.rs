use pca_db_local::{DbActorHandle, DbError};
use pca_domain::{DomainError, EventCommit, EventSink, EventSinkFuture};
use std::{future::Future, sync::Arc, time::Duration};

pub(crate) struct DbEventSink {
    database: Arc<DbActorHandle>,
}

impl DbEventSink {
    pub(crate) fn new(database: Arc<DbActorHandle>) -> Self {
        Self { database }
    }
}

impl EventSink for DbEventSink {
    fn commit(&self, commit: EventCommit) -> EventSinkFuture<'_> {
        Box::pin(async move { commit_with_deadline(self.database.commit_events(&commit)).await })
    }
}

async fn commit_with_deadline<F>(future: F) -> Result<(), DomainError>
where
    F: Future<Output = Result<(), DbError>>,
{
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .map_err(|_| DomainError::new("COLLECTOR_TIMEOUT", "event commit timed out", true))?
        .map_err(|error| map_database_error(&error))
}

fn map_database_error(error: &DbError) -> DomainError {
    eprintln!(
        "pca-agentd: collector persistence unavailable kind={}",
        database_error_kind(error)
    );
    DomainError::new(
        "COLLECTOR_DEGRADED",
        "collector persistence unavailable",
        error.is_retryable(),
    )
}

fn database_error_kind(error: &DbError) -> &'static str {
    match error {
        DbError::Sqlite { .. } => "sqlite",
        DbError::Serialization(_) => "serialization",
        DbError::CommunicationSourceConflict => "communication_source_conflict",
        DbError::CommunicationSpoolUnavailable => "communication_spool_unavailable",
        DbError::MigrationChecksumMismatch { .. } => "migration_checksum",
        DbError::IncompleteMigration { .. } => "incomplete_migration",
        DbError::UnsupportedSchemaVersion { .. } => "unsupported_schema",
        DbError::InvalidMigrationId(_) => "invalid_migration_id",
        DbError::IntegrityCheck { .. } => "integrity_check",
        DbError::ForeignKeyCheck { .. } => "foreign_key_check",
        DbError::ActorUnavailable => "actor_unavailable",
        DbError::ActorThreadPanic => "actor_thread_panic",
    }
}

#[cfg(test)]
mod tests {
    use super::{commit_with_deadline, database_error_kind, map_database_error};
    use pca_db_local::DbError;
    use std::{future::pending, time::Duration};

    #[tokio::test(start_paused = true)]
    async fn commit_deadline_is_exactly_five_seconds() {
        let started = tokio::time::Instant::now();

        let error = commit_with_deadline(pending::<Result<(), DbError>>())
            .await
            .expect_err("pending commit must time out");

        assert_eq!(
            tokio::time::Instant::now() - started,
            Duration::from_secs(5)
        );
        assert_eq!(error.code, "COLLECTOR_TIMEOUT");
        assert_eq!(error.message, "event commit timed out");
        assert!(error.retryable);
    }

    #[test]
    fn database_errors_keep_redaction_without_retrying_permanent_failures() {
        let variants = [
            (DbError::ActorUnavailable, "actor_unavailable", false),
            (
                DbError::Serialization("secret database detail".to_owned()),
                "serialization",
                false,
            ),
            (
                DbError::UnsupportedSchemaVersion {
                    found: 99,
                    max_supported: 2,
                },
                "unsupported_schema",
                false,
            ),
            (
                DbError::IntegrityCheck {
                    details: vec!["secret corrupt page".to_owned()],
                },
                "integrity_check",
                false,
            ),
        ];

        for (database_error, expected_kind, expected_retryable) in variants {
            assert_eq!(database_error_kind(&database_error), expected_kind);
            let mapped = map_database_error(&database_error);
            assert_eq!(mapped.code, "COLLECTOR_DEGRADED");
            assert_eq!(mapped.message, "collector persistence unavailable");
            assert_eq!(mapped.retryable, expected_retryable);
            assert!(!mapped.message.contains("secret"));
            assert!(!mapped.message.contains("99"));
        }
    }
}
