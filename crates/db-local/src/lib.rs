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
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};

/// Physical local-media usage grouped by whether every database reference is Cloud-completed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CommunicationMediaStorageStats {
    pub completed_file_count: u64,
    pub completed_bytes: u64,
    pub protected_file_count: u64,
    pub protected_bytes: u64,
}

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
/// Records verified Cloud completion time for safe manual local media cleanup.
pub const ATTACHMENT_COMPLETION_RETENTION_MIGRATION: &str =
    include_str!("../migrations/0008_attachment_completion_retention.sql");
/// Allows different `WeChat` message kinds to use the same conversation-local source sequence.
pub const ALLOW_MESSAGE_KIND_SEQUENCE_OVERLAP_MIGRATION: &str =
    include_str!("../migrations/0009_allow_message_kind_sequence_overlap.sql");
/// Adds file messages and file attachment spool rows without rewriting existing data.
pub const ADD_FILE_MESSAGES_MIGRATION: &str =
    include_str!("../migrations/0010_add_file_messages.sql");
/// Repairs unsynced Apple Messages rows created with the wrong Cloud idempotency key.
pub const REPAIR_APPLE_MESSAGE_IDEMPOTENCY_MIGRATION: &str =
    include_str!("../migrations/0011_repair_apple_message_idempotency.sql");
/// Normalizes unsynced Apple Messages payload timestamps to the local event millisecond boundary.
pub const NORMALIZE_APPLE_MESSAGE_TIMESTAMPS_MIGRATION: &str =
    include_str!("../migrations/0012_normalize_apple_message_timestamps.sql");
/// Makes photo Event, Outbox, and private upload-task persistence atomic.
pub const PHOTO_UPLOAD_SPOOL_MIGRATION: &str =
    include_str!("../migrations/0013_photo_upload_spool.sql");
/// Separates terminal local-media corruption from retryable Cloud transfer failures.
pub const TERMINAL_MEDIA_FAILURES_MIGRATION: &str =
    include_str!("../migrations/0014_terminal_media_failures.sql");
/// Persists the Owner's explicit unpair decision independently from Keychain availability.
pub const MANUAL_UNPAIR_STATE_MIGRATION: &str =
    include_str!("../migrations/0015_manual_unpair_state.sql");
/// Persists complete local `WeChat` and Screenshot control for offline recovery.
///
/// Upgrades from older schemas may temporarily contain a legacy `WeChat` and scheduled Screenshot
/// bootstrap at revision zero until the first complete Cloud control snapshot is applied.
pub const APPLIED_COLLECTOR_CONTROL_MIGRATION: &str =
    include_str!("../migrations/0016_applied_collector_control.sql");

/// Non-secret local Collector control that remains authoritative while Cloud is unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "the persisted contract has fixed independent Collector switches"
)]
pub struct AppliedCollectorControl {
    pub device_id: String,
    pub workspace_id: String,
    pub configuration_revision: u64,
    pub communication_wechat_enabled: bool,
    pub screen_capture_enabled: bool,
    pub screen_capture_scheduled_enabled: bool,
    pub screen_capture_interval_seconds: u64,
    pub screen_capture_activity_enabled: bool,
    pub screen_capture_activity_min_interval_seconds: u64,
    pub screen_capture_excluded_bundle_ids: Vec<String>,
    pub updated_at_ms: i64,
}

impl AppliedCollectorControl {
    /// Revision zero is a migration-only bootstrap containing only the legacy `WeChat` enable bit.
    #[must_use]
    pub const fn is_legacy_bootstrap(&self) -> bool {
        self.configuration_revision == 0
    }
}

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
/// The validated file handle deliberately has no `Debug` implementation so diagnostics cannot
/// print media or private local paths. Callers stream from a cloned handle instead of retaining
/// the complete attachment in memory.
pub struct PendingCommunicationAttachment {
    pub event_id: String,
    pub source: String,
    pub attachment_id: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub mime_type: String,
    file: std::fs::File,
}

