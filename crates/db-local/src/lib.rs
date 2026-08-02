//! Immutable migrations and a single-owner async facade for local `SQLite`.

#![forbid(unsafe_code)]

#[cfg(all(feature = "process-test-hooks", not(debug_assertions)))]
compile_error!("process-test-hooks cannot be compiled into a release build");

mod actor;
mod error;
mod migrations;
mod repository;

pub use actor::DbActorHandle;
#[cfg(feature = "process-test-hooks")]
pub use actor::ProcessTestHooks;
pub use error::DbError;
use pca_domain::{CommunicationMessageRecorded, EventEnvelope};

/// The initial local database migration.
pub const BASELINE_MIGRATION: &str = include_str!("../migrations/0000_baseline.sql");
/// The immutable S1A runtime database migration.
pub const S1A_RUNTIME_MIGRATION: &str = include_str!("../migrations/0001_s1a_runtime.sql");
/// The immutable S2 Collector-state database migration.
pub const S2_COLLECTOR_STATE_MIGRATION: &str =
    include_str!("../migrations/0002_s2_collector_state.sql");
/// The immutable S1B pairing-state database migration.
pub const S1B_PAIRING_STATE_MIGRATION: &str =
    include_str!("../migrations/0003_s1b_pairing_state.sql");
/// The immutable S1B Cloud API origin migration.
pub const S1B_CLOUD_API_ORIGIN_MIGRATION: &str =
    include_str!("../migrations/0004_s1b_cloud_api_origin.sql");
/// The immutable S3 communication-message local storage migration.
pub const WECHAT_MESSAGES_MIGRATION: &str = include_str!("../migrations/0005_wechat_messages.sql");
/// The immutable Task 4 fix migration for deterministic attachment spool names.
pub const HARDEN_ATTACHMENT_SPOOL_MIGRATION: &str =
    include_str!("../migrations/0006_harden_attachment_spool.sql");
/// Expands the verified small-group limit while preserving existing communication rows.
pub const EXPAND_GROUP_LIMIT_MIGRATION: &str =
    include_str!("../migrations/0007_expand_group_limit.sql");
/// Records verified Cloud completion time for seven-day local media cleanup.
pub const ATTACHMENT_COMPLETION_RETENTION_MIGRATION: &str =
    include_str!("../migrations/0008_attachment_completion_retention.sql");
/// Allows different `WeChat` message kinds to use the same conversation-local source sequence.
pub const ALLOW_MESSAGE_KIND_SEQUENCE_OVERLAP_MIGRATION: &str =
    include_str!("../migrations/0009_allow_message_kind_sequence_overlap.sql");

/// A private spool-file reference corresponding to one validated media manifest.
///
/// This deliberately carries a path without implementing `Debug`, so ordinary diagnostics cannot
/// emit private local file names.
#[derive(Clone, PartialEq, Eq)]
pub struct CommunicationAttachmentSpoolReference {
    pub attachment_id: String,
    /// Fixed lower-case SHA-256 filename, stored directly below the private spool root.
    pub file_name: String,
}

/// One acknowledged communication attachment ready for a bounded Cloud upload attempt.
///
/// The byte body deliberately has no `Debug` implementation so diagnostics cannot print media.
pub struct PendingCommunicationAttachment {
    pub event_id: String,
    pub attachment_id: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub mime_type: String,
    pub bytes: Vec<u8>,
}

/// The complete local atomic-write input for one eligible communication message.
///
/// The caller must provide a canonical event envelope and source sequence.  The local store
/// validates the immutable event/message correspondence and all spool references before commit.
/// This deliberately carries message content and has no `Debug` implementation.
#[derive(Clone, PartialEq)]
pub struct CommunicationMessageCommit {
    pub account_id: String,
    pub source_sequence: u64,
    pub event: EventEnvelope,
    pub message: CommunicationMessageRecorded,
    pub attachment_spool: Vec<CommunicationAttachmentSpoolReference>,
}

