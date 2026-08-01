use std::{
    os::unix::fs::{symlink, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use pca_db_local::{
    CommunicationAttachmentSpoolReference, CommunicationMessageCommit, DbActorHandle,
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
async fn same_conversation_sequence_with_different_source_key_is_rejected() {
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

    assert!(store
        .commit_communication_message(&conflicting)
        .await
        .is_err());
    assert_eq!(row_counts(&path), (1, 1, 1, 1, 1, 1));
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
