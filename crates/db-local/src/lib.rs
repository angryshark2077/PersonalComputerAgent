//! Immutable migrations and a single-owner async facade for local `SQLite`.

#![forbid(unsafe_code)]

mod actor;
mod error;
mod migrations;
mod repository;

pub use actor::DbActorHandle;
pub use error::DbError;

/// The initial local database migration.
pub const BASELINE_MIGRATION: &str = include_str!("../migrations/0000_baseline.sql");
/// The immutable S1A runtime database migration.
pub const S1A_RUNTIME_MIGRATION: &str = include_str!("../migrations/0001_s1a_runtime.sql");

/// Results of fresh `SQLite` health checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DbHealth {
    /// Highest completed migration understood by this binary.
    pub schema_version: u32,
    /// Whether `PRAGMA integrity_check` returned `ok`.
    pub integrity_ok: bool,
    /// Whether `PRAGMA foreign_key_check` returned no rows.
    pub foreign_keys_ok: bool,
}

#[cfg(test)]
mod tests {
    use super::{BASELINE_MIGRATION, S1A_RUNTIME_MIGRATION};

    #[test]
    fn baseline_creates_only_the_migration_ledger() {
        assert!(BASELINE_MIGRATION.contains("CREATE TABLE IF NOT EXISTS schema_migrations"));
        assert_eq!(BASELINE_MIGRATION.matches("CREATE TABLE").count(), 1);
    }

    #[test]
    fn s1a_runtime_migration_has_only_the_required_tables() {
        assert_eq!(S1A_RUNTIME_MIGRATION.matches("CREATE TABLE").count(), 5);
        assert_eq!(S1A_RUNTIME_MIGRATION.matches("CREATE INDEX").count(), 2);
    }
}