/// Non-secret local pointer to an Agent credential validated in Keychain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingState {
    /// Cloud-assigned device identifier.
    pub device_id: String,
    /// Cloud Workspace that owns the device.
    pub workspace_id: String,
    /// Keychain reference; never credential material.
    pub credential_ref: String,
    /// Current server-side credential generation.
    pub credential_generation: u64,
    /// Non-secret HTTPS Cloud API origin used for authenticated control after restart.
    pub cloud_api_origin: String,
    /// Highest complete control revision applied locally.
    pub applied_control_revision: u64,
    /// Time the validated credential reference was saved, in Unix milliseconds.
    pub paired_at_ms: i64,
}

impl PairingState {
    /// Builds state after the caller has validated the referenced Keychain credential.
    #[must_use]
    pub fn paired(
        device_id: impl Into<String>,
        workspace_id: impl Into<String>,
        credential_ref: impl Into<String>,
        credential_generation: u64,
        cloud_api_origin: impl Into<String>,
    ) -> Self {
        let elapsed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let paired_at_ms = i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX);
        Self {
            device_id: device_id.into(),
            workspace_id: workspace_id.into(),
            credential_ref: credential_ref.into(),
            credential_generation,
            cloud_api_origin: cloud_api_origin.into(),
            applied_control_revision: 0,
            paired_at_ms,
        }
    }
}

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
    use super::{
        ALLOW_MESSAGE_KIND_SEQUENCE_OVERLAP_MIGRATION, BASELINE_MIGRATION,
        HARDEN_ATTACHMENT_SPOOL_MIGRATION, S1A_RUNTIME_MIGRATION, S1B_CLOUD_API_ORIGIN_MIGRATION,
        S1B_PAIRING_STATE_MIGRATION, S2_COLLECTOR_STATE_MIGRATION, WECHAT_MESSAGES_MIGRATION,
    };

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

    #[test]
    fn s2_collector_state_migration_has_only_the_required_table() {
        assert_eq!(
            S2_COLLECTOR_STATE_MIGRATION.matches("CREATE TABLE").count(),
            1
        );
        assert!(!S2_COLLECTOR_STATE_MIGRATION.contains("CREATE INDEX"));
    }

    #[test]
    fn s1b_pairing_state_migration_has_only_the_non_secret_singleton() {
        assert_eq!(
            S1B_PAIRING_STATE_MIGRATION.matches("CREATE TABLE").count(),
            1
        );
        assert!(!S1B_PAIRING_STATE_MIGRATION.contains("CREATE INDEX"));
        assert!(!S1B_PAIRING_STATE_MIGRATION.contains("token"));
        assert!(!S1B_PAIRING_STATE_MIGRATION.contains("secret"));
    }

    #[test]
    fn s1b_cloud_origin_migration_has_no_secret_columns() {
        assert!(S1B_CLOUD_API_ORIGIN_MIGRATION.contains("cloud_api_origin"));
        assert!(!S1B_CLOUD_API_ORIGIN_MIGRATION.contains("token"));
        assert!(!S1B_CLOUD_API_ORIGIN_MIGRATION.contains("secret"));
    }

    #[test]
    fn wechat_message_migration_keeps_content_in_private_local_tables() {
        assert!(WECHAT_MESSAGES_MIGRATION.contains("communication_messages"));
        assert!(WECHAT_MESSAGES_MIGRATION.contains("attachment_spool"));
        assert!(!WECHAT_MESSAGES_MIGRATION.contains("key_material"));
        assert!(!WECHAT_MESSAGES_MIGRATION.contains("credential"));
    }

    #[test]
    fn attachment_spool_fix_requires_the_sha256_filename() {
        assert!(HARDEN_ATTACHMENT_SPOOL_MIGRATION.contains("spool_relative_path <> NEW.sha256"));
    }

    #[test]
    fn message_kind_sequence_overlap_keeps_source_key_as_the_idempotency_boundary() {
        assert!(!ALLOW_MESSAGE_KIND_SEQUENCE_OVERLAP_MIGRATION
            .contains("UNIQUE (account_id, external_conversation_id, source_sequence)"));
        assert!(ALLOW_MESSAGE_KIND_SEQUENCE_OVERLAP_MIGRATION
            .contains("UNIQUE (account_id, source_key)"));
    }
}
