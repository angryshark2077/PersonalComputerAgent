use std::{collections::HashSet, fs::File, os::unix::fs::MetadataExt, path::Path};

use pca_domain::{
    AgentStatus, BridgeStatus, CollectorState, CollectorStatus, CommunicationAttachment,
    ConversationScope, Direction, EventCommit, EventEnvelope, MessageKind, Sensitivity,
};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use sha2::{Digest, Sha256};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use uuid::Uuid;

#[cfg(feature = "process-test-hooks")]
use std::{
    fs::{self, OpenOptions},
    io::Write,
    thread,
    time::{Duration, Instant},
};

#[cfg(feature = "process-test-hooks")]
use crate::actor::{ProcessTestBarrier, ProcessTestHooks};
use crate::{
    migrations::MAX_SUPPORTED_SCHEMA_VERSION, AppliedCollectorControl, CommunicationMessageCommit,
    DbError, DbHealth, PairingState, PendingCommunicationAttachment, PendingPhotoUpload,
    PhotoUploadCommit,
};

struct SerializedEvent<'a> {
    event: &'a EventEnvelope,
    payload_json: String,
    attachment_refs_json: String,
    outbox_id: String,
}

struct ValidatedSpoolReference<'a> {
    attachment: &'a CommunicationAttachment,
}

pub(crate) fn append_event_with_outbox(
    connection: &mut Connection,
    event: &EventEnvelope,
    #[cfg(feature = "process-test-hooks")] process_test_hooks: Option<&ProcessTestHooks>,
) -> Result<(), DbError> {
    let serialized = serialize_event(event)?;
    let transaction = connection
        .transaction()
        .map_err(|error| DbError::sqlite("start event transaction", error))?;
    insert_event(&transaction, &serialized)?;
    #[cfg(feature = "process-test-hooks")]
    if let Some(hooks) = process_test_hooks {
        if let Some(barrier) = hooks.event_outbox.as_ref() {
            wait_at_process_test_barrier(barrier, b"event-inserted\n")?;
        }
    }
    insert_stable_outbox(&transaction, &serialized)?;
    transaction
        .commit()
        .map_err(|error| DbError::sqlite("commit event transaction", error))
}

pub(crate) fn commit_events(
    connection: &mut Connection,
    commit: &EventCommit,
    #[cfg(feature = "process-test-hooks")] process_test_hooks: Option<&ProcessTestHooks>,
) -> Result<(), DbError> {
    let serialized = commit
        .events()
        .iter()
        .map(serialize_event)
        .collect::<Result<Vec<_>, _>>()?;
    let transaction = connection
        .transaction()
        .map_err(|error| DbError::sqlite("start Collector transaction", error))?;
    for event in &serialized {
        insert_event(&transaction, event)?;
        insert_stable_outbox(&transaction, event)?;
    }
    #[cfg(feature = "process-test-hooks")]
    if commit.collector_state().is_some() {
        if let Some(barrier) = process_test_hooks.and_then(|hooks| hooks.collector_commit.as_ref())
        {
            wait_at_process_test_barrier(barrier, b"collector-event-outbox-inserted\n")?;
        }
    }
    if let Some(state) = commit.collector_state() {
        upsert_collector_state_in(&transaction, state)?;
    }
    transaction
        .commit()
        .map_err(|error| DbError::sqlite("commit Collector transaction", error))
}

pub(crate) fn commit_communication_message(
    connection: &mut Connection,
    spool_root: &Path,
    commit: &CommunicationMessageCommit,
) -> Result<(), DbError> {
    validate_communication_commit(commit)?;
    let serialized = serialize_event(&commit.event)?;
    let metadata = commit
        .metadata_events
        .iter()
        .map(serialize_event)
        .collect::<Result<Vec<_>, _>>()?;
    let spool_references = validate_spool_references(spool_root, commit)?;
    let source_sequence = i64::try_from(commit.source_sequence).map_err(|_| {
        DbError::sqlite(
            "validate communication source sequence",
            "source sequence exceeds SQLite integer range",
        )
    })?;
    let cursor_sequence = i64::try_from(commit.cursor_sequence).map_err(|_| {
        DbError::sqlite(
            "validate communication cursor sequence",
            "cursor sequence exceeds SQLite integer range",
        )
    })?;

    let transaction = connection
        .transaction()
        .map_err(|error| DbError::sqlite("start communication message transaction", error))?;
    match existing_communication_message(&transaction, commit, &serialized)? {
        ExistingCommunicationMessage::Absent => {}
        ExistingCommunicationMessage::SameEvent => {
            validate_existing_communication_event(&transaction, &serialized)?;
            validate_existing_outbox(&transaction, &serialized)?;
            validate_existing_attachment_spool(&transaction, commit)?;
            for event in &metadata {
                insert_event(&transaction, event)?;
                insert_stable_outbox(&transaction, event)?;
            }
            update_communication_message_cursor(&transaction, commit, cursor_sequence)?;
            advance_communication_cursor(&transaction, commit, cursor_sequence)?;
            return transaction.commit().map_err(|error| {
                DbError::sqlite("commit communication metadata transaction", error)
            });
        }
        ExistingCommunicationMessage::RepairedDeviceReplay => {
            validate_existing_attachment_spool(&transaction, commit)?;
            update_communication_message_cursor(&transaction, commit, cursor_sequence)?;
            advance_communication_cursor(&transaction, commit, cursor_sequence)?;
            return transaction.commit().map_err(|error| {
                DbError::sqlite("commit repaired-device replay transaction", error)
            });
        }
    }

    insert_event(&transaction, &serialized)?;
    for event in &metadata {
        insert_event(&transaction, event)?;
        insert_stable_outbox(&transaction, event)?;
    }
    upsert_communication_conversation(&transaction, commit)?;
    let local_message_id = insert_communication_message(&transaction, commit, source_sequence)?;
    for spool_reference in &spool_references {
        insert_attachment_spool(&transaction, local_message_id, spool_reference)?;
    }
    insert_stable_outbox(&transaction, &serialized)?;
    advance_communication_cursor(&transaction, commit, cursor_sequence)?;
    transaction
        .commit()
        .map_err(|error| DbError::sqlite("commit communication message transaction", error))
}

pub(crate) fn consume_communication_source_conflict(
    connection: &mut Connection,
    commit: &CommunicationMessageCommit,
) -> Result<(), DbError> {
    let cursor_sequence = i64::try_from(commit.cursor_sequence).map_err(|_| {
        DbError::sqlite(
            "consume communication source conflict",
            "source sequence exceeds SQLite integer range",
        )
    })?;
    let transaction = connection
        .transaction()
        .map_err(|error| DbError::sqlite("begin communication conflict transaction", error))?;
    let source_key = commit.message.source_key();
    let source_key_hash = format!("{:x}", Sha256::digest(source_key.as_bytes()));
    let occurred_at_ms = i64::try_from(
        OffsetDateTime::now_utc()
            .unix_timestamp_nanos()
            .div_euclid(1_000_000),
    )
    .unwrap_or(i64::MAX);
    let code = "COMMUNICATION_SOURCE_IDENTITY_CONFLICT";
    let diagnostic_id = format!("{code}:{source_key_hash}");
    let redacted_json = serde_json::json!({
        "collector": "communication.wechat",
        "stage": "local_persistence",
        "category": "immutable_source_conflict",
        "source_key_hash": source_key_hash,
    })
    .to_string();
    transaction
        .execute(
            "INSERT INTO diagnostic_events (
                diagnostic_id, occurred_at_ms, level, code, redacted_json
             ) VALUES (?1, ?2, 'error', ?3, ?4)
             ON CONFLICT(diagnostic_id) DO UPDATE SET
                occurred_at_ms = excluded.occurred_at_ms,
                level = excluded.level,
                code = excluded.code,
                redacted_json = excluded.redacted_json",
            params![diagnostic_id, occurred_at_ms, code, redacted_json],
        )
        .map_err(|error| DbError::sqlite("record communication source conflict", error))?;
    advance_communication_cursor(&transaction, commit, cursor_sequence)?;
    transaction
        .commit()
        .map_err(|error| DbError::sqlite("commit communication conflict transaction", error))
}

pub(crate) fn record_apple_message_invalid_record(
    connection: &Connection,
    source_key: &str,
) -> Result<(), DbError> {
    let source_key_hash = format!("{:x}", Sha256::digest(source_key.as_bytes()));
    let occurred_at_ms = i64::try_from(
        OffsetDateTime::now_utc()
            .unix_timestamp_nanos()
            .div_euclid(1_000_000),
    )
    .unwrap_or(i64::MAX);
    let code = "APPLE_MESSAGE_INVALID_RECORD";
    let diagnostic_id = format!("{code}:{source_key_hash}");
    let redacted_json = serde_json::json!({
        "collector": "communication.messages",
        "stage": "source_validation",
        "category": "invalid_record",
        "source_key_hash": source_key_hash,
    })
    .to_string();
    connection
        .execute(
            "INSERT INTO diagnostic_events (
                diagnostic_id, occurred_at_ms, level, code, redacted_json
             ) VALUES (?1, ?2, 'error', ?3, ?4)
             ON CONFLICT(diagnostic_id) DO UPDATE SET
                occurred_at_ms = excluded.occurred_at_ms,
                level = excluded.level,
                code = excluded.code,
                redacted_json = excluded.redacted_json",
            params![diagnostic_id, occurred_at_ms, code, redacted_json],
        )
        .map(|_| ())
        .map_err(|error| DbError::sqlite("record Apple Messages invalid record", error))
}

pub(crate) fn commit_photo_upload(
    connection: &mut Connection,
    commit: &PhotoUploadCommit,
) -> Result<(), DbError> {
    validate_photo_upload_commit(commit)?;
    let serialized = serialize_event(&commit.event)?;
    let transaction = connection
        .transaction()
        .map_err(|error| DbError::sqlite("start photo upload transaction", error))?;
    insert_event(&transaction, &serialized)?;
    insert_stable_outbox(&transaction, &serialized)?;
    let created_at_ms = i64::try_from(
        OffsetDateTime::now_utc()
            .unix_timestamp_nanos()
            .div_euclid(1_000_000),
    )
    .map_err(|_| DbError::sqlite("read photo upload timestamp", "out of range"))?;
    let inserted = transaction
        .execute(
            "INSERT INTO photo_upload_spool (
                photo_id, event_id, manifest_json, transfer_state, created_at_ms
             ) VALUES (?1, ?2, ?3, 'pending', ?4)
             ON CONFLICT(photo_id) DO NOTHING",
            params![
                commit.photo_id,
                commit.event.event_id,
                commit.manifest_json,
                created_at_ms,
            ],
        )
        .map_err(|error| DbError::sqlite("insert photo upload task", error))?;
    if inserted == 0 {
        let identical = transaction
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM photo_upload_spool
                    WHERE photo_id = ?1
                      AND event_id = ?2
                      AND manifest_json = ?3
                )",
                params![commit.photo_id, commit.event.event_id, commit.manifest_json],
                |row| row.get::<_, bool>(0),
            )
            .map_err(|error| DbError::sqlite("validate existing photo upload task", error))?;
        if !identical {
            return Err(DbError::sqlite(
                "validate existing photo upload task",
                "photo ID conflicts with different immutable upload data",
            ));
        }
    }
    transaction
        .commit()
        .map_err(|error| DbError::sqlite("commit photo upload transaction", error))
}

