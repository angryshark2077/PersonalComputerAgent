use std::{
    collections::HashSet,
    fs::File,
    io::{Read, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    path::Path,
};

use pca_domain::{
    AgentStatus, BridgeStatus, CollectorState, CollectorStatus, CommunicationAttachment,
    ConversationScope, Direction, EventCommit, EventEnvelope, MessageKind, Sensitivity,
};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use sha2::{Digest, Sha256};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

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
    migrations::MAX_SUPPORTED_SCHEMA_VERSION, CommunicationMessageCommit, DbError, DbHealth,
    PairingState, PendingCommunicationAttachment,
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
    let spool_references = validate_spool_references(spool_root, commit)?;
    let source_sequence = i64::try_from(commit.source_sequence).map_err(|_| {
        DbError::sqlite(
            "validate communication source sequence",
            "source sequence exceeds SQLite integer range",
        )
    })?;

    let transaction = connection
        .transaction()
        .map_err(|error| DbError::sqlite("start communication message transaction", error))?;
    if communication_message_exists(&transaction, commit)? {
        validate_existing_communication_event(&transaction, &serialized)?;
        validate_existing_outbox(&transaction, &serialized)?;
        validate_existing_attachment_spool(&transaction, commit)?;
        return Ok(());
    }

    insert_event(&transaction, &serialized)?;
    upsert_communication_conversation(&transaction, commit)?;
    let local_message_id = insert_communication_message(&transaction, commit, source_sequence)?;
    for spool_reference in &spool_references {
        insert_attachment_spool(&transaction, local_message_id, spool_reference)?;
    }
    insert_stable_outbox(&transaction, &serialized)?;
    advance_communication_cursor(&transaction, commit, source_sequence)?;
    transaction
        .commit()
        .map_err(|error| DbError::sqlite("commit communication message transaction", error))
}

fn validate_communication_commit(commit: &CommunicationMessageCommit) -> Result<(), DbError> {
    if commit.account_id.trim().is_empty()
        || commit.event.event_id.trim().is_empty()
        || commit.event.event_type != "communication.message_recorded"
        || commit.event.source != "communication.wechat"
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

fn communication_message_exists(
    transaction: &Transaction<'_>,
    commit: &CommunicationMessageCommit,
) -> Result<bool, DbError> {
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
        return Ok(false);
    };
    if event_id == commit.event.event_id
        && conversation_id == commit.message.conversation_id()
        && u64::try_from(source_sequence).ok() == Some(commit.source_sequence)
    {
        Ok(true)
    } else {
        Err(DbError::sqlite(
            "validate existing communication message",
            "source key conflicts with a different immutable communication message",
        ))
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
        return Err(DbError::sqlite(
            "validate existing attachment spool",
            "source key conflicts with a different attachment spool count",
        ));
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
            return Err(DbError::sqlite(
                "validate existing attachment spool",
                "source key conflicts with different immutable attachment spool metadata",
            ));
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
    transaction
        .execute(
            "INSERT INTO communication_messages (
                event_id, account_id, external_conversation_id, source_sequence, source_key,
                direction, kind, occurred_at_ms, text_body, created_at_ms
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7,
                CAST(unixepoch(?8, 'subsec') * 1000 AS INTEGER), ?9,
                CAST(unixepoch(?10, 'subsec') * 1000 AS INTEGER)
             )",
            params![
                commit.event.event_id,
                commit.account_id,
                commit.message.conversation_id(),
                source_sequence,
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
    .map_err(|error| DbError::sqlite("open communication spool root", error))?;
    let root_metadata = directory
        .metadata()
        .map_err(|error| DbError::sqlite("inspect communication spool root", error))?;
    if !root_metadata.is_dir() || root_metadata.mode() & 0o077 != 0 {
        return Err(DbError::sqlite(
            "inspect communication spool root",
            "communication spool root is not owner-private",
        ));
    }

    let file = rustix::fs::openat(
        &directory,
        file_name,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| DbError::sqlite("open communication spool file", error))?;
    if !file
        .metadata()
        .map_err(|error| DbError::sqlite("inspect communication spool file", error))?
        .is_file()
    {
        return Err(DbError::sqlite(
            "inspect communication spool file",
            "communication spool entry must be a regular file",
        ));
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
        .ok_or_else(|| {
            DbError::sqlite(
                "validate existing communication event",
                "stable communication event conflicts with different immutable fields",
            )
        })?;
    let mut existing_payload =
        serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&existing_payload)
            .map_err(|error| DbError::Serialization(error.to_string()))?;
    let mut candidate_payload = serialized.event.payload.clone();
    for field in ["sender_id", "sender_display_name"] {
        existing_payload.remove(field);
        candidate_payload.remove(field);
    }
    if existing_payload == candidate_payload {
        Ok(())
    } else {
        Err(DbError::sqlite(
            "validate existing communication event",
            "stable communication event conflicts with different message content",
        ))
    }
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
    connection
        .execute(
            "INSERT INTO collector_states (
                collector_key, status, version, desired_revision, applied_revision,
                last_event_at_ms, last_health_at_ms, last_error_code,
                created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(collector_key) DO UPDATE SET
                status = excluded.status,
                version = excluded.version,
                desired_revision = excluded.desired_revision,
                applied_revision = excluded.applied_revision,
                last_event_at_ms = excluded.last_event_at_ms,
                last_health_at_ms = excluded.last_health_at_ms,
                last_error_code = excluded.last_error_code,
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
            ],
        )
        .map(|_| ())
        .map_err(|error| DbError::sqlite("upsert Collector state", error))
}

pub(crate) fn load_pairing_state(connection: &Connection) -> Result<Option<PairingState>, DbError> {
    connection
        .query_row(
            "SELECT device_id, workspace_id, credential_ref, credential_generation,
                    cloud_api_origin, applied_control_revision, paired_at_ms
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
                })
            },
        )
        .optional()
        .map_err(|error| DbError::sqlite("load pairing state", error))
}

