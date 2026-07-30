//! Immutable local database migration assets.

/// The initial local database migration.
pub const BASELINE_MIGRATION: &str = include_str!("../migrations/0000_baseline.sql");

#[cfg(test)]
mod tests {
    use super::BASELINE_MIGRATION;

    #[test]
    fn baseline_creates_only_the_migration_ledger() {
        assert!(BASELINE_MIGRATION.contains("CREATE TABLE IF NOT EXISTS schema_migrations"));
        assert_eq!(BASELINE_MIGRATION.matches("CREATE TABLE").count(), 1);
    }
}