fn validate_photo_upload_commit(commit: &PhotoUploadCommit) -> Result<(), DbError> {
    if !is_uuid_like(&commit.photo_id)
        || commit.event.event_id.trim().is_empty()
        || commit.event.event_type != "photos.asset_recorded"
        || commit.event.source != "photos.library"
        || commit.event.schema_version != 1
        || commit.event.sensitivity != Sensitivity::High
        || commit.event.attachment_refs != Vec::<String>::new()
    {
        return Err(DbError::sqlite(
            "validate photo upload commit",
            "event does not match the fixed photo upload contract",
        ));
    }
    let manifest = serde_json::from_str::<serde_json::Value>(&commit.manifest_json)
        .map_err(|error| DbError::Serialization(error.to_string()))?;
    if manifest.get("photo_id").and_then(serde_json::Value::as_str)
        != Some(commit.photo_id.as_str())
        || manifest.get("event_id").and_then(serde_json::Value::as_str)
            != Some(commit.event.event_id.as_str())
    {
        return Err(DbError::sqlite(
            "validate photo upload commit",
            "manifest does not match the immutable photo event identity",
        ));
    }
    Ok(())
}

fn is_uuid_like(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            matches!(index, 8 | 13 | 18 | 23) && byte == b'-' || byte.is_ascii_hexdigit()
        })
}

pub(crate) fn photo_upload_exists(
    connection: &Connection,
    photo_id: &str,
) -> Result<bool, DbError> {
    if !is_uuid_like(photo_id) {
        return Err(DbError::sqlite(
            "check photo upload task",
            "photo ID is not a UUID",
        ));
    }
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM photo_upload_spool WHERE photo_id = ?1)",
            [photo_id],
            |row| row.get(0),
        )
        .map_err(|error| DbError::sqlite("check photo upload task", error))
}

pub(crate) fn load_pending_photo_uploads(
    connection: &Connection,
    limit: u16,
    workspace_id: &str,
    device_id: &str,
) -> Result<Vec<PendingPhotoUpload>, DbError> {
    let mut statement = connection
        .prepare(
            "SELECT p.photo_id, p.manifest_json
             FROM photo_upload_spool AS p
             INNER JOIN events_local AS e ON e.event_id = p.event_id
             WHERE p.transfer_state = 'pending'
               AND p.terminal_failure_code IS NULL
               AND e.workspace_id = ?1
               AND e.device_id = ?2
             ORDER BY p.created_at_ms, p.photo_id
             LIMIT ?3",
        )
        .map_err(|error| DbError::sqlite("prepare pending photo upload query", error))?;
    let rows = statement
        .query_map(params![workspace_id, device_id, i64::from(limit)], |row| {
            Ok(PendingPhotoUpload {
                photo_id: row.get(0)?,
                manifest_json: row.get(1)?,
            })
        })
        .map_err(|error| DbError::sqlite("query pending photo uploads", error))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| DbError::sqlite("read pending photo upload", error))
}

pub(crate) fn complete_photo_upload(
    connection: &Connection,
    photo_id: &str,
) -> Result<(), DbError> {
    if !is_uuid_like(photo_id) {
        return Err(DbError::sqlite(
            "complete photo upload",
            "photo ID is not a UUID",
        ));
    }
    let updated = connection
        .execute(
            "UPDATE photo_upload_spool
             SET transfer_state = 'completed',
                 completed_at_ms = CAST(unixepoch('subsec') * 1000 AS INTEGER)
             WHERE photo_id = ?1
               AND transfer_state = 'pending'
               AND terminal_failure_code IS NULL",
            [photo_id],
        )
        .map_err(|error| DbError::sqlite("complete photo upload", error))?;
    if updated == 1 {
        Ok(())
    } else {
        Err(DbError::sqlite(
            "complete photo upload",
            "photo upload is not pending",
        ))
    }
}

pub(crate) fn quarantine_invalid_photo_upload(
    connection: &Connection,
    photo_id: &str,
) -> Result<(), DbError> {
    if !is_uuid_like(photo_id) {
        return Err(DbError::sqlite(
            "quarantine invalid photo upload",
            "photo ID is not a UUID",
        ));
    }
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| DbError::sqlite("start invalid photo quarantine", error))?;
    let occurred_at_ms = i64::try_from(
        OffsetDateTime::now_utc()
            .unix_timestamp_nanos()
            .div_euclid(1_000_000),
    )
    .unwrap_or(i64::MAX);
    let updated = transaction
        .execute(
            "UPDATE photo_upload_spool
             SET terminal_failure_code = 'PHOTOS_LOCAL_MANIFEST_INVALID'
             WHERE photo_id = ?1
               AND transfer_state = 'pending'
               AND terminal_failure_code IS NULL",
            [photo_id],
        )
        .map_err(|error| DbError::sqlite("quarantine invalid photo upload", error))?;
    if updated != 1 {
        return Err(DbError::sqlite(
            "quarantine invalid photo upload",
            "photo upload is not retryable",
        ));
    }
    record_media_diagnostic(
        &transaction,
        photo_id,
        "PHOTOS_LOCAL_MANIFEST_INVALID",
        "local_validation",
        "contract",
        None,
        occurred_at_ms,
    )?;
    persist_terminal_media_collector_failure(
        &transaction,
        "photos.library",
        "PHOTOS_LOCAL_MANIFEST_INVALID",
        occurred_at_ms,
    )?;
    transaction
        .commit()
        .map_err(|error| DbError::sqlite("commit invalid photo quarantine", error))
}

fn validate_communication_commit(commit: &CommunicationMessageCommit) -> Result<(), DbError> {
    if commit.account_id.trim().is_empty()
        || commit.event.event_id.trim().is_empty()
        || commit.event.event_type != "communication.message_recorded"
        || !matches!(
            commit.event.source.as_str(),
            "communication.wechat" | "communication.messages"
        )
        || commit.event.schema_version != 1
        || commit.event.sensitivity != Sensitivity::High
        || commit.event.occurred_at != commit.message.occurred_at()
        || commit.event.idempotency_key.as_deref() != Some(commit.message.source_key())
    {
        return Err(DbError::sqlite(
            "validate communication message commit",
            "event does not match the fixed communication message contract",
        ));
    }
    if commit.metadata_events.iter().any(|event| {
        event.workspace_id != commit.event.workspace_id
            || event.device_id != commit.event.device_id
            || event.source != commit.event.source
            || event.schema_version != 1
            || event.sensitivity != Sensitivity::High
            || !matches!(
                event.event_type.as_str(),
                "communication.conversation_observed" | "communication.message_sender_observed"
            )
    }) {
        return Err(DbError::sqlite(
            "validate communication metadata commit",
            "metadata events do not match the communication message identity",
        ));
    }

    let expected_payload = serde_json::to_value(&commit.message)
        .map_err(|error| DbError::Serialization(error.to_string()))?;
    if expected_payload.as_object() != Some(&commit.event.payload) {
        return Err(DbError::sqlite(
            "validate communication message commit",
            "event payload does not exactly match the communication message",
        ));
    }

    let expected_attachment_ids = commit
        .message
        .attachments()
        .iter()
        .map(|attachment| attachment.attachment_id().to_owned())
        .collect::<Vec<_>>();
    if commit.event.attachment_refs != expected_attachment_ids {
        return Err(DbError::sqlite(
            "validate communication message commit",
            "event attachment references do not exactly match the message manifests",
        ));
    }
    Ok(())
}

fn validate_spool_references<'a>(
    spool_root: &Path,
    commit: &'a CommunicationMessageCommit,
) -> Result<Vec<ValidatedSpoolReference<'a>>, DbError> {
    if commit.message.attachments().len() != commit.attachment_spool.len() {
        return Err(DbError::sqlite(
            "validate communication spool references",
            "every media manifest must have exactly one private spool reference",
        ));
    }

    let mut remaining = commit
        .message
        .attachments()
        .iter()
        .map(|attachment| (attachment.attachment_id(), attachment))
        .collect::<std::collections::HashMap<_, _>>();
    if remaining.len() != commit.message.attachments().len() {
        return Err(DbError::sqlite(
            "validate communication spool references",
            "message contains duplicate attachment manifest identifiers",
        ));
    }

    let mut seen_attachment_ids = HashSet::new();
    let mut validated = Vec::with_capacity(commit.attachment_spool.len());
    for reference in &commit.attachment_spool {
        if !seen_attachment_ids.insert(&reference.attachment_id) {
            return Err(DbError::sqlite(
                "validate communication spool references",
                "attachment spool reference is duplicated",
            ));
        }
        let attachment = remaining
            .remove(reference.attachment_id.as_str())
            .ok_or_else(|| {
                DbError::sqlite(
                    "validate communication spool references",
                    "attachment spool reference has no matching manifest",
                )
            })?;
        if !is_lowercase_sha256(attachment.sha256()) || attachment.size_bytes() == 0 {
            return Err(DbError::sqlite(
                "validate communication spool references",
                "attachment manifest has an invalid hash or byte length",
            ));
        }

        if reference.file_name != attachment.sha256() {
            return Err(DbError::sqlite(
                "validate communication spool path",
                "attachment spool filename must equal its manifest SHA-256",
            ));
        }
        let _file = open_communication_spool_file(spool_root, &reference.file_name)?;
        validated.push(ValidatedSpoolReference { attachment });
    }
    if !remaining.is_empty() {
        return Err(DbError::sqlite(
            "validate communication spool references",
            "message has a media manifest without a private spool reference",
        ));
    }
    Ok(validated)
}

enum ExistingCommunicationMessage {
    Absent,
    SameEvent,
    RepairedDeviceReplay,
}