pub(crate) fn save_pairing_state(
    connection: &Connection,
    state: &PairingState,
) -> Result<(), DbError> {
    connection
        .execute(
            "INSERT INTO pairing_state (
                singleton_id, device_id, workspace_id, credential_ref,
                credential_generation, cloud_api_origin, applied_control_revision, paired_at_ms
             ) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(singleton_id) DO UPDATE SET
                device_id = excluded.device_id,
                workspace_id = excluded.workspace_id,
                credential_ref = excluded.credential_ref,
                credential_generation = excluded.credential_generation,
                cloud_api_origin = excluded.cloud_api_origin,
                applied_control_revision = excluded.applied_control_revision,
                paired_at_ms = excluded.paired_at_ms",
            params![
                state.device_id,
                state.workspace_id,
                state.credential_ref,
                state.credential_generation,
                state.cloud_api_origin,
                state.applied_control_revision,
                state.paired_at_ms,
            ],
        )
        .map(|_| ())
        .map_err(|error| DbError::sqlite("save pairing state", error))
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

pub(crate) fn clear_pairing_state(connection: &Connection) -> Result<(), DbError> {
    connection
        .execute("DELETE FROM pairing_state WHERE singleton_id = 1", [])
        .map(|_| ())
        .map_err(|error| DbError::sqlite("clear pairing state", error))
}

pub(crate) fn clear_pairing_state_and_disable_sensitive_collectors(
    connection: &mut Connection,
) -> Result<(), DbError> {
    let transaction = connection
        .transaction()
        .map_err(|error| DbError::sqlite("start pairing revocation transaction", error))?;
    transaction
        .execute("DELETE FROM pairing_state WHERE singleton_id = 1", [])
        .map_err(|error| DbError::sqlite("clear pairing state", error))?;
    let now_ms = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX);
    for collector_key in ["network", "communication.wechat"] {
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
            "SELECT COUNT(*) FROM sync_outbox WHERE state <> 'acked'",
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
            "SELECT e.event_id, e.workspace_id, e.device_id, e.event_type, e.source,
                    e.schema_version, e.occurred_at_ms, e.created_at_ms, e.payload_json,
                    e.idempotency_key
             FROM sync_outbox AS o
             INNER JOIN events_local AS e ON e.event_id = o.event_id
             WHERE o.state = 'pending'
               AND e.event_type IN (
                   'system.metric_sampled',
                   'system.health_changed',
                   'collector.status_changed'
               )
               AND e.sensitivity = 'normal'
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
            sensitivity: Sensitivity::Normal,
            payload,
            attachment_refs: Vec::new(),
            idempotency_key,
        })
    })
    .collect()
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
                   AND state = 'pending'
                   AND EXISTS (
                       SELECT 1 FROM events_local
                       WHERE events_local.event_id = sync_outbox.event_id
                         AND events_local.event_type IN (
                             'system.metric_sampled',
                             'system.health_changed',
                             'collector.status_changed'
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
    }
    transaction
        .commit()
        .map_err(|error| DbError::sqlite("commit system event acknowledgement", error))
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
             WHERE o.state = 'pending'
               AND e.event_type IN (
                   'communication.message_recorded',
                   'communication.conversation_observed',
                   'communication.message_sender_observed'
               )
               AND e.source = 'communication.wechat'
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
                   AND state = 'pending'
                   AND EXISTS (
                       SELECT 1 FROM events_local AS e
                       WHERE e.event_id = sync_outbox.event_id
                         AND e.event_type IN (
                             'communication.message_recorded',
                             'communication.conversation_observed',
                             'communication.message_sender_observed'
                         )
                         AND e.source = 'communication.wechat'
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
    }
    transaction
        .commit()
        .map_err(|error| DbError::sqlite("commit communication event acknowledgement", error))
}

