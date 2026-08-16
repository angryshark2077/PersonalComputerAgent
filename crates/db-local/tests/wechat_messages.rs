use std::{
    os::unix::fs::{symlink, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use pca_db_local::{
    CommunicationAttachmentSpoolReference, CommunicationMessageCommit, DbActorHandle, DbError,
};
use pca_domain::{
    CommunicationAttachment, CommunicationMessageRecorded, CommunicationMessageRecordedInput,
    ConversationScope, Direction, EventEnvelope, MessageKind, Sensitivity,
};
use rusqlite::Connection;
use serde_json::{Map, Value};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TempDirectory(PathBuf);

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn database_path() -> (TempDirectory, PathBuf) {
    let identifier = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "pca-wechat-messages-{}-{identifier}",
        std::process::id()
    ));
    std::fs::create_dir(&directory).expect("temporary directory");
    let path = directory.join("agent.sqlite3");
    (TempDirectory(directory), path)
}

fn valid_commit(database: &Path) -> CommunicationMessageCommit {
    let spool_root = DbActorHandle::communication_spool_root(database);
    std::fs::create_dir_all(&spool_root).expect("private spool root");
    let sha256 = "a".repeat(64);
    std::fs::write(spool_root.join(&sha256), b"private media").expect("private spool file");

    let attachment = CommunicationAttachment::try_new(
        "attachment-1".to_owned(),
        MessageKind::Image,
        sha256.clone(),
        1,
        "image/png".to_owned(),
    )
    .expect("valid attachment");
    let message = CommunicationMessageRecorded::try_new(CommunicationMessageRecordedInput {
        message_id: "message-1".to_owned(),
        conversation_id: "conversation-1".to_owned(),
        sender_id: "wxid_sender".to_owned(),
        sender_display_name: "Sender".to_owned(),
        source_key: "source-key-1".to_owned(),
        occurred_at: "2026-08-02T12:00:00Z".to_owned(),
        direction: Direction::Incoming,
        kind: MessageKind::Image,
        conversation: ConversationScope::Direct,
        text: None,
        attachments: vec![attachment],
    })
    .expect("valid message");
    let payload = serde_json::to_value(&message).expect("message payload");
    let Value::Object(payload) = payload else {
        panic!("message payload is an object");
    };

    CommunicationMessageCommit {
        account_id: "account-1".to_owned(),
        source_sequence: 1,
        cursor_sequence: 1,
        event: EventEnvelope {
            event_id: "event-1".to_owned(),
            workspace_id: "workspace-1".to_owned(),
            device_id: "device-1".to_owned(),
            event_type: "communication.message_recorded".to_owned(),
            source: "communication.wechat".to_owned(),
            schema_version: 1,
            occurred_at: "2026-08-02T12:00:00Z".to_owned(),
            created_at: "2026-08-02T12:00:01Z".to_owned(),
            sensitivity: Sensitivity::High,
            payload,
            attachment_refs: vec!["attachment-1".to_owned()],
            idempotency_key: Some("source-key-1".to_owned()),
        },
        metadata_events: Vec::new(),
        message,
        attachment_spool: vec![CommunicationAttachmentSpoolReference {
            attachment_id: "attachment-1".to_owned(),
            file_name: sha256,
        }],
    }
}

fn commit_with(
    database: &Path,
    event_id: &str,
    message_id: &str,
    conversation_id: &str,
    source_key: &str,
    attachment_id: &str,
) -> CommunicationMessageCommit {
    let mut commit = valid_commit(database);
    let attachment = CommunicationAttachment::try_new(
        attachment_id.to_owned(),
        MessageKind::Image,
        "b".repeat(64),
        1,
        "image/png".to_owned(),
    )
    .expect("valid attachment");
    let spool_root = DbActorHandle::communication_spool_root(database);
    std::fs::write(spool_root.join(attachment.sha256()), b"private media")
        .expect("private spool file");
    commit.source_sequence = 2;
    commit.message = CommunicationMessageRecorded::try_new(CommunicationMessageRecordedInput {
        message_id: message_id.to_owned(),
        conversation_id: conversation_id.to_owned(),
        sender_id: "wxid_sender".to_owned(),
        sender_display_name: "Sender".to_owned(),
        source_key: source_key.to_owned(),
        occurred_at: "2026-08-02T12:00:00Z".to_owned(),
        direction: Direction::Incoming,
        kind: MessageKind::Image,
        conversation: ConversationScope::Direct,
        text: None,
        attachments: vec![attachment],
    })
    .expect("valid message");
    let Value::Object(payload) = serde_json::to_value(&commit.message).expect("message payload")
    else {
        panic!("message payload is an object");
    };
    event_id.clone_into(&mut commit.event.event_id);
    commit.event.payload = payload;
    commit.event.attachment_refs = vec![attachment_id.to_owned()];
    commit.event.idempotency_key = Some(source_key.to_owned());
    commit.attachment_spool[0].file_name = "b".repeat(64);
    commit
}