fn existing_communication_message(
    transaction: &Transaction<'_>,
    commit: &CommunicationMessageCommit,
    serialized: &SerializedEvent<'_>,
) -> Result<ExistingCommunicationMessage, DbError> {
    let existing = transaction
        .query_row(
            "SELECT event_id, external_conversation_id, source_sequence
             FROM communication_messages
             WHERE account_id = ?1 AND source_key = ?2",
            params![commit.account_id, commit.message.source_key()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|error| DbError::sqlite("read existing communication message", error))?;
    let Some((event_id, conversation_id, source_sequence)) = existing else {
        return Ok(ExistingCommunicationMessage::Absent);
    };
    if conversation_id != commit.message.conversation_id()
        || u64::try_from(source_sequence).ok() != Some(commit.source_sequence)
    {
        return Err(DbError::CommunicationSourceConflict);
    }
    if event_id == commit.event.event_id {
        return Ok(ExistingCommunicationMessage::SameEvent);
    }
    validate_repaired_device_replay(transaction, &event_id, serialized)?;
    Ok(ExistingCommunicationMessage::RepairedDeviceReplay)
}

fn validate_repaired_device_replay(
    transaction: &Transaction<'_>,
    existing_event_id: &str,
    serialized: &SerializedEvent<'_>,
) -> Result<(), DbError> {
    let event = serialized.event;
    let existing_payload = transaction
        .query_row(
            "SELECT payload_json FROM events_local
             WHERE event_id = ?1
               AND workspace_id = ?2
               AND device_id <> ?3
               AND event_type = ?4
               AND source = ?5
               AND schema_version = ?6
               AND occurred_at_ms = CAST(unixepoch(?7, 'subsec') * 1000 AS INTEGER)
               AND sensitivity = ?8
               AND attachment_refs_json = ?9
               AND idempotency_key IS ?10",
            params![
                existing_event_id,
                event.workspace_id,
                event.device_id,
                event.event_type,
                event.source,
                event.schema_version,
                event.occurred_at,
                sensitivity_name(event.sensitivity),
                serialized.attachment_refs_json,
                event.idempotency_key,
            ],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| DbError::sqlite("validate repaired-device replay", error))?
        .ok_or(DbError::CommunicationSourceConflict)?;
    if communication_payload_matches(&existing_payload, &event.payload)? {
        Ok(())
    } else {
        Err(DbError::CommunicationSourceConflict)
    }
}

fn validate_existing_attachment_spool(
    transaction: &Transaction<'_>,
    commit: &CommunicationMessageCommit,
) -> Result<(), DbError> {
    let mut statement = transaction
        .prepare(
            "SELECT s.attachment_id, s.kind, s.sha256, s.size_bytes, s.mime_type,
                    s.spool_relative_path
             FROM attachment_spool AS s
             INNER JOIN communication_messages AS m ON m.local_message_id = s.local_message_id
             WHERE m.account_id = ?1 AND m.source_key = ?2",
        )
        .map_err(|error| DbError::sqlite("prepare existing attachment spool query", error))?;
    let rows = statement
        .query_map(
            params![commit.account_id, commit.message.source_key()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            },
        )
        .map_err(|error| DbError::sqlite("query existing attachment spool", error))?;
    let persisted = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| DbError::sqlite("read existing attachment spool", error))?;
    if persisted.len() != commit.attachment_spool.len() {
        return Err(DbError::CommunicationSourceConflict);
    }

    for reference in &commit.attachment_spool {
        let attachment = commit
            .message
            .attachments()
            .iter()
            .find(|attachment| attachment.attachment_id() == reference.attachment_id)
            .ok_or_else(|| {
                DbError::sqlite(
                    "validate existing attachment spool",
                    "attachment spool reference has no matching manifest",
                )
            })?;
        let expected_size = i64::try_from(attachment.size_bytes()).map_err(|_| {
            DbError::sqlite(
                "validate existing attachment spool",
                "attachment byte length exceeds SQLite integer range",
            )
        })?;
        let matches = persisted.iter().any(
            |(attachment_id, kind, sha256, size_bytes, mime_type, spool_relative_path)| {
                attachment_id == &reference.attachment_id
                    && kind == message_kind_name(attachment.kind())
                    && sha256 == attachment.sha256()
                    && *size_bytes == expected_size
                    && mime_type == attachment.mime_type()
                    && spool_relative_path == &reference.file_name
            },
        );
        if !matches {
            return Err(DbError::CommunicationSourceConflict);
        }
    }
    Ok(())
}

fn upsert_communication_conversation(
    transaction: &Transaction<'_>,
    commit: &CommunicationMessageCommit,
) -> Result<(), DbError> {
    let (scope, member_count) = communication_scope(commit.message.conversation());
    transaction
        .execute(
            "INSERT INTO communication_conversations (
                account_id, external_conversation_id, scope, member_count, created_at_ms, updated_at_ms
             ) VALUES (
                ?1, ?2, ?3, ?4,
                CAST(unixepoch(?5, 'subsec') * 1000 AS INTEGER),
                CAST(unixepoch(?5, 'subsec') * 1000 AS INTEGER)
             ) ON CONFLICT(account_id, external_conversation_id) DO UPDATE SET
                scope = excluded.scope,
                member_count = excluded.member_count,
                updated_at_ms = excluded.updated_at_ms",
            params![
                commit.account_id,
                commit.message.conversation_id(),
                scope,
                member_count,
                commit.event.created_at,
            ],
        )
        .map(|_| ())
        .map_err(|error| DbError::sqlite("upsert communication conversation", error))
}

fn insert_communication_message(
    transaction: &Transaction<'_>,
    commit: &CommunicationMessageCommit,
    source_sequence: i64,
) -> Result<i64, DbError> {
    let cursor_sequence = i64::try_from(commit.cursor_sequence).map_err(|_| {
        DbError::sqlite(
            "insert communication message",
            "cursor sequence exceeds SQLite integer range",
        )
    })?;
    transaction
        .execute(
            "INSERT INTO communication_messages (
                event_id, account_id, external_conversation_id, source_sequence, cursor_sequence, source_key,
                direction, kind, occurred_at_ms, text_body, created_at_ms
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                CAST(unixepoch(?9, 'subsec') * 1000 AS INTEGER), ?10,
                CAST(unixepoch(?11, 'subsec') * 1000 AS INTEGER)
             )",
            params![
                commit.event.event_id,
                commit.account_id,
                commit.message.conversation_id(),
                source_sequence,
                cursor_sequence,
                commit.message.source_key(),
                direction_name(commit.message.direction()),
                message_kind_name(commit.message.kind()),
                commit.message.occurred_at(),
                commit.message.text(),
                commit.event.created_at,
            ],
        )
        .map_err(|error| DbError::sqlite("insert communication message", error))?;
    Ok(transaction.last_insert_rowid())
}

fn update_communication_message_cursor(
    transaction: &Transaction<'_>,
    commit: &CommunicationMessageCommit,
    cursor_sequence: i64,
) -> Result<(), DbError> {
    transaction
        .execute(
            "UPDATE communication_messages
             SET cursor_sequence = MAX(COALESCE(cursor_sequence, 0), ?3)
             WHERE account_id = ?1 AND source_key = ?2",
            params![
                commit.account_id,
                commit.message.source_key(),
                cursor_sequence,
            ],
        )
        .map(|_| ())
        .map_err(|error| DbError::sqlite("update communication message cursor", error))
}

fn insert_attachment_spool(
    transaction: &Transaction<'_>,
    local_message_id: i64,
    reference: &ValidatedSpoolReference<'_>,
) -> Result<(), DbError> {
    let size_bytes = i64::try_from(reference.attachment.size_bytes()).map_err(|_| {
        DbError::sqlite(
            "insert attachment spool",
            "attachment byte length exceeds SQLite integer range",
        )
    })?;
    transaction
        .execute(
            "INSERT INTO attachment_spool (
                attachment_id, local_message_id, kind, sha256, size_bytes, mime_type,
                spool_relative_path, transfer_state, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending',
                CAST(unixepoch('now', 'subsec') * 1000 AS INTEGER))",
            params![
                reference.attachment.attachment_id(),
                local_message_id,
                message_kind_name(reference.attachment.kind()),
                reference.attachment.sha256(),
                size_bytes,
                reference.attachment.mime_type(),
                reference.attachment.sha256(),
            ],
        )
        .map(|_| ())
        .map_err(|error| DbError::sqlite("insert attachment spool", error))
}

pub(crate) fn open_communication_spool_file(
    spool_root: &Path,
    file_name: &str,
) -> Result<File, DbError> {
    validate_spool_file_name(file_name)?;
    let directory = rustix::fs::open(
        spool_root,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map(File::from)
    .map_err(|_| DbError::CommunicationSpoolUnavailable)?;
    let root_metadata = directory
        .metadata()
        .map_err(|_| DbError::CommunicationSpoolUnavailable)?;
    if !root_metadata.is_dir() || root_metadata.mode() & 0o077 != 0 {
        return Err(DbError::CommunicationSpoolUnavailable);
    }

    let file = rustix::fs::openat(
        &directory,
        file_name,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map(File::from)
    .map_err(|_| DbError::CommunicationSpoolUnavailable)?;
    if !file
        .metadata()
        .map_err(|_| DbError::CommunicationSpoolUnavailable)?
        .is_file()
    {
        return Err(DbError::CommunicationSpoolUnavailable);
    }
    Ok(file)
}

fn validate_spool_file_name(file_name: &str) -> Result<(), DbError> {
    if is_lowercase_sha256(file_name) {
        Ok(())
    } else {
        Err(DbError::sqlite(
            "validate communication spool path",
            "attachment spool filename must be one lowercase SHA-256 component",
        ))
    }
}

fn advance_communication_cursor(
    transaction: &Transaction<'_>,
    commit: &CommunicationMessageCommit,
    source_sequence: i64,
) -> Result<(), DbError> {
    transaction
        .execute(
            "INSERT INTO communication_cursors (
                account_id, external_conversation_id, last_source_sequence, updated_at_ms
             ) VALUES (?1, ?2, ?3, CAST(unixepoch(?4, 'subsec') * 1000 AS INTEGER))
             ON CONFLICT(account_id, external_conversation_id) DO UPDATE SET
                last_source_sequence = MAX(last_source_sequence, excluded.last_source_sequence),
                updated_at_ms = excluded.updated_at_ms",
            params![
                commit.account_id,
                commit.message.conversation_id(),
                source_sequence,
                commit.event.created_at,
            ],
        )
        .map(|_| ())
        .map_err(|error| DbError::sqlite("advance communication cursor", error))
}

fn communication_scope(scope: &ConversationScope) -> (&'static str, Option<u8>) {
    match scope {
        ConversationScope::Direct => ("direct", None),
        ConversationScope::Group { member_count } => ("group", Some(*member_count)),
    }
}

const fn direction_name(direction: Direction) -> &'static str {
    match direction {
        Direction::Incoming => "incoming",
        Direction::Outgoing => "outgoing",
    }
}

const fn message_kind_name(kind: MessageKind) -> &'static str {
    match kind {
        MessageKind::Text => "text",
        MessageKind::Audio => "audio",
        MessageKind::Image => "image",
        MessageKind::Video => "video",
        MessageKind::File => "file",
    }
}

fn is_lowercase_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn serialize_event(event: &EventEnvelope) -> Result<SerializedEvent<'_>, DbError> {
    let payload_json = serde_json::to_string(&event.payload)
        .map_err(|error| DbError::Serialization(error.to_string()))?;
    let attachment_refs_json = serde_json::to_string(&event.attachment_refs)
        .map_err(|error| DbError::Serialization(error.to_string()))?;
    Ok(SerializedEvent {
        event,
        payload_json,
        attachment_refs_json,
        outbox_id: format!("event:{}", event.event_id),
    })
}

fn insert_event(
    transaction: &Transaction<'_>,
    serialized: &SerializedEvent<'_>,
) -> Result<(), DbError> {
    let event = serialized.event;
    let inserted = transaction
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
                serialized.payload_json,
                serialized.attachment_refs_json,
                event.idempotency_key,
            ],
        )
        .map_err(|error| DbError::sqlite("insert local event", error))?;
    if inserted == 0 {
        validate_existing_event(transaction, serialized)?;
    }
    Ok(())
}