pub(crate) fn load_pending_communication_attachments(
    connection: &Connection,
    spool_root: &Path,
    limit: u16,
) -> Result<Vec<PendingCommunicationAttachment>, DbError> {
    let mut statement = connection
        .prepare(
            "SELECT m.event_id, s.attachment_id, s.sha256, s.size_bytes, s.mime_type,
                    s.spool_relative_path
             FROM attachment_spool AS s
             INNER JOIN communication_messages AS m
                ON m.local_message_id = s.local_message_id
             INNER JOIN sync_outbox AS o ON o.event_id = m.event_id
             WHERE o.state = 'acked' AND s.transfer_state <> 'completed'
             ORDER BY CASE s.transfer_state WHEN 'failed' THEN 1 ELSE 0 END,
                      s.created_at_ms, s.attachment_id
             LIMIT ?1",
        )
        .map_err(|error| DbError::sqlite("prepare pending attachment query", error))?;
    let rows = statement
        .query_map([i64::from(limit)], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })
        .map_err(|error| DbError::sqlite("query pending attachments", error))?;
    let mut pending = Vec::new();
    for row in rows {
        let (event_id, attachment_id, sha256, size_bytes, mime_type, file_name) =
            row.map_err(|error| DbError::sqlite("read pending attachment", error))?;
        let expected_size = u64::try_from(size_bytes).map_err(|_| {
            DbError::sqlite("read pending attachment", "attachment size is invalid")
        })?;
        let mut file = open_communication_spool_file(spool_root, &file_name)?;
        let mut hasher = Sha256::new();
        let mut bytes_read = 0_u64;
        let mut buffer = vec![0_u8; 1024 * 1024];
        loop {
            let read = file
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
        if bytes_read != expected_size || format!("{:x}", hasher.finalize()) != sha256 {
            return Err(DbError::sqlite(
                "verify pending attachment body",
                "attachment body does not match immutable manifest",
            ));
        }
        file.seek(SeekFrom::Start(0))
            .map_err(|error| DbError::sqlite("rewind pending attachment body", error))?;
        pending.push(PendingCommunicationAttachment {
            event_id,
            attachment_id,
            sha256,
            size_bytes: expected_size,
            mime_type,
            file,
        });
    }
    Ok(pending)
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
             WHERE attachment_id = ?1 AND transfer_state <> 'completed'",
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
) -> Result<(), DbError> {
    let updated = connection
        .execute(
            "UPDATE attachment_spool
             SET transfer_state = 'failed'
             WHERE attachment_id = ?1 AND transfer_state <> 'completed'",
            [attachment_id],
        )
        .map_err(|error| DbError::sqlite("defer communication attachment", error))?;
    if updated == 1 {
        Ok(())
    } else {
        Err(DbError::sqlite(
            "defer communication attachment",
            "attachment was not pending",
        ))
    }
}

pub(crate) fn cleanup_completed_communication_attachments(
    connection: &Connection,
    spool_root: &Path,
    cutoff_ms: i64,
) -> Result<u64, DbError> {
    let mut statement = connection
        .prepare(
            "SELECT DISTINCT current.spool_relative_path
             FROM attachment_spool AS current
             WHERE current.transfer_state = 'completed'
               AND current.completed_at_ms IS NOT NULL
               AND current.completed_at_ms <= ?1
               AND NOT EXISTS (
                   SELECT 1 FROM attachment_spool AS other
                   WHERE other.spool_relative_path = current.spool_relative_path
                     AND (
                         other.transfer_state <> 'completed'
                         OR other.completed_at_ms IS NULL
                         OR other.completed_at_ms > ?1
                     )
               )",
        )
        .map_err(|error| DbError::sqlite("prepare completed attachment cleanup", error))?;
    let files = statement
        .query_map([cutoff_ms], |row| row.get::<_, String>(0))
        .map_err(|error| DbError::sqlite("query completed attachment cleanup", error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| DbError::sqlite("read completed attachment cleanup", error))?;
    let mut removed = 0_u64;
    for file_name in files {
        validate_spool_file_name(&file_name)?;
        match remove_communication_spool_file(spool_root, &file_name) {
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
    Ok(removed)
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
        Err(DbError::sqlite("checkpoint WAL", "database remained busy"))
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