fn row_counts(database: &Path) -> (u64, u64, u64, u64, u64, u64) {
    let connection = Connection::open(database).expect("inspect local database");
    connection
        .query_row(
            "SELECT
                (SELECT COUNT(*) FROM events_local),
                (SELECT COUNT(*) FROM communication_conversations),
                (SELECT COUNT(*) FROM communication_messages),
                (SELECT COUNT(*) FROM sync_outbox),
                (SELECT COUNT(*) FROM communication_cursors),
                (SELECT COUNT(*) FROM attachment_spool)",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .expect("read local row counts")
}

fn cursor_sequence(database: &Path) -> u64 {
    let connection = Connection::open(database).expect("inspect local database");
    connection
        .query_row(
            "SELECT last_source_sequence FROM communication_cursors
             WHERE account_id = 'account-1' AND external_conversation_id = 'conversation-1'",
            [],
            |row| row.get(0),
        )
        .expect("read communication cursor")
}

fn system_event() -> EventEnvelope {
    let mut payload = Map::new();
    payload.insert(
        "metric_group".to_owned(),
        Value::String("cpu_memory".to_owned()),
    );
    EventEnvelope {
        event_id: "system-event-1".to_owned(),
        workspace_id: "workspace-1".to_owned(),
        device_id: "device-1".to_owned(),
        event_type: "system.metric_sampled".to_owned(),
        source: "system".to_owned(),
        schema_version: 1,
        occurred_at: "2026-08-02T12:00:00Z".to_owned(),
        created_at: "2026-08-02T12:00:01Z".to_owned(),
        sensitivity: Sensitivity::Normal,
        payload,
        attachment_refs: Vec::new(),
        idempotency_key: Some("system-event-1".to_owned()),
    }
}

#[tokio::test]
async fn source_key_creates_one_message_outbox_cursor_and_spool_reference() {
    let (_directory, path) = database_path();
    let store = DbActorHandle::open(&path, "0.1.0")
        .await
        .expect("open database");
    let commit = valid_commit(&path);

    store
        .commit_communication_message(&commit)
        .await
        .expect("commit communication message");
    store
        .commit_communication_message(&commit)
        .await
        .expect("deduplicate repeated source key");
    assert_eq!(row_counts(&path), (1, 1, 1, 1, 1, 1));
    assert_eq!(
        std::fs::metadata(DbActorHandle::communication_spool_root(&path))
            .expect("private spool root metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );

    let pending = store
        .load_pending_communication_events(1)
        .await
        .expect("load communication event");
    assert_eq!(pending, vec![commit.event.clone()]);
    store
        .acknowledge_communication_events(std::slice::from_ref(&commit.event.event_id))
        .await
        .expect("acknowledge communication event");
    assert!(store
        .load_pending_communication_events(1)
        .await
        .expect("load acknowledged communication events")
        .is_empty());
}

#[tokio::test]
async fn replay_lifts_the_durable_cursor_checkpoint_without_rewriting_source_sequence() {
    let (_directory, path) = database_path();
    let store = DbActorHandle::open(&path, "0.2.2")
        .await
        .expect("open database");
    let mut commit = valid_commit(&path);
    commit.cursor_sequence = 0;
    store
        .commit_communication_message(&commit)
        .await
        .expect("commit behind a pending outbound row");

    commit.cursor_sequence = 1;
    store
        .commit_communication_message(&commit)
        .await
        .expect("replay after the pending row becomes final");
    store.shutdown().await.expect("close database");

    let connection = Connection::open(&path).expect("reopen database");
    let (source_sequence, cursor_sequence) = connection
        .query_row(
            "SELECT source_sequence, cursor_sequence FROM communication_messages",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .expect("read persisted cursor checkpoint");
    assert_eq!((source_sequence, cursor_sequence), (1, 1));
}

#[tokio::test]
async fn cloud_completed_media_is_deleted_only_after_the_local_retention_cutoff() {
    let (_directory, path) = database_path();
    let store = DbActorHandle::open(&path, "0.1.0")
        .await
        .expect("open database");
    let commit = valid_commit(&path);
    let spool_file =
        DbActorHandle::communication_spool_root(&path).join(&commit.attachment_spool[0].file_name);
    store
        .commit_communication_message(&commit)
        .await
        .expect("commit communication message");
    store
        .acknowledge_communication_events(std::slice::from_ref(&commit.event.event_id))
        .await
        .expect("acknowledge communication event");
    store
        .complete_communication_attachment("attachment-1")
        .await
        .expect("complete attachment");

    let before_cleanup = store
        .communication_media_storage_stats()
        .await
        .expect("measure completed media");
    assert_eq!(before_cleanup.completed_file_count, 1);
    assert_eq!(
        before_cleanup.completed_bytes,
        std::fs::metadata(&spool_file).unwrap().len()
    );
    assert_eq!(before_cleanup.protected_file_count, 0);

    assert_eq!(
        store
            .cleanup_completed_communication_attachments(i64::MIN)
            .await
            .expect("retain recent body"),
        0
    );
    assert!(spool_file.is_file());
    assert_eq!(
        store
            .cleanup_completed_communication_attachments(i64::MAX)
            .await
            .expect("delete expired body"),
        1
    );
    assert!(!spool_file.exists());
    assert_eq!(
        store
            .communication_media_storage_stats()
            .await
            .expect("measure removed media")
            .completed_bytes,
        0
    );
}

#[tokio::test]
async fn cleanup_never_deletes_pending_failed_or_shared_media() {
    let (_directory, path) = database_path();
    let store = DbActorHandle::open(&path, "0.1.0")
        .await
        .expect("open database");
    let commit = valid_commit(&path);
    let spool_root = DbActorHandle::communication_spool_root(&path);
    let shared_file = spool_root.join(&commit.attachment_spool[0].file_name);
    let pending_sha256 = "b".repeat(64);
    let failed_sha256 = "c".repeat(64);
    let pending_file = spool_root.join(&pending_sha256);
    let failed_file = spool_root.join(&failed_sha256);
    std::fs::write(&pending_file, b"pending media").expect("pending spool file");
    std::fs::write(&failed_file, b"failed media").expect("failed spool file");

    store
        .commit_communication_message(&commit)
        .await
        .expect("commit communication message");
    store
        .acknowledge_communication_events(std::slice::from_ref(&commit.event.event_id))
        .await
        .expect("acknowledge communication event");
    store
        .complete_communication_attachment("attachment-1")
        .await
        .expect("complete attachment");

    let connection = Connection::open(&path).expect("open fixture database");
    connection
        .execute(
            "INSERT INTO attachment_spool (
                attachment_id, local_message_id, kind, sha256, size_bytes, mime_type,
                spool_relative_path, transfer_state, created_at_ms, completed_at_ms
             ) VALUES (?1, 1, 'image', ?2, 1, 'image/png', ?2, ?3, 1, NULL)",
            rusqlite::params!["shared-pending", "a".repeat(64), "pending"],
        )
        .expect("insert pending shared reference");
    connection
        .execute(
            "INSERT INTO attachment_spool (
                attachment_id, local_message_id, kind, sha256, size_bytes, mime_type,
                spool_relative_path, transfer_state, created_at_ms, completed_at_ms
             ) VALUES (?1, 1, 'image', ?2, 1, 'image/png', ?2, ?3, 1, NULL)",
            rusqlite::params!["pending-only", pending_sha256, "pending"],
        )
        .expect("insert pending attachment");
    connection
        .execute(
            "INSERT INTO attachment_spool (
                attachment_id, local_message_id, kind, sha256, size_bytes, mime_type,
                spool_relative_path, transfer_state, created_at_ms, completed_at_ms
             ) VALUES (?1, 1, 'image', ?2, 1, 'image/png', ?2, ?3, 1, NULL)",
            rusqlite::params!["failed-only", failed_sha256, "failed"],
        )
        .expect("insert failed attachment");
    drop(connection);

    let protected = store
        .communication_media_storage_stats()
        .await
        .expect("measure protected media");
    assert_eq!(protected.completed_file_count, 0);
    assert_eq!(protected.protected_file_count, 3);

    assert_eq!(
        store
            .cleanup_completed_communication_attachments(i64::MAX)
            .await
            .expect("protect non-completed media"),
        0
    );
    assert!(shared_file.is_file());
    assert!(pending_file.is_file());
    assert!(failed_file.is_file());
}

#[tokio::test]
async fn source_path_outside_private_spool_root_leaves_no_partial_communication_rows() {
    let (_directory, path) = database_path();
    let store = DbActorHandle::open(&path, "0.1.0")
        .await
        .expect("open database");
    let mut commit = valid_commit(&path);
    commit.attachment_spool[0].file_name = "../outside-media.bin".to_owned();

    assert!(store.commit_communication_message(&commit).await.is_err());
    store.shutdown().await.expect("close database");

    assert_eq!(row_counts(&path), (0, 0, 0, 0, 0, 0));
}

#[tokio::test]
async fn direct_spool_file_symlink_is_rejected_without_partial_rows() {
    let (_directory, path) = database_path();
    let store = DbActorHandle::open(&path, "0.1.0")
        .await
        .expect("open database");
    let commit = valid_commit(&path);
    let spool_root = DbActorHandle::communication_spool_root(&path);
    let outside = path
        .parent()
        .expect("database parent")
        .join("outside-media");
    std::fs::write(&outside, b"outside private spool").expect("outside media");
    std::fs::remove_file(spool_root.join(&commit.attachment_spool[0].file_name))
        .expect("remove private spool file");
    symlink(
        &outside,
        spool_root.join(&commit.attachment_spool[0].file_name),
    )
    .expect("replace with symlink");

    assert!(store.commit_communication_message(&commit).await.is_err());
    store.shutdown().await.expect("close database");
    assert_eq!(row_counts(&path), (0, 0, 0, 0, 0, 0));
}

#[tokio::test]
async fn replaced_spool_root_symlink_is_rejected_by_the_hardened_reopen_boundary() {
    let (_directory, path) = database_path();
    let store = DbActorHandle::open(&path, "0.1.0")
        .await
        .expect("open database");
    let commit = valid_commit(&path);
    let spool_root = DbActorHandle::communication_spool_root(&path);
    let moved_root = path
        .parent()
        .expect("database parent")
        .join("moved-spool-root");
    std::fs::rename(&spool_root, &moved_root).expect("move private spool root");
    symlink(&moved_root, &spool_root).expect("replace private spool root with symlink");

    assert!(DbActorHandle::open_communication_spool_file(
        &path,
        &commit.attachment_spool[0].file_name
    )
    .is_err());
    assert!(store.commit_communication_message(&commit).await.is_err());
}

#[tokio::test]
async fn every_media_manifest_requires_exactly_one_spool_reference() {
    let (_directory, path) = database_path();
    let store = DbActorHandle::open(&path, "0.1.0")
        .await
        .expect("open database");
    let mut commit = valid_commit(&path);
    commit.attachment_spool.clear();

    assert!(store.commit_communication_message(&commit).await.is_err());
    store.shutdown().await.expect("close database");
    assert_eq!(row_counts(&path), (0, 0, 0, 0, 0, 0));
}

#[test]
fn uppercase_attachment_hash_is_rejected_before_local_persistence() {
    assert!(CommunicationAttachment::try_new(
        "attachment-1".to_owned(),
        MessageKind::Image,
        "A".repeat(64),
        1,
        "image/png".to_owned(),
    )
    .is_err());
}

#[tokio::test]
async fn attachment_conflict_rolls_back_event_projection_outbox_and_cursor_together() {
    let (_directory, path) = database_path();
    let store = DbActorHandle::open(&path, "0.1.0")
        .await
        .expect("open database");
    let first = valid_commit(&path);
    store
        .commit_communication_message(&first)
        .await
        .expect("commit first message");

    let conflicting = commit_with(
        &path,
        "event-2",
        "message-2",
        "conversation-2",
        "source-key-2",
        "attachment-1",
    );
    assert!(store
        .commit_communication_message(&conflicting)
        .await
        .is_err());
    store.shutdown().await.expect("close database");

    assert_eq!(row_counts(&path), (1, 1, 1, 1, 1, 1));
}

#[tokio::test]
async fn source_key_with_changed_immutable_event_fields_is_rejected() {
    let (_directory, path) = database_path();
    let store = DbActorHandle::open(&path, "0.1.0")
        .await
        .expect("open database");
    let commit = valid_commit(&path);
    store
        .commit_communication_message(&commit)
        .await
        .expect("commit first message");

    let mut conflicting = commit.clone();
    conflicting.event.created_at = "2026-08-02T12:00:02Z".to_owned();
    assert!(store
        .commit_communication_message(&conflicting)
        .await
        .is_err());
}

#[tokio::test]
async fn source_key_replay_allows_sender_metadata_to_be_observed_separately() {
    let (_directory, path) = database_path();
    let store = DbActorHandle::open(&path, "0.1.0")
        .await
        .expect("open database");
    let commit = valid_commit(&path);
    store
        .commit_communication_message(&commit)
        .await
        .expect("commit first message");

    let mut replay = commit.clone();
    replay.message = CommunicationMessageRecorded::try_new(CommunicationMessageRecordedInput {
        message_id: "message-1".to_owned(),
        conversation_id: "conversation-1".to_owned(),
        sender_id: "wxid_resolved_sender".to_owned(),
        sender_display_name: "Resolved Sender".to_owned(),
        source_key: "source-key-1".to_owned(),
        occurred_at: "2026-08-02T12:00:00Z".to_owned(),
        direction: Direction::Incoming,
        kind: MessageKind::Image,
        conversation: ConversationScope::Direct,
        text: None,
        attachments: commit.message.attachments().to_vec(),
    })
    .expect("replayed message with resolved sender");
    let Value::Object(payload) = serde_json::to_value(&replay.message).expect("message payload")
    else {
        panic!("message payload is an object");
    };
    replay.event.payload = payload;

    store
        .commit_communication_message(&replay)
        .await
        .expect("sender metadata does not rewrite immutable message event");
    assert_eq!(row_counts(&path), (1, 1, 1, 1, 1, 1));
}

#[tokio::test]
async fn exact_source_replay_from_a_repaired_device_keeps_the_original_event() {
    let (_directory, path) = database_path();
    let store = DbActorHandle::open(&path, "0.1.0")
        .await
        .expect("open database");
    let commit = valid_commit(&path);
    store
        .commit_communication_message(&commit)
        .await
        .expect("commit original device message");

    let mut repaired_device_replay = commit.clone();
    repaired_device_replay.event.event_id = "event-from-repaired-device".to_owned();
    repaired_device_replay.event.device_id = "device-2".to_owned();
    store
        .commit_communication_message(&repaired_device_replay)
        .await
        .expect("deduplicate exact source replay from repaired device");

    assert_eq!(row_counts(&path), (1, 1, 1, 1, 1, 1));
    let connection = Connection::open(&path).expect("inspect original event identity");
    let identity = connection
        .query_row(
            "SELECT event_id, device_id FROM events_local WHERE event_type = 'communication.message_recorded'",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .expect("read original event identity");
    assert_eq!(identity, ("event-1".to_owned(), "device-1".to_owned()));
}

#[tokio::test]
async fn repaired_device_replay_with_changed_message_content_is_rejected() {
    let (_directory, path) = database_path();
    let store = DbActorHandle::open(&path, "0.1.0")
        .await
        .expect("open database");
    let commit = valid_commit(&path);
    store
        .commit_communication_message(&commit)
        .await
        .expect("commit original device message");

    let mut conflicting = commit.clone();
    conflicting.event.event_id = "event-from-repaired-device".to_owned();
    conflicting.event.device_id = "device-2".to_owned();
    conflicting.message =
        CommunicationMessageRecorded::try_new(CommunicationMessageRecordedInput {
            message_id: "different-message".to_owned(),
            conversation_id: "conversation-1".to_owned(),
            sender_id: "wxid_sender".to_owned(),
            sender_display_name: "Sender".to_owned(),
            source_key: "source-key-1".to_owned(),
            occurred_at: "2026-08-02T12:00:00Z".to_owned(),
            direction: Direction::Incoming,
            kind: MessageKind::Image,
            conversation: ConversationScope::Direct,
            text: None,
            attachments: commit.message.attachments().to_vec(),
        })
        .expect("valid conflicting message");
    let Value::Object(payload) =
        serde_json::to_value(&conflicting.message).expect("conflicting message payload")
    else {
        panic!("message payload is an object");
    };
    conflicting.event.payload = payload;

    let error = store
        .commit_communication_message(&conflicting)
        .await
        .expect_err("changed immutable content must be rejected");
    assert_eq!(error, DbError::CommunicationSourceConflict);
    assert_eq!(row_counts(&path), (1, 1, 1, 1, 1, 1));
}

#[tokio::test]
async fn source_key_with_changed_spool_file_name_is_rejected_without_cursor_advance() {
    let (_directory, path) = database_path();
    let store = DbActorHandle::open(&path, "0.1.0")
        .await
        .expect("open database");
    let commit = valid_commit(&path);
    store
        .commit_communication_message(&commit)
        .await
        .expect("commit first message");

    let mut conflicting = commit.clone();
    conflicting.attachment_spool[0].file_name = "b".repeat(64);
    assert!(store
        .commit_communication_message(&conflicting)
        .await
        .is_err());
    assert_eq!(row_counts(&path), (1, 1, 1, 1, 1, 1));
    assert_eq!(cursor_sequence(&path), 1);
}

#[tokio::test]
async fn duplicate_rejects_when_persisted_spool_metadata_differs() {
    let (_directory, path) = database_path();
    let store = DbActorHandle::open(&path, "0.1.0")
        .await
        .expect("open database");
    let commit = valid_commit(&path);
    store
        .commit_communication_message(&commit)
        .await
        .expect("commit first message");

    let connection = Connection::open(&path).expect("open metadata corruption connection");
    connection
        .execute(
            "UPDATE attachment_spool SET spool_relative_path = ?1 WHERE attachment_id = 'attachment-1'",
            ["b".repeat(64)],
        )
        .expect("simulate an older noncanonical spool row");
    drop(connection);

    assert!(store.commit_communication_message(&commit).await.is_err());
    assert_eq!(row_counts(&path), (1, 1, 1, 1, 1, 1));
}

#[tokio::test]
async fn same_conversation_sequence_with_different_source_key_is_allowed() {
    let (_directory, path) = database_path();
    let store = DbActorHandle::open(&path, "0.1.0")
        .await
        .expect("open database");
    let first = valid_commit(&path);
    store
        .commit_communication_message(&first)
        .await
        .expect("commit first message");
    let mut conflicting = commit_with(
        &path,
        "event-2",
        "message-2",
        "conversation-1",
        "source-key-2",
        "attachment-2",
    );
    conflicting.source_sequence = first.source_sequence;
    conflicting.attachment_spool[0].attachment_id = "attachment-2".to_owned();

    store
        .commit_communication_message(&conflicting)
        .await
        .expect("different source keys may share a conversation-local sequence");
    assert_eq!(row_counts(&path), (2, 1, 2, 2, 1, 2));
    assert_eq!(cursor_sequence(&path), 1);
}

#[tokio::test]
async fn communication_load_and_ack_never_load_or_acknowledge_system_events() {
    let (_directory, path) = database_path();
    let store = DbActorHandle::open(&path, "0.1.0")
        .await
        .expect("open database");
    let system = system_event();
    store
        .append_event_with_outbox(&system)
        .await
        .expect("persist system event");
    let communication = valid_commit(&path);
    store
        .commit_communication_message(&communication)
        .await
        .expect("persist communication event");

    assert_eq!(
        store
            .load_pending_communication_events(200)
            .await
            .expect("load communication events"),
        vec![communication.event.clone()]
    );
    assert!(store
        .acknowledge_communication_events(std::slice::from_ref(&system.event_id))
        .await
        .is_err());
    assert_eq!(
        store
            .load_pending_system_events(200)
            .await
            .expect("system event remains pending"),
        vec![system.clone()]
    );
    store
        .acknowledge_communication_events(std::slice::from_ref(&communication.event.event_id))
        .await
        .expect("acknowledge communication event");
    assert_eq!(
        store
            .load_pending_system_events(200)
            .await
            .expect("communication acknowledgement leaves system event pending"),
        vec![system]
    );
}

#[tokio::test]
async fn cloud_rejected_communication_event_becomes_terminal_without_deleting_local_history() {
    let (_directory, path) = database_path();
    let store = DbActorHandle::open(&path, "0.1.0")
        .await
        .expect("open database");
    let communication = valid_commit(&path);
    store
        .commit_communication_message(&communication)
        .await
        .expect("persist communication event");

    store
        .dead_letter_rejected_communication_events(std::slice::from_ref(
            &communication.event.event_id,
        ))
        .await
        .expect("dead-letter rejected communication event");

    assert!(store
        .load_pending_communication_events(200)
        .await
        .expect("load pending communication events")
        .is_empty());
    assert_eq!(store.active_outbox_depth().await.expect("active depth"), 0);
    let connection = Connection::open(&path).expect("inspect rejected communication event");
    assert_eq!(
        connection
            .query_row(
                "SELECT state FROM sync_outbox WHERE event_id = ?1",
                [&communication.event.event_id],
                |row| row.get::<_, String>(0),
            )
            .expect("read rejected state"),
        "dead_letter"
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM communication_messages WHERE event_id = ?1",
                [&communication.event.event_id],
                |row| row.get::<_, u64>(0),
            )
            .expect("read retained local message"),
        1
    );
}