fn validate_existing_event(
    transaction: &Transaction<'_>,
    serialized: &SerializedEvent<'_>,
) -> Result<(), DbError> {
    let event = serialized.event;
    let identical = transaction
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM events_local
                WHERE event_id = ?1
                  AND workspace_id = ?2
                  AND device_id = ?3
                  AND event_type = ?4
                  AND source = ?5
                  AND schema_version = ?6
                  AND occurred_at_ms =
                      CAST(unixepoch(?7, 'subsec') * 1000 AS INTEGER)
                  AND created_at_ms =
                      CAST(unixepoch(?8, 'subsec') * 1000 AS INTEGER)
                  AND sensitivity = ?9
                  AND payload_json = ?10
                  AND attachment_refs_json = ?11
                  AND idempotency_key IS ?12
            )",
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
                serialized.payload_json,
                serialized.attachment_refs_json,
                event.idempotency_key,
            ],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|error| DbError::sqlite("validate existing local event", error))?;
    if identical {
        Ok(())
    } else {
        Err(DbError::sqlite(
            "validate existing local event",
            format!(
                "event ID {} conflicts with different immutable fields",
                event.event_id
            ),
        ))
    }
}

fn validate_existing_communication_event(
    transaction: &Transaction<'_>,
    serialized: &SerializedEvent<'_>,
) -> Result<(), DbError> {
    let event = serialized.event;
    let existing_payload = transaction
        .query_row(
            "SELECT payload_json FROM events_local
             WHERE event_id = ?1
               AND workspace_id = ?2
               AND device_id = ?3
               AND event_type = ?4
               AND source = ?5
               AND schema_version = ?6
               AND occurred_at_ms = CAST(unixepoch(?7, 'subsec') * 1000 AS INTEGER)
               AND created_at_ms = CAST(unixepoch(?8, 'subsec') * 1000 AS INTEGER)
               AND sensitivity = ?9
               AND attachment_refs_json = ?10
               AND idempotency_key IS ?11",
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
                serialized.attachment_refs_json,
                event.idempotency_key,
            ],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| DbError::sqlite("validate existing communication event", error))?
        .ok_or(DbError::CommunicationSourceConflict)?;
    if communication_payload_matches(&existing_payload, &serialized.event.payload)? {
        Ok(())
    } else {
        Err(DbError::CommunicationSourceConflict)
    }
}

fn communication_payload_matches(
    existing_payload: &str,
    candidate_payload: &serde_json::Map<String, serde_json::Value>,
) -> Result<bool, DbError> {
    let mut existing_payload =
        serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(existing_payload)
            .map_err(|error| DbError::Serialization(error.to_string()))?;
    let mut candidate_payload = candidate_payload.clone();
    for field in ["sender_id", "sender_display_name"] {
        existing_payload.remove(field);
        candidate_payload.remove(field);
    }
    Ok(existing_payload == candidate_payload)
}

fn insert_stable_outbox(
    transaction: &Transaction<'_>,
    serialized: &SerializedEvent<'_>,
) -> Result<(), DbError> {
    let inserted = transaction
        .execute(
            "INSERT INTO sync_outbox (outbox_id, event_id, state, created_at_ms)
             VALUES (
                ?1, ?2, 'pending',
                CAST(unixepoch(?3, 'subsec') * 1000 AS INTEGER)
             ) ON CONFLICT DO NOTHING",
            params![
                serialized.outbox_id,
                serialized.event.event_id,
                serialized.event.created_at
            ],
        )
        .map_err(|error| DbError::sqlite("insert event outbox", error))?;
    if inserted == 0 {
        validate_existing_outbox(transaction, serialized)?;
    }
    Ok(())
}

fn validate_existing_outbox(
    transaction: &Transaction<'_>,
    serialized: &SerializedEvent<'_>,
) -> Result<(), DbError> {
    let identical = transaction
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sync_outbox
                WHERE outbox_id = ?1
                  AND event_id = ?2
                  AND created_at_ms =
                      CAST(unixepoch(?3, 'subsec') * 1000 AS INTEGER)
            )",
            params![
                serialized.outbox_id,
                serialized.event.event_id,
                serialized.event.created_at
            ],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|error| DbError::sqlite("validate existing event Outbox", error))?;
    if identical {
        Ok(())
    } else {
        Err(DbError::sqlite(
            "validate existing event Outbox",
            format!(
                "stable Outbox {} conflicts with different immutable fields",
                serialized.outbox_id
            ),
        ))
    }
}

#[cfg(feature = "process-test-hooks")]
fn wait_at_process_test_barrier(
    barrier: &ProcessTestBarrier,
    ready_contents: &[u8],
) -> Result<(), DbError> {
    let mut ready = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&barrier.ready)
        .map_err(|error| DbError::sqlite("create process test barrier", error))?;
    ready
        .write_all(ready_contents)
        .and_then(|()| ready.sync_all())
        .map_err(|error| DbError::sqlite("publish process test barrier", error))?;

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match fs::symlink_metadata(&barrier.release) {
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
    upsert_collector_state_with_media_guard_in(connection, state, false)
}

pub(crate) fn upsert_collector_state_preserving_media_failure_in(
    connection: &Connection,
    state: &CollectorState,
) -> Result<(), DbError> {
    upsert_collector_state_with_media_guard_in(connection, state, true)
}

fn upsert_collector_state_with_media_guard_in(
    connection: &Connection,
    state: &CollectorState,
    preserve_media_failure: bool,
) -> Result<(), DbError> {
    connection
        .execute(
            "INSERT INTO collector_states (
                collector_key, status, version, desired_revision, applied_revision,
                last_event_at_ms, last_health_at_ms, last_error_code,
                created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(collector_key) DO UPDATE SET
                status = CASE
                    WHEN (collector_states.last_error_code = 'SYNC_PAYLOAD_REJECTED'
                          OR (?11 = 1 AND collector_states.last_error_code IN (
                              'COMMUNICATION_MEDIA_UPLOAD_FAILED',
                              'PHOTOS_UPLOAD_FAILED',
                              'SCREEN_UPLOAD_FAILED',
                              'SCREEN_UPLOAD_TIMEOUT',
                              'MEDIA_CYCLE_TIMEOUT'
                          )))
                     AND excluded.status = 'running'
                     AND excluded.last_error_code IS NULL
                    THEN 'degraded'
                    ELSE excluded.status
                END,
                version = excluded.version,
                desired_revision = excluded.desired_revision,
                applied_revision = excluded.applied_revision,
                last_event_at_ms = excluded.last_event_at_ms,
                last_health_at_ms = CASE
                    WHEN ?11 = 1
                     AND collector_states.last_error_code IN (
                         'COMMUNICATION_MEDIA_UPLOAD_FAILED',
                         'PHOTOS_UPLOAD_FAILED',
                         'SCREEN_UPLOAD_FAILED',
                         'SCREEN_UPLOAD_TIMEOUT',
                         'MEDIA_CYCLE_TIMEOUT'
                     )
                     AND excluded.status = 'running'
                     AND excluded.last_error_code IS NULL
                    THEN collector_states.last_health_at_ms
                    ELSE excluded.last_health_at_ms
                END,
                last_error_code = CASE
                    WHEN (collector_states.last_error_code = 'SYNC_PAYLOAD_REJECTED'
                          OR (?11 = 1 AND collector_states.last_error_code IN (
                              'COMMUNICATION_MEDIA_UPLOAD_FAILED',
                              'PHOTOS_UPLOAD_FAILED',
                              'SCREEN_UPLOAD_FAILED',
                              'SCREEN_UPLOAD_TIMEOUT',
                              'MEDIA_CYCLE_TIMEOUT'
                          )))
                     AND excluded.status = 'running'
                     AND excluded.last_error_code IS NULL
                    THEN collector_states.last_error_code
                    ELSE excluded.last_error_code
                END,
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
                i64::from(preserve_media_failure),
            ],
        )
        .map(|_| ())
        .map_err(|error| DbError::sqlite("upsert Collector state", error))
}

pub(crate) fn load_pairing_state(connection: &Connection) -> Result<Option<PairingState>, DbError> {
    connection
        .query_row(
            "SELECT device_id, workspace_id, credential_ref, credential_generation,
                    cloud_api_origin, applied_control_revision, paired_at_ms, manually_unpaired
             FROM pairing_state WHERE singleton_id = 1",
            [],
            |row| {
                Ok(PairingState {
                    device_id: row.get(0)?,
                    workspace_id: row.get(1)?,
                    credential_ref: row.get(2)?,
                    credential_generation: row.get(3)?,
                    cloud_api_origin: row.get(4)?,
                    applied_control_revision: row.get(5)?,
                    paired_at_ms: row.get(6)?,
                    manually_unpaired: row.get(7)?,
                })
            },
        )
        .optional()
        .map_err(|error| DbError::sqlite("load pairing state", error))
}

pub(crate) fn save_pairing_state(
    connection: &mut Connection,
    state: &PairingState,
) -> Result<(), DbError> {
    let transaction = connection
        .transaction()
        .map_err(|error| DbError::sqlite("start pairing state transaction", error))?;
    transaction
        .execute(
            "DELETE FROM applied_collector_control
             WHERE singleton_id = 1
               AND (device_id <> ?1 OR workspace_id <> ?2)",
            params![state.device_id, state.workspace_id],
        )
        .map_err(|error| DbError::sqlite("clear replaced pairing control", error))?;
    transaction
        .execute(
            "INSERT INTO pairing_state (
                singleton_id, device_id, workspace_id, credential_ref,
                credential_generation, cloud_api_origin, applied_control_revision, paired_at_ms,
                manually_unpaired
             ) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(singleton_id) DO UPDATE SET
                device_id = excluded.device_id,
                workspace_id = excluded.workspace_id,
                credential_ref = excluded.credential_ref,
                credential_generation = excluded.credential_generation,
                cloud_api_origin = excluded.cloud_api_origin,
                applied_control_revision = excluded.applied_control_revision,
                paired_at_ms = excluded.paired_at_ms,
                manually_unpaired = excluded.manually_unpaired",
            params![
                state.device_id,
                state.workspace_id,
                state.credential_ref,
                state.credential_generation,
                state.cloud_api_origin,
                state.applied_control_revision,
                state.paired_at_ms,
                state.manually_unpaired,
            ],
        )
        .map_err(|error| DbError::sqlite("save pairing state", error))?;
    transaction
        .commit()
        .map_err(|error| DbError::sqlite("commit pairing state", error))
}

pub(crate) fn save_control_revision(
    connection: &Connection,
    applied_control_revision: u64,
) -> Result<(), DbError> {
    let updated = connection
        .execute(
            "UPDATE pairing_state
             SET applied_control_revision = MAX(applied_control_revision, ?1)
             WHERE singleton_id = 1",
            [applied_control_revision],
        )
        .map_err(|error| DbError::sqlite("save applied control revision", error))?;
    if updated == 1 {
        Ok(())
    } else {
        Err(DbError::sqlite(
            "save applied control revision",
            "pairing state is absent",
        ))
    }
}