impl PendingCommunicationAttachment {
    pub(crate) fn verify_body(mut self) -> Result<Self, DbError> {
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|error| DbError::sqlite("rewind pending attachment body", error))?;
        let mut hasher = Sha256::new();
        let mut bytes_read = 0_u64;
        let mut buffer = vec![0_u8; 1024 * 1024];
        loop {
            let read = self
                .file
                .read(&mut buffer)
                .map_err(|error| DbError::sqlite("read pending attachment body", error))?;
            if read == 0 {
                break;
            }
            bytes_read = bytes_read
                .checked_add(u64::try_from(read).map_err(|_| {
                    DbError::sqlite("read pending attachment", "attachment size is invalid")
                })?)
                .ok_or_else(|| {
                    DbError::sqlite("read pending attachment", "attachment size is invalid")
                })?;
            hasher.update(&buffer[..read]);
        }
        if bytes_read != self.size_bytes || format!("{:x}", hasher.finalize()) != self.sha256 {
            return Err(DbError::sqlite(
                "verify pending attachment body",
                "attachment body does not match immutable manifest",
            ));
        }
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|error| DbError::sqlite("rewind pending attachment body", error))?;
        Ok(self)
    }

    /// Clones the already-open, immutable spool file for one upload attempt.
    ///
    /// # Errors
    ///
    /// Returns the operating-system error when the file descriptor cannot be duplicated.
    pub fn try_clone_file(&self) -> std::io::Result<std::fs::File> {
        let mut file = self.file.try_clone()?;
        file.seek(SeekFrom::Start(0))?;
        Ok(file)
    }
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
    pub metadata_events: Vec<EventEnvelope>,
    pub message: CommunicationMessageRecorded,
    pub attachment_spool: Vec<CommunicationAttachmentSpoolReference>,
}

/// The private upload-task manifest committed with one photo Event and its Outbox intent.
///
/// `manifest_json` intentionally has no `Debug` implementation because it includes photo
/// metadata. The media body remains in the private `PhotoSpool` directory.
#[derive(Clone, PartialEq)]
pub struct PhotoUploadCommit {
    pub event: EventEnvelope,
    pub photo_id: String,
    pub manifest_json: String,
}

/// One pending private photo upload task. The caller deserializes the manifest only when it is
/// ready to make a bounded Cloud upload attempt.
pub struct PendingPhotoUpload {
    pub photo_id: String,
    pub manifest_json: String,
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
    /// Whether the Owner explicitly revoked this pairing.
    pub manually_unpaired: bool,
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
            manually_unpaired: false,
        }
    }

    /// Returns whether this durable record still represents an active pairing.
    #[must_use]
    pub const fn is_paired(&self) -> bool {
        !self.manually_unpaired
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
        ALLOW_MESSAGE_KIND_SEQUENCE_OVERLAP_MIGRATION, APPLIED_COLLECTOR_CONTROL_MIGRATION,
        BASELINE_MIGRATION, HARDEN_ATTACHMENT_SPOOL_MIGRATION, MANUAL_UNPAIR_STATE_MIGRATION,
        PHOTO_UPLOAD_SPOOL_MIGRATION, S1A_RUNTIME_MIGRATION, S1B_CLOUD_API_ORIGIN_MIGRATION,
        S1B_PAIRING_STATE_MIGRATION, S2_COLLECTOR_STATE_MIGRATION,
        TERMINAL_MEDIA_FAILURES_MIGRATION, WECHAT_MESSAGES_MIGRATION,
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

    #[test]
    fn photo_upload_spool_is_private_and_event_bound() {
        assert!(PHOTO_UPLOAD_SPOOL_MIGRATION.contains("photo_upload_spool"));
        assert!(PHOTO_UPLOAD_SPOOL_MIGRATION.contains("FOREIGN KEY (event_id)"));
        assert!(PHOTO_UPLOAD_SPOOL_MIGRATION.contains("json_valid(manifest_json)"));
    }

    #[test]
    fn terminal_media_failures_are_preserved_without_remaining_retryable() {
        assert!(TERMINAL_MEDIA_FAILURES_MIGRATION.contains("terminal_failure_code"));
        assert!(TERMINAL_MEDIA_FAILURES_MIGRATION.contains("MEDIA_LOCAL_BODY_INVALID"));
        assert!(TERMINAL_MEDIA_FAILURES_MIGRATION.contains("PHOTOS_LOCAL_MANIFEST_INVALID"));
        assert!(!TERMINAL_MEDIA_FAILURES_MIGRATION.contains("DELETE"));
    }

    #[test]
    fn manual_unpair_state_is_a_checked_boolean() {
        assert!(MANUAL_UNPAIR_STATE_MIGRATION.contains("manually_unpaired"));
        assert!(MANUAL_UNPAIR_STATE_MIGRATION.contains("IN (0, 1)"));
        assert!(!MANUAL_UNPAIR_STATE_MIGRATION.contains("DELETE"));
    }

    #[test]
    fn applied_control_migration_contains_no_credential_or_one_shot_request() {
        assert_eq!(
            APPLIED_COLLECTOR_CONTROL_MIGRATION
                .matches("CREATE TABLE")
                .count(),
            1
        );
        assert!(!APPLIED_COLLECTOR_CONTROL_MIGRATION.contains("token"));
        assert!(!APPLIED_COLLECTOR_CONTROL_MIGRATION.contains("secret"));
        assert!(!APPLIED_COLLECTOR_CONTROL_MIGRATION.contains("request_id"));
    }
}
