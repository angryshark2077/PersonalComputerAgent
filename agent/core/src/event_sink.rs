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
        .map_err(map_database_error)
}

fn map_database_error(_error: DbError) -> DomainError {
    DomainError::new(
        "COLLECTOR_DEGRADED",
        "collector persistence unavailable",
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::{commit_with_deadline, map_database_error};
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
    fn every_database_error_maps_to_one_redacted_retryable_error() {
        let variants = [
            DbError::ActorUnavailable,
            DbError::Serialization("secret database detail".to_owned()),
            DbError::UnsupportedSchemaVersion {
                found: 99,
                max_supported: 2,
            },
        ];

        for database_error in variants {
            let mapped = map_database_error(database_error);
            assert_eq!(mapped.code, "COLLECTOR_DEGRADED");
            assert_eq!(mapped.message, "collector persistence unavailable");
            assert!(mapped.retryable);
            assert!(!mapped.message.contains("secret"));
            assert!(!mapped.message.contains("99"));
        }
    }
}