pub(crate) fn load_applied_collector_control(
    connection: &Connection,
) -> Result<Option<AppliedCollectorControl>, DbError> {
    connection
        .query_row(
            "SELECT device_id, workspace_id, configuration_revision,
                    communication_wechat_enabled, screen_capture_enabled,
                    screen_capture_scheduled_enabled, screen_capture_interval_seconds,
                    screen_capture_activity_enabled,
                    screen_capture_activity_min_interval_seconds,
                    screen_capture_excluded_bundle_ids_json, updated_at_ms
             FROM applied_collector_control WHERE singleton_id = 1",
            [],
            |row| {
                let excluded_json = row.get::<_, String>(9)?;
                let excluded =
                    serde_json::from_str::<Vec<String>>(&excluded_json).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            9,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?;
                Ok(AppliedCollectorControl {
                    device_id: row.get(0)?,
                    workspace_id: row.get(1)?,
                    configuration_revision: row.get(2)?,
                    communication_wechat_enabled: row.get(3)?,
                    screen_capture_enabled: row.get(4)?,
                    screen_capture_scheduled_enabled: row.get(5)?,
                    screen_capture_interval_seconds: row.get(6)?,
                    screen_capture_activity_enabled: row.get(7)?,
                    screen_capture_activity_min_interval_seconds: row.get(8)?,
                    screen_capture_excluded_bundle_ids: excluded,
                    updated_at_ms: row.get(10)?,
                })
            },
        )
        .optional()
        .map_err(|error| DbError::sqlite("load applied Collector control", error))
}

pub(crate) fn save_applied_collector_control(
    connection: &mut Connection,
    control: &AppliedCollectorControl,
) -> Result<(), DbError> {
    validate_applied_collector_control(control)?;
    let excluded_bundle_ids = serde_json::to_string(&control.screen_capture_excluded_bundle_ids)
        .map_err(|error| DbError::Serialization(error.to_string()))?;
    let transaction = connection
        .transaction()
        .map_err(|error| DbError::sqlite("start applied Collector control transaction", error))?;
    let pairing = transaction
        .query_row(
            "SELECT device_id, workspace_id, manually_unpaired
             FROM pairing_state WHERE singleton_id = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|error| DbError::sqlite("validate applied Collector control identity", error))?;
    if pairing.as_ref()
        != Some(&(
            control.device_id.clone(),
            control.workspace_id.clone(),
            false,
        ))
    {
        return Err(DbError::sqlite(
            "validate applied Collector control identity",
            "control does not belong to the active local pairing",
        ));
    }
    let control_updated = transaction
        .execute(
            "INSERT INTO applied_collector_control (
                singleton_id, device_id, workspace_id, configuration_revision,
                communication_wechat_enabled, screen_capture_enabled,
                screen_capture_scheduled_enabled, screen_capture_interval_seconds,
                screen_capture_activity_enabled,
                screen_capture_activity_min_interval_seconds,
                screen_capture_excluded_bundle_ids_json, updated_at_ms
             ) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(singleton_id) DO UPDATE SET
                device_id = excluded.device_id,
                workspace_id = excluded.workspace_id,
                configuration_revision = excluded.configuration_revision,
                communication_wechat_enabled = excluded.communication_wechat_enabled,
                screen_capture_enabled = excluded.screen_capture_enabled,
                screen_capture_scheduled_enabled = excluded.screen_capture_scheduled_enabled,
                screen_capture_interval_seconds = excluded.screen_capture_interval_seconds,
                screen_capture_activity_enabled = excluded.screen_capture_activity_enabled,
                screen_capture_activity_min_interval_seconds =
                    excluded.screen_capture_activity_min_interval_seconds,
                screen_capture_excluded_bundle_ids_json =
                    excluded.screen_capture_excluded_bundle_ids_json,
                updated_at_ms = excluded.updated_at_ms
             WHERE excluded.device_id = applied_collector_control.device_id
               AND excluded.workspace_id = applied_collector_control.workspace_id
               AND excluded.configuration_revision >=
                   applied_collector_control.configuration_revision",
            params![
                control.device_id,
                control.workspace_id,
                control.configuration_revision,
                control.communication_wechat_enabled,
                control.screen_capture_enabled,
                control.screen_capture_scheduled_enabled,
                control.screen_capture_interval_seconds,
                control.screen_capture_activity_enabled,
                control.screen_capture_activity_min_interval_seconds,
                excluded_bundle_ids,
                control.updated_at_ms,
            ],
        )
        .map_err(|error| DbError::sqlite("save applied Collector control", error))?;
    if control_updated == 0 {
        return transaction
            .rollback()
            .map_err(|error| DbError::sqlite("rollback stale applied Collector control", error));
    }
    transaction
        .execute(
            "UPDATE pairing_state
             SET applied_control_revision = MAX(applied_control_revision, ?1)
             WHERE singleton_id = 1",
            [control.configuration_revision],
        )
        .map_err(|error| DbError::sqlite("save applied Collector control revision", error))?;
    transaction
        .commit()
        .map_err(|error| DbError::sqlite("commit applied Collector control", error))
}

fn validate_applied_collector_control(control: &AppliedCollectorControl) -> Result<(), DbError> {
    if !is_uuid_like(&control.device_id)
        || !is_uuid_like(&control.workspace_id)
        || control.configuration_revision == 0
        || !(60..=86_400).contains(&control.screen_capture_interval_seconds)
        || !(10..=3_600).contains(&control.screen_capture_activity_min_interval_seconds)
        || control.screen_capture_excluded_bundle_ids.len() > 100
        || control
            .screen_capture_excluded_bundle_ids
            .iter()
            .any(|value| {
                value.is_empty()
                    || value.len() > 255
                    || !value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
            })
        || control.updated_at_ms < 0
    {
        return Err(DbError::sqlite(
            "validate applied Collector control",
            "control does not match the fixed local persistence contract",
        ));
    }
    Ok(())
}

pub(crate) fn mark_pairing_manually_unpaired_and_disable_sensitive_collectors(
    connection: &mut Connection,
) -> Result<(), DbError> {
    let transaction = connection
        .transaction()
        .map_err(|error| DbError::sqlite("start pairing revocation transaction", error))?;
    let updated = transaction
        .execute(
            "UPDATE pairing_state SET manually_unpaired = 1 WHERE singleton_id = 1",
            [],
        )
        .map_err(|error| DbError::sqlite("mark pairing manually unpaired", error))?;
    if updated != 1 {
        return Err(DbError::sqlite(
            "mark pairing manually unpaired",
            "pairing state is absent",
        ));
    }
    transaction
        .execute(
            "DELETE FROM applied_collector_control WHERE singleton_id = 1",
            [],
        )
        .map_err(|error| DbError::sqlite("clear applied Collector control", error))?;
    let now_ms = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX);
    for collector_key in ["network", "communication.wechat", "screen.capture"] {
        transaction
            .execute(
                "INSERT INTO collector_states (
                    collector_key, status, version, desired_revision, applied_revision,
                    last_event_at_ms, last_health_at_ms, last_error_code,
                    created_at_ms, updated_at_ms
                 ) VALUES (?1, 'disabled', 's1b-unavailable', 0, 0, NULL, NULL, NULL, ?2, ?2)
                 ON CONFLICT(collector_key) DO UPDATE SET
                    status = 'disabled',
                    desired_revision = 0,
                    applied_revision = 0,
                    last_error_code = NULL,
                    updated_at_ms = excluded.updated_at_ms",
                params![collector_key, now_ms],
            )
            .map_err(|error| DbError::sqlite("disable sensitive Collector", error))?;
    }
    transaction
        .commit()
        .map_err(|error| DbError::sqlite("commit pairing revocation transaction", error))
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

pub(crate) fn active_outbox_depth(connection: &Connection) -> Result<u64, DbError> {
    connection
        .query_row(
            "SELECT COUNT(*) FROM sync_outbox WHERE state IN ('pending', 'sending')",
            [],
            |row| row.get(0),
        )
        .map_err(|error| DbError::sqlite("count active Outbox rows", error))
}

pub(crate) fn load_pending_system_events(
    connection: &Connection,
    limit: u16,
) -> Result<Vec<EventEnvelope>, DbError> {
    let mut statement = connection
        .prepare(
            "SELECT e.event_id, e.workspace_id, e.device_id,
                    CASE e.event_type
                        WHEN 'AGENT_STARTED' THEN 'agent.started'
                        WHEN 'AGENT_STOPPED' THEN 'agent.stopped'
                        WHEN 'AGENT_CRASH_RECOVERED' THEN 'agent.crash_recovered'
                        WHEN 'SYSTEM_SLEEP' THEN 'system.sleep'
                        WHEN 'SYSTEM_WAKE' THEN 'system.wake'
                        ELSE e.event_type
                    END AS event_type,
                    CASE
                        WHEN e.event_type IN (
                            'AGENT_STARTED', 'AGENT_STOPPED', 'AGENT_CRASH_RECOVERED',
                            'SYSTEM_SLEEP', 'SYSTEM_WAKE'
                        ) AND e.source = 'runtime'
                        THEN 'runtime.lifecycle'
                        ELSE e.source
                    END AS source,
                    e.schema_version, e.occurred_at_ms, e.created_at_ms, e.payload_json,
                    e.idempotency_key, e.sensitivity
             FROM sync_outbox AS o
             INNER JOIN events_local AS e ON e.event_id = o.event_id
             WHERE o.state IN ('pending', 'sending')
               AND e.event_type IN (
                   'system.metric_sampled',
                   'system.health_changed',
                   'collector.status_changed',
                   'agent.started',
                   'agent.stopped',
                   'agent.crash_recovered',
                   'system.sleep',
                   'system.wake',
                   'network.offline',
                   'network.online',
                   'network.changed',
                   'photos.asset_recorded',
                   'AGENT_STARTED',
                   'AGENT_STOPPED',
                   'AGENT_CRASH_RECOVERED',
                   'SYSTEM_SLEEP',
                   'SYSTEM_WAKE'
               )
               AND ((e.event_type = 'photos.asset_recorded' AND e.source = 'photos.library' AND e.sensitivity = 'high')
                    OR (e.event_type <> 'photos.asset_recorded' AND e.sensitivity = 'normal'))
               AND e.attachment_refs_json = '[]'
             ORDER BY o.created_at_ms, e.event_id
             LIMIT ?1",
        )
        .map_err(|error| DbError::sqlite("prepare pending system event query", error))?;
    let rows = statement
        .query_map([i64::from(limit)], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, u32>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, String>(10)?,
            ))
        })
        .map_err(|error| DbError::sqlite("query pending system events", error))?;
    rows.map(|row| {
        let (
            event_id,
            workspace_id,
            device_id,
            event_type,
            source,
            schema_version,
            occurred_at_ms,
            created_at_ms,
            payload_json,
            idempotency_key,
            sensitivity,
        ) = row.map_err(|error| DbError::sqlite("read pending system event", error))?;
        let payload = serde_json::from_str(&payload_json)
            .map_err(|error| DbError::Serialization(error.to_string()))?;
        Ok(EventEnvelope {
            event_id,
            workspace_id,
            device_id,
            event_type,
            source,
            schema_version,
            occurred_at: format_timestamp(occurred_at_ms)?,
            created_at: format_timestamp(created_at_ms)?,
            sensitivity: system_event_sensitivity(&sensitivity),
            payload,
            attachment_refs: Vec::new(),
            idempotency_key,
        })
    })
    .collect()
}

fn system_event_sensitivity(value: &str) -> Sensitivity {
    if value == "high" {
        Sensitivity::High
    } else {
        Sensitivity::Normal
    }
}

pub(crate) fn dead_letter_mismatched_outbox_events(
    connection: &mut Connection,
    workspace_id: &str,
    device_id: &str,
) -> Result<u64, DbError> {
    let transaction = connection
        .transaction()
        .map_err(|error| DbError::sqlite("start Outbox identity quarantine", error))?;
    let updated = transaction
        .execute(
            "UPDATE sync_outbox
             SET state = 'dead_letter'
             WHERE state IN ('pending', 'sending')
               AND EXISTS (
                   SELECT 1 FROM events_local AS e
                   WHERE e.event_id = sync_outbox.event_id
                     AND (e.workspace_id <> ?1 OR e.device_id <> ?2)
               )",
            params![workspace_id, device_id],
        )
        .map_err(|error| DbError::sqlite("dead-letter mismatched Outbox events", error))?;
    if updated > 0 {
        let occurred_at_ms = i64::try_from(
            OffsetDateTime::now_utc()
                .unix_timestamp_nanos()
                .div_euclid(1_000_000),
        )
        .unwrap_or(i64::MAX);
        let redacted_json = serde_json::json!({ "dead_lettered_event_count": updated }).to_string();
        transaction
            .execute(
                "INSERT INTO diagnostic_events (
                    diagnostic_id, occurred_at_ms, level, code, redacted_json
                 ) VALUES (
                    'OUTBOX_IDENTITY_MISMATCH', ?1, 'warning',
                    'OUTBOX_IDENTITY_MISMATCH', ?2
                 )
                 ON CONFLICT(diagnostic_id) DO UPDATE SET
                    occurred_at_ms = excluded.occurred_at_ms,
                    level = excluded.level,
                    code = excluded.code,
                    redacted_json = excluded.redacted_json",
                params![occurred_at_ms, redacted_json],
            )
            .map_err(|error| DbError::sqlite("record Outbox identity diagnostic", error))?;
    }
    transaction
        .commit()
        .map_err(|error| DbError::sqlite("commit Outbox identity quarantine", error))?;
    u64::try_from(updated)
        .map_err(|_| DbError::sqlite("dead-letter mismatched Outbox events", "row count overflow"))
}

pub(crate) fn acknowledge_system_events(
    connection: &mut Connection,
    event_ids: &[String],
) -> Result<(), DbError> {
    let transaction = connection
        .transaction()
        .map_err(|error| DbError::sqlite("start system event acknowledgement", error))?;
    for event_id in event_ids {
        let updated = transaction
            .execute(
                "UPDATE sync_outbox
                 SET state = 'acked'
                 WHERE event_id = ?1
                   AND state IN ('pending', 'sending')
                   AND EXISTS (
                       SELECT 1 FROM events_local
                       WHERE events_local.event_id = sync_outbox.event_id
                         AND events_local.event_type IN (
                             'system.metric_sampled',
                             'system.health_changed',
                             'collector.status_changed',
                             'agent.started',
                             'agent.stopped',
                             'agent.crash_recovered',
                             'system.sleep',
                             'system.wake',
                             'network.offline',
                             'network.online',
                             'network.changed',
                             'photos.asset_recorded',
                             'AGENT_STARTED',
                             'AGENT_STOPPED',
                             'AGENT_CRASH_RECOVERED',
                             'SYSTEM_SLEEP',
                             'SYSTEM_WAKE'
                         )
                   )",
                [event_id],
            )
            .map_err(|error| DbError::sqlite("acknowledge system event", error))?;
        if updated != 1 {
            return Err(DbError::sqlite(
                "acknowledge system event",
                "event was not pending system data",
            ));
        }
        clear_sync_failure_after_acknowledgement(&transaction, event_id)?;
    }
    transaction
        .commit()
        .map_err(|error| DbError::sqlite("commit system event acknowledgement", error))
}

pub(crate) fn dead_letter_rejected_system_events(
    connection: &mut Connection,
    event_ids: &[String],
) -> Result<(), DbError> {
    dead_letter_rejected_events(connection, event_ids, "system", "SYNC_DEAD_LETTER")
}

pub(crate) fn load_pending_communication_events(
    connection: &Connection,
    limit: u16,
) -> Result<Vec<EventEnvelope>, DbError> {
    let mut statement = connection
        .prepare(
            "SELECT e.event_id, e.workspace_id, e.device_id, e.event_type, e.source,
                    e.schema_version, e.occurred_at_ms, e.created_at_ms, e.payload_json,
                    e.attachment_refs_json, e.idempotency_key
             FROM sync_outbox AS o
             INNER JOIN events_local AS e ON e.event_id = o.event_id
             WHERE o.state IN ('pending', 'sending')
               AND e.event_type IN (
                   'communication.message_recorded',
                   'communication.conversation_observed',
                   'communication.message_sender_observed'
               )
               AND e.source IN ('communication.wechat', 'communication.messages')
               AND e.schema_version = 1
               AND e.sensitivity = 'high'
             ORDER BY o.created_at_ms, e.event_id
             LIMIT ?1",
        )
        .map_err(|error| DbError::sqlite("prepare pending communication event query", error))?;
    let rows = statement
        .query_map([i64::from(limit)], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, u32>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, Option<String>>(10)?,
            ))
        })
        .map_err(|error| DbError::sqlite("query pending communication events", error))?;
    rows.map(|row| {
        let (
            event_id,
            workspace_id,
            device_id,
            event_type,
            source,
            schema_version,
            occurred_at_ms,
            created_at_ms,
            payload_json,
            attachment_refs_json,
            idempotency_key,
        ) = row.map_err(|error| DbError::sqlite("read pending communication event", error))?;
        Ok(EventEnvelope {
            event_id,
            workspace_id,
            device_id,
            event_type,
            source,
            schema_version,
            occurred_at: format_timestamp(occurred_at_ms)?,
            created_at: format_timestamp(created_at_ms)?,
            sensitivity: Sensitivity::High,
            payload: serde_json::from_str(&payload_json)
                .map_err(|error| DbError::Serialization(error.to_string()))?,
            attachment_refs: serde_json::from_str(&attachment_refs_json)
                .map_err(|error| DbError::Serialization(error.to_string()))?,
            idempotency_key,
        })
    })
    .collect()
}

pub(crate) fn acknowledge_communication_events(
    connection: &mut Connection,
    event_ids: &[String],
) -> Result<(), DbError> {
    let transaction = connection
        .transaction()
        .map_err(|error| DbError::sqlite("start communication event acknowledgement", error))?;
    for event_id in event_ids {
        let updated = transaction
            .execute(
                "UPDATE sync_outbox
                 SET state = 'acked'
                 WHERE event_id = ?1
                   AND state IN ('pending', 'sending')
                   AND EXISTS (
                       SELECT 1 FROM events_local AS e
                       WHERE e.event_id = sync_outbox.event_id
                         AND e.event_type IN (
                             'communication.message_recorded',
                             'communication.conversation_observed',
                             'communication.message_sender_observed'
                         )
                         AND e.source IN ('communication.wechat', 'communication.messages')
                         AND e.schema_version = 1
                         AND e.sensitivity = 'high'
                   )",
                [event_id],
            )
            .map_err(|error| DbError::sqlite("acknowledge communication event", error))?;
        if updated != 1 {
            return Err(DbError::sqlite(
                "acknowledge communication event",
                "event was not pending communication data",
            ));
        }
        clear_sync_failure_after_acknowledgement(&transaction, event_id)?;
    }
    transaction
        .commit()
        .map_err(|error| DbError::sqlite("commit communication event acknowledgement", error))
}

pub(crate) fn dead_letter_rejected_communication_events(
    connection: &mut Connection,
    event_ids: &[String],
) -> Result<(), DbError> {
    dead_letter_rejected_events(connection, event_ids, "communication", "SYNC_DEAD_LETTER")
}

fn clear_sync_failure_after_acknowledgement(
    transaction: &Transaction<'_>,
    event_id: &str,
) -> Result<(), DbError> {
    transaction
        .execute(
            "UPDATE collector_states
             SET last_error_code = NULL
             WHERE last_error_code = 'SYNC_PAYLOAD_REJECTED'
               AND collector_key = (
                    SELECT CASE
                        WHEN source = 'communication.wechat' THEN 'communication.wechat'
                        WHEN source = 'communication.messages' THEN 'communication.messages'
                        WHEN source = 'photos.library' THEN 'photos.library'
                        WHEN event_type LIKE 'network.%' THEN 'network'
                        ELSE 'system'
                    END
                    FROM events_local WHERE event_id = ?1
               )",
            [event_id],
        )
        .map(|_| ())
        .map_err(|error| DbError::sqlite("clear recovered sync failure", error))
}

fn dead_letter_rejected_events(
    connection: &mut Connection,
    event_ids: &[String],
    event_class: &'static str,
    diagnostic_code: &'static str,
) -> Result<(), DbError> {
    let transaction = connection
        .transaction()
        .map_err(|error| DbError::sqlite("start rejected event quarantine", error))?;
    let occurred_at_ms = i64::try_from(
        OffsetDateTime::now_utc()
            .unix_timestamp_nanos()
            .div_euclid(1_000_000),
    )
    .unwrap_or(i64::MAX);
    for event_id in event_ids {
        let updated = transaction
            .execute(
                "UPDATE sync_outbox
                 SET state = 'dead_letter'
                 WHERE event_id = ?1 AND state IN ('pending', 'sending')
                   AND EXISTS (
                       SELECT 1 FROM events_local AS e
                       WHERE e.event_id = sync_outbox.event_id
                         AND (
                           (?2 = 'system' AND e.event_type IN (
                               'system.metric_sampled', 'system.health_changed',
                               'collector.status_changed', 'agent.started', 'agent.stopped',
                               'agent.crash_recovered', 'system.sleep', 'system.wake',
                               'network.offline', 'network.online', 'network.changed',
                               'photos.asset_recorded', 'AGENT_STARTED', 'AGENT_STOPPED',
                               'AGENT_CRASH_RECOVERED', 'SYSTEM_SLEEP', 'SYSTEM_WAKE'
                           ))
                           OR
                           (?2 = 'communication'
                            AND e.event_type IN (
                                'communication.message_recorded',
                                'communication.conversation_observed',
                                'communication.message_sender_observed'
                            )
                            AND e.source IN ('communication.wechat', 'communication.messages')
                            AND e.schema_version = 1
                            AND e.sensitivity = 'high')
                         )
                   )",
                params![event_id, event_class],
            )
            .map_err(|error| DbError::sqlite("quarantine rejected event", error))?;
        if updated != 1 {
            return Err(DbError::sqlite(
                "quarantine rejected event",
                "event was not pending",
            ));
        }
        let diagnostic_id = format!("{diagnostic_code}:{event_id}");
        let redacted_json = serde_json::json!({ "event_id": event_id }).to_string();
        transaction
            .execute(
                "INSERT INTO diagnostic_events (
                    diagnostic_id, occurred_at_ms, level, code, redacted_json
                 ) VALUES (?1, ?2, 'error', ?3, ?4)
                 ON CONFLICT(diagnostic_id) DO UPDATE SET
                    occurred_at_ms = excluded.occurred_at_ms,
                    level = excluded.level,
                    code = excluded.code,
                    redacted_json = excluded.redacted_json",
                params![
                    diagnostic_id,
                    occurred_at_ms,
                    diagnostic_code,
                    redacted_json
                ],
            )
            .map_err(|error| DbError::sqlite("record rejected event diagnostic", error))?;
    }
    transaction
        .commit()
        .map_err(|error| DbError::sqlite("commit rejected event quarantine", error))
}

struct PendingAttachmentManifest {
    event_id: String,
    source: String,
    attachment_id: String,
    sha256: String,
    size_bytes: i64,
    mime_type: String,
    file_name: String,
    transfer_state: String,
}

pub(crate) fn load_pending_communication_attachments(
    connection: &Connection,
    spool_root: &Path,
    limit: u16,
) -> Result<Vec<PendingCommunicationAttachment>, DbError> {
    let mut statement = connection
        .prepare(
            "SELECT m.event_id, e.source, s.attachment_id, s.sha256, s.size_bytes, s.mime_type,
                    s.spool_relative_path, s.transfer_state
             FROM attachment_spool AS s
             INNER JOIN communication_messages AS m
                ON m.local_message_id = s.local_message_id
             INNER JOIN events_local AS e ON e.event_id = m.event_id
             INNER JOIN sync_outbox AS o ON o.event_id = m.event_id
             WHERE o.state = 'acked'
               AND s.transfer_state <> 'completed'
               AND s.terminal_failure_code IS NULL
             ORDER BY s.created_at_ms, s.attachment_id",
        )
        .map_err(|error| DbError::sqlite("prepare pending attachment query", error))?;
    let rows = statement
        .query_map([], |row| {
            Ok(PendingAttachmentManifest {
                event_id: row.get(0)?,
                source: row.get(1)?,
                attachment_id: row.get(2)?,
                sha256: row.get(3)?,
                size_bytes: row.get(4)?,
                mime_type: row.get(5)?,
                file_name: row.get(6)?,
                transfer_state: row.get(7)?,
            })
        })
        .map_err(|error| DbError::sqlite("query pending attachments", error))?;
    let manifests = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| DbError::sqlite("read pending attachment", error))?;
    drop(statement);
    let (pending, failed): (Vec<_>, Vec<_>) = manifests
        .into_iter()
        .partition(|manifest| manifest.transfer_state != "failed");
    let limit = usize::from(limit);
    let pending_target = if !pending.is_empty() && !failed.is_empty() && limit > 1 {
        limit - 1
    } else {
        limit
    };
    let mut pending = pending.into_iter();
    let mut failed = failed.into_iter();
    let mut loaded = Vec::with_capacity(limit);
    load_valid_attachments(
        connection,
        spool_root,
        &mut pending,
        pending_target,
        &mut loaded,
    )?;
    load_valid_attachments(connection, spool_root, &mut failed, limit, &mut loaded)?;
    load_valid_attachments(connection, spool_root, &mut pending, limit, &mut loaded)?;
    Ok(loaded)
}

fn load_valid_attachments(
    connection: &Connection,
    spool_root: &Path,
    manifests: &mut impl Iterator<Item = PendingAttachmentManifest>,
    target: usize,
    loaded: &mut Vec<PendingCommunicationAttachment>,
) -> Result<(), DbError> {
    while loaded.len() < target {
        let Some(manifest) = manifests.next() else {
            break;
        };
        match load_valid_attachment(spool_root, manifest) {
            Ok(attachment) => loaded.push(attachment),
            Err((attachment_id, source, _error)) => {
                quarantine_invalid_attachment(connection, &attachment_id, &source)?;
            }
        }
    }
    Ok(())
}

fn load_valid_attachment(
    spool_root: &Path,
    manifest: PendingAttachmentManifest,
) -> Result<PendingCommunicationAttachment, (String, String, DbError)> {
    let attachment_id = manifest.attachment_id.clone();
    let source = manifest.source.clone();
    let result = (|| {
        let expected_size = u64::try_from(manifest.size_bytes).map_err(|_| {
            DbError::sqlite("read pending attachment", "attachment size is invalid")
        })?;
        let file = open_communication_spool_file(spool_root, &manifest.file_name)?;
        if file
            .metadata()
            .map_err(|error| DbError::sqlite("read pending attachment metadata", error))?
            .len()
            != expected_size
        {
            return Err(DbError::sqlite(
                "verify pending attachment metadata",
                "attachment size does not match immutable manifest",
            ));
        }
        Ok(PendingCommunicationAttachment {
            event_id: manifest.event_id,
            source: manifest.source,
            attachment_id: manifest.attachment_id,
            sha256: manifest.sha256,
            size_bytes: expected_size,
            mime_type: manifest.mime_type,
            file,
        })
    })();
    result.map_err(|error| (attachment_id, source, error))
}

pub(crate) fn quarantine_invalid_attachment(
    connection: &Connection,
    attachment_id: &str,
    source: &str,
) -> Result<(), DbError> {
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| DbError::sqlite("start invalid attachment quarantine", error))?;
    transaction
        .execute(
            "UPDATE attachment_spool
             SET transfer_state = 'failed',
                 terminal_failure_code = 'MEDIA_LOCAL_BODY_INVALID'
             WHERE attachment_id = ?1
               AND transfer_state <> 'completed'
               AND terminal_failure_code IS NULL",
            [attachment_id],
        )
        .map_err(|error| DbError::sqlite("quarantine invalid attachment", error))?;
    let occurred_at_ms = i64::try_from(
        OffsetDateTime::now_utc()
            .unix_timestamp_nanos()
            .div_euclid(1_000_000),
    )
    .unwrap_or(i64::MAX);
    record_media_diagnostic(
        &transaction,
        attachment_id,
        "MEDIA_LOCAL_BODY_INVALID",
        "local_validation",
        "contract",
        None,
        occurred_at_ms,
    )?;
    if let Some(collector_key) = communication_collector_key(source) {
        persist_terminal_media_collector_failure(
            &transaction,
            collector_key,
            "MEDIA_LOCAL_BODY_INVALID",
            occurred_at_ms,
        )?;
    }
    transaction
        .commit()
        .map_err(|error| DbError::sqlite("commit invalid attachment quarantine", error))
}

pub(crate) fn complete_communication_attachment(
    connection: &Connection,
    attachment_id: &str,
) -> Result<(), DbError> {
    let updated = connection
        .execute(
            "UPDATE attachment_spool
             SET transfer_state = 'completed',
                 completed_at_ms = CAST(unixepoch('now', 'subsec') * 1000 AS INTEGER)
             WHERE attachment_id = ?1
               AND transfer_state <> 'completed'
               AND terminal_failure_code IS NULL",
            [attachment_id],
        )
        .map_err(|error| DbError::sqlite("complete communication attachment", error))?;
    if updated == 1 {
        Ok(())
    } else {
        Err(DbError::sqlite(
            "complete communication attachment",
            "attachment was not pending",
        ))
    }
}

pub(crate) fn defer_communication_attachment(
    connection: &Connection,
    attachment_id: &str,
    failure_stage: &str,
    failure_category: &str,
    fallback_from: Option<&str>,
) -> Result<(), DbError> {
    const FAILURE_STAGES: &[&str] = &[
        "prepare",
        "proxy_upload",
        "direct_upload",
        "complete",
        "client",
        "local_validation",
    ];
    const FAILURE_CATEGORIES: &[&str] = &["transient", "revoked", "invalid_credential", "contract"];
    if !FAILURE_STAGES.contains(&failure_stage)
        || !FAILURE_CATEGORIES.contains(&failure_category)
        || fallback_from.is_some_and(|stage| !FAILURE_STAGES.contains(&stage))
    {
        return Err(DbError::sqlite(
            "defer communication attachment",
            "media upload diagnostic classification is invalid",
        ));
    }
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| DbError::sqlite("start attachment deferral", error))?;
    let attempted_at_ms = i64::try_from(
        OffsetDateTime::now_utc()
            .unix_timestamp_nanos()
            .div_euclid(1_000_000),
    )
    .unwrap_or(i64::MAX);
    let updated = transaction
        .execute(
            "UPDATE attachment_spool
             SET transfer_state = 'failed',
                 created_at_ms = MAX(
                    ?2,
                    (SELECT COALESCE(MAX(queued.created_at_ms), 0) + 1
                     FROM attachment_spool AS queued)
                 )
             WHERE attachment_id = ?1
               AND transfer_state <> 'completed'
               AND terminal_failure_code IS NULL",
            params![attachment_id, attempted_at_ms],
        )
        .map_err(|error| DbError::sqlite("defer communication attachment", error))?;
    if updated != 1 {
        return Err(DbError::sqlite(
            "defer communication attachment",
            "attachment was not pending",
        ));
    }
    record_media_diagnostic(
        &transaction,
        attachment_id,
        "MEDIA_UPLOAD_FAILED",
        failure_stage,
        failure_category,
        fallback_from,
        attempted_at_ms,
    )?;
    transaction
        .commit()
        .map_err(|error| DbError::sqlite("commit attachment deferral", error))
}

pub(crate) fn quarantine_unsupported_communication_attachment(
    connection: &Connection,
    attachment_id: &str,
) -> Result<(), DbError> {
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| DbError::sqlite("start unsupported attachment quarantine", error))?;
    let occurred_at_ms = i64::try_from(
        OffsetDateTime::now_utc()
            .unix_timestamp_nanos()
            .div_euclid(1_000_000),
    )
    .unwrap_or(i64::MAX);
    let updated = transaction
        .execute(
            "UPDATE attachment_spool
             SET transfer_state = 'failed',
                 terminal_failure_code = 'MEDIA_SOURCE_UNSUPPORTED'
             WHERE attachment_id = ?1
               AND transfer_state <> 'completed'
               AND terminal_failure_code IS NULL",
            [attachment_id],
        )
        .map_err(|error| DbError::sqlite("quarantine unsupported attachment", error))?;
    if updated != 1 {
        return Err(DbError::sqlite(
            "quarantine unsupported attachment",
            "attachment was not retryable",
        ));
    }
    record_media_diagnostic(
        &transaction,
        attachment_id,
        "MEDIA_SOURCE_UNSUPPORTED",
        "local_validation",
        "contract",
        None,
        occurred_at_ms,
    )?;
    transaction
        .commit()
        .map_err(|error| DbError::sqlite("commit unsupported attachment quarantine", error))
}

fn record_media_diagnostic(
    transaction: &Transaction<'_>,
    attachment_id: &str,
    code: &str,
    failure_stage: &str,
    failure_category: &str,
    fallback_from: Option<&str>,
    occurred_at_ms: i64,
) -> Result<(), DbError> {
    let attachment_id_hash = format!("{:x}", Sha256::digest(attachment_id.as_bytes()));
    let diagnostic_id = format!("{code}:{attachment_id_hash}");
    let redacted_json = serde_json::json!({
        "attachment_id_hash": attachment_id_hash,
        "stage": failure_stage,
        "category": failure_category,
        "fallback_from": fallback_from,
    })
    .to_string();
    transaction
        .execute(
            "INSERT INTO diagnostic_events (
                diagnostic_id, occurred_at_ms, level, code, redacted_json
             ) VALUES (?1, ?2, 'error', ?3, ?4)
             ON CONFLICT(diagnostic_id) DO UPDATE SET
                occurred_at_ms = excluded.occurred_at_ms,
                level = excluded.level,
                code = excluded.code,
                redacted_json = excluded.redacted_json",
            params![diagnostic_id, occurred_at_ms, code, redacted_json],
        )
        .map(|_| ())
        .map_err(|error| DbError::sqlite("record media diagnostic", error))
}

pub(crate) fn record_terminal_media_diagnostic(
    connection: &Connection,
    subject_id: &str,
    code: &str,
) -> Result<(), DbError> {
    if subject_id.is_empty()
        || subject_id.len() > 255
        || !matches!(code, "SCREEN_LOCAL_MANIFEST_INVALID")
    {
        return Err(DbError::sqlite(
            "validate terminal media diagnostic",
            "terminal media diagnostic does not match the fixed contract",
        ));
    }
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| DbError::sqlite("start terminal media diagnostic", error))?;
    let occurred_at_ms = i64::try_from(
        OffsetDateTime::now_utc()
            .unix_timestamp_nanos()
            .div_euclid(1_000_000),
    )
    .unwrap_or(i64::MAX);
    record_media_diagnostic(
        &transaction,
        subject_id,
        code,
        "local_validation",
        "contract",
        None,
        occurred_at_ms,
    )?;
    transaction
        .commit()
        .map_err(|error| DbError::sqlite("commit terminal media diagnostic", error))
}

pub(crate) fn remember_screenshot_request(
    connection: &Connection,
    request_id: &str,
) -> Result<(), DbError> {
    validate_screenshot_request_id(request_id)?;
    let handled_at_ms = i64::try_from(
        OffsetDateTime::now_utc()
            .unix_timestamp_nanos()
            .div_euclid(1_000_000),
    )
    .unwrap_or(i64::MAX);
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| DbError::sqlite("start screenshot request history", error))?;
    transaction
        .execute(
            "INSERT INTO handled_screenshot_requests (request_id, handled_at_ms)
             VALUES (?1, ?2)
             ON CONFLICT(request_id) DO NOTHING",
            params![request_id, handled_at_ms],
        )
        .map_err(|error| DbError::sqlite("remember screenshot request", error))?;
    transaction
        .execute(
            "DELETE FROM handled_screenshot_requests
             WHERE request_id IN (
                 SELECT request_id FROM handled_screenshot_requests
                 ORDER BY handled_at_ms DESC, request_id DESC
                 LIMIT -1 OFFSET 10000
             )",
            [],
        )
        .map_err(|error| DbError::sqlite("prune screenshot request history", error))?;
    transaction
        .commit()
        .map_err(|error| DbError::sqlite("commit screenshot request history", error))
}

pub(crate) fn screenshot_request_was_handled(
    connection: &Connection,
    request_id: &str,
) -> Result<bool, DbError> {
    validate_screenshot_request_id(request_id)?;
    connection
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM handled_screenshot_requests WHERE request_id = ?1
             )",
            [request_id],
            |row| row.get(0),
        )
        .map_err(|error| DbError::sqlite("read screenshot request history", error))
}

fn validate_screenshot_request_id(request_id: &str) -> Result<(), DbError> {
    let parsed = Uuid::parse_str(request_id).map_err(|_| {
        DbError::sqlite(
            "validate screenshot request identifier",
            "screenshot request identifier must be a UUID",
        )
    })?;
    if parsed.hyphenated().to_string() != request_id {
        return Err(DbError::sqlite(
            "validate screenshot request identifier",
            "screenshot request identifier must be a canonical lowercase UUID",
        ));
    }
    Ok(())
}

fn communication_collector_key(source: &str) -> Option<&'static str> {
    match source {
        "communication.wechat" | "wechat" => Some("communication.wechat"),
        "communication.messages" | "messages" => Some("communication.messages"),
        _ => None,
    }
}

fn persist_terminal_media_collector_failure(
    transaction: &Transaction<'_>,
    collector_key: &str,
    code: &str,
    occurred_at_ms: i64,
) -> Result<(), DbError> {
    transaction
        .execute(
            "UPDATE collector_states
             SET status = 'degraded',
                 last_error_code = ?2,
                 updated_at_ms = MAX(updated_at_ms, ?3)
             WHERE collector_key = ?1
               AND status <> 'disabled'
               AND (
                    last_error_code IS NULL
                    OR last_error_code IN (
                        'COMMUNICATION_MEDIA_UPLOAD_FAILED',
                        'PHOTOS_UPLOAD_FAILED',
                        'SCREEN_UPLOAD_FAILED',
                        'SCREEN_UPLOAD_TIMEOUT',
                        'MEDIA_CYCLE_TIMEOUT'
                    )
               )",
            params![collector_key, code, occurred_at_ms],
        )
        .map(|_| ())
        .map_err(|error| DbError::sqlite("persist terminal media Collector failure", error))
}

pub(crate) fn cleanup_completed_communication_attachments_batch(
    connection: &Connection,
    spool_root: &Path,
    cutoff_ms: i64,
    after_file_name: Option<&str>,
) -> Result<(u64, Option<String>), DbError> {
    let mut statement = connection
        .prepare(
            "SELECT DISTINCT current.spool_relative_path
             FROM attachment_spool AS current
             WHERE current.transfer_state = 'completed'
               AND current.completed_at_ms IS NOT NULL
               AND current.completed_at_ms <= ?1
               AND (?2 IS NULL OR current.spool_relative_path > ?2)
               AND NOT EXISTS (
                   SELECT 1 FROM attachment_spool AS other
                   WHERE other.spool_relative_path = current.spool_relative_path
                     AND (
                         other.transfer_state <> 'completed'
                         OR other.completed_at_ms IS NULL
                         OR other.completed_at_ms > ?1
                     )
               )
             ORDER BY current.spool_relative_path
             LIMIT 32",
        )
        .map_err(|error| DbError::sqlite("prepare completed attachment cleanup", error))?;
    let files = statement
        .query_map(rusqlite::params![cutoff_ms, after_file_name], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|error| DbError::sqlite("query completed attachment cleanup", error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| DbError::sqlite("read completed attachment cleanup", error))?;
    let mut removed = 0_u64;
    for file_name in &files {
        validate_spool_file_name(file_name)?;
        match remove_communication_spool_file(spool_root, file_name) {
            Ok(()) => removed = removed.saturating_add(1),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(DbError::sqlite(
                    "delete completed communication attachment",
                    error,
                ));
            }
        }
    }
    Ok((removed, files.last().cloned()))
}

pub(crate) fn communication_media_storage_entries(
    connection: &Connection,
) -> Result<Vec<(String, u64, bool)>, DbError> {
    let mut statement = connection
        .prepare(
            "SELECT spool_relative_path,
                    MAX(size_bytes),
                    MIN(CASE
                        WHEN transfer_state = 'completed' AND completed_at_ms IS NOT NULL
                        THEN 1 ELSE 0
                    END) AS completed_only
             FROM attachment_spool
             GROUP BY spool_relative_path",
        )
        .map_err(|error| DbError::sqlite("prepare communication media statistics", error))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, bool>(2)?,
            ))
        })
        .map_err(|error| DbError::sqlite("query communication media statistics", error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| DbError::sqlite("read communication media statistics", error))?;
    let mut entries = Vec::with_capacity(rows.len());
    for (file_name, size_bytes, completed_only) in rows {
        validate_spool_file_name(&file_name)?;
        let size_bytes = u64::try_from(size_bytes).map_err(|_| {
            DbError::sqlite(
                "read communication media statistics",
                "communication attachment size must be non-negative",
            )
        })?;
        entries.push((file_name, size_bytes, completed_only));
    }
    Ok(entries)
}

pub(crate) fn communication_media_storage_stats(
    spool_root: &Path,
    entries: Vec<(String, u64, bool)>,
) -> Result<crate::CommunicationMediaStorageStats, DbError> {
    let directory = rustix::fs::open(
        spool_root,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| DbError::sqlite("open communication spool for statistics", error))?;
    let mut stats = crate::CommunicationMediaStorageStats::default();
    for (file_name, _expected_size, completed_only) in entries {
        let file = match rustix::fs::openat(
            &directory,
            file_name.as_str(),
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        ) {
            Ok(file) => File::from(file),
            Err(error) if error == rustix::io::Errno::NOENT => continue,
            Err(error) => {
                return Err(DbError::sqlite(
                    "open communication media for statistics",
                    std::io::Error::from(error),
                ));
            }
        };
        let metadata = file
            .metadata()
            .map_err(|error| DbError::sqlite("inspect communication media statistics", error))?;
        if !metadata.is_file() {
            return Err(DbError::sqlite(
                "inspect communication media statistics",
                "communication spool entry must be a regular file",
            ));
        }
        if completed_only {
            stats.completed_file_count = stats.completed_file_count.saturating_add(1);
            stats.completed_bytes = stats.completed_bytes.saturating_add(metadata.len());
        } else {
            stats.protected_file_count = stats.protected_file_count.saturating_add(1);
            stats.protected_bytes = stats.protected_bytes.saturating_add(metadata.len());
        }
    }
    Ok(stats)
}

fn remove_communication_spool_file(
    spool_root: &Path,
    file_name: &str,
) -> Result<(), std::io::Error> {
    let directory = rustix::fs::open(
        spool_root,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map(File::from)?;
    rustix::fs::unlinkat(&directory, file_name, rustix::fs::AtFlags::empty())
        .map_err(std::io::Error::from)
}

fn format_timestamp(milliseconds: i64) -> Result<String, DbError> {
    let nanos = i128::from(milliseconds)
        .checked_mul(1_000_000)
        .ok_or_else(|| DbError::sqlite("format event timestamp", "timestamp overflow"))?;
    let timestamp = OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .map_err(|error| DbError::sqlite("format event timestamp", error))?;
    timestamp
        .format(&Rfc3339)
        .map_err(|error| DbError::sqlite("format event timestamp", error))
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
        "SELECT singleton_id, device_id, workspace_id, credential_ref,
                credential_generation, cloud_api_origin, applied_control_revision, paired_at_ms
         FROM pairing_state LIMIT 1",
        "SELECT event_id, workspace_id, device_id, event_type, source, schema_version,
                occurred_at_ms, created_at_ms, sensitivity, payload_json,
                attachment_refs_json, idempotency_key
         FROM events_local LIMIT 1",
        "SELECT outbox_id, event_id, state, created_at_ms FROM sync_outbox LIMIT 1",
        "SELECT diagnostic_id, occurred_at_ms, level, code, redacted_json
         FROM diagnostic_events LIMIT 1",
        "SELECT account_id, external_conversation_id, scope, member_count,
                created_at_ms, updated_at_ms
         FROM communication_conversations LIMIT 1",
        "SELECT local_message_id, event_id, account_id, external_conversation_id,
                source_sequence, source_key, direction, kind, occurred_at_ms, text_body,
                created_at_ms
         FROM communication_messages LIMIT 1",
        "SELECT account_id, external_conversation_id, last_source_sequence, updated_at_ms
         FROM communication_cursors LIMIT 1",
        "SELECT attachment_id, local_message_id, kind, sha256, size_bytes, mime_type,
                spool_relative_path, transfer_state, created_at_ms, completed_at_ms
         FROM attachment_spool LIMIT 1",
        "SELECT account_id, source_key, tombstoned_at_ms FROM local_tombstones LIMIT 1",
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
        Err(DbError::retryable(
            "checkpoint WAL",
            "database remained busy",
        ))
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
