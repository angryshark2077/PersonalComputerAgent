use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use ::time::{format_description::well_known::Rfc3339, OffsetDateTime};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use pca_bridge_client::ScreenCaptureCommandHandle;
use pca_db_local::DbActorHandle;
use pca_domain::{
    CommunicationMessageRecorded, CommunicationMessageRecordedInput, ConversationScope, Direction,
    EventCommit, EventEnvelope, MessageKind, Sensitivity,
};
use rusqlite::Connection;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tokio::{sync::watch, task, time};
use uuid::Uuid;

use crate::cloud_control::{persist_aux_collector_state, AppliedControl};

const POLL_INTERVAL: Duration = Duration::from_secs(30);
const APPLE_EPOCH_UNIX_SECONDS: i128 = 978_307_200;
const LOOKBACK_DAYS: i64 = 7;

struct SourceMessage {
    row_id: i64,
    guid: String,
    chat_guid: String,
    display_name: String,
    sender_id: String,
    sender_display_name: String,
    is_from_me: bool,
    apple_date: i64,
    member_count: u8,
    text: Option<String>,
    attributed_body: Option<Vec<u8>>,
}

pub(crate) async fn run(
    database: Arc<DbActorHandle>,
    bridge: ScreenCaptureCommandHandle,
    workspace_id: String,
    device_id: String,
    mut controls: watch::Receiver<Option<AppliedControl>>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut interval = time::interval(POLL_INTERVAL);
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let control = controls.borrow().clone();
                let revision = control.as_ref().map_or(0, |value| value.configuration_revision);
                let enabled = control.as_ref().is_some_and(|value| value.communication_messages_enabled);
                let result = if enabled {
                    match messages_database_path() {
                        Some(source) => collect_once(&database, &bridge, &source, &workspace_id, &device_id)
                            .await
                            .map_err(|()| "MESSAGES_COLLECTION_FAILED"),
                        None => Err("MESSAGES_DATABASE_UNAVAILABLE"),
                    }
                } else {
                    Ok(false)
                };
                let _ = persist_aux_collector_state(
                    &database,
                    "communication.messages",
                    enabled,
                    revision,
                    result.as_ref().copied().unwrap_or(false),
                    result.err(),
                ).await;
            }
            changed = controls.changed() => {
                if changed.is_err() { return; }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow_and_update() { return; }
            }
        }
    }
}

async fn collect_once(
    database: &DbActorHandle,
    bridge: &ScreenCaptureCommandHandle,
    source: &Path,
    workspace_id: &str,
    device_id: &str,
) -> Result<bool, ()> {
    let cursor = load_cursor().await?;
    let path = source.to_path_buf();
    let messages = task::spawn_blocking(move || load_recent_messages(&path, cursor))
        .await
        .map_err(|_| ())?
        .map_err(|_| ())?;
    let encoded = messages
        .iter()
        .filter_map(|message| {
            (message
                .text
                .as_deref()
                .is_none_or(|value| value.trim().is_empty()))
            .then(|| {
                message
                    .attributed_body
                    .as_ref()
                    .map(|body| STANDARD.encode(body))
            })
            .flatten()
        })
        .collect::<Vec<_>>();
    let mut decoded = Vec::with_capacity(encoded.len());
    for chunk in encoded.chunks(100) {
        decoded.extend(
            bridge
                .decode_message_bodies(chunk.to_vec())
                .await
                .map_err(|_| ())?,
        );
    }
    let mut decoded = decoded.into_iter();
    let mut event_observed = false;
    for source_message in messages {
        let row_id = source_message.row_id;
        let text = source_message
            .text
            .as_deref()
            .and_then(normalize_text)
            .map(str::to_owned)
            .or_else(|| {
                source_message
                    .attributed_body
                    .as_ref()
                    .and_then(|_| decoded.next().flatten())
            });
        if let Some(text) = text {
            let events = message_events(&source_message, text, workspace_id, device_id)?;
            let commit = EventCommit::try_new(events, None).map_err(|_| ())?;
            database.commit_events(&commit).await.map_err(|_| ())?;
            event_observed = true;
        }
        persist_cursor(row_id).await?;
    }
    Ok(event_observed)
}

fn load_recent_messages(
    path: &PathBuf,
    after_row_id: Option<i64>,
) -> rusqlite::Result<Vec<SourceMessage>> {
    let connection = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    let cutoff_unix = OffsetDateTime::now_utc().unix_timestamp() - LOOKBACK_DAYS * 86_400;
    let cutoff_apple_ns = (i128::from(cutoff_unix) - APPLE_EPOCH_UNIX_SECONDS) * 1_000_000_000;
    let cutoff_apple_ns = i64::try_from(cutoff_apple_ns).unwrap_or(i64::MIN);
    let mut statement = connection.prepare(
        "SELECT m.ROWID, m.guid, c.guid, COALESCE(NULLIF(trim(c.display_name), ''), NULLIF(trim(c.chat_identifier), ''), c.guid),
                COALESCE(h.id, 'me'), COALESCE(h.id, 'Me'), m.is_from_me, m.date,
                MAX(1, (SELECT COUNT(*) FROM chat_handle_join chj WHERE chj.chat_id = c.ROWID)),
                m.text, m.attributedBody
         FROM message m
         INNER JOIN chat_message_join cmj ON cmj.message_id = m.ROWID
         INNER JOIN chat c ON c.ROWID = cmj.chat_id
         LEFT JOIN handle h ON h.ROWID = m.handle_id
         WHERE ((?1 IS NULL AND m.date >= ?2) OR (?1 IS NOT NULL AND m.ROWID > ?1)) AND m.is_empty = 0
         ORDER BY m.ROWID ASC"
    )?;
    let rows = statement
        .query_map(rusqlite::params![after_row_id, cutoff_apple_ns], |row| {
            let members =
                u8::try_from(row.get::<_, i64>(8)?.clamp(1, i64::from(u8::MAX))).unwrap_or(u8::MAX);
            let is_from_me = row.get::<_, i64>(6)? != 0;
            let remote_sender = row.get::<_, String>(4)?;
            Ok(SourceMessage {
                row_id: row.get(0)?,
                guid: row.get(1)?,
                chat_guid: row.get(2)?,
                display_name: row.get(3)?,
                sender_id: if is_from_me {
                    "me".to_owned()
                } else {
                    remote_sender.clone()
                },
                sender_display_name: if is_from_me {
                    "Me".to_owned()
                } else {
                    remote_sender
                },
                is_from_me,
                apple_date: row.get(7)?,
                member_count: members,
                text: row.get(9)?,
                attributed_body: row.get(10)?,
            })
        })?
        .collect();
    rows
}

fn message_events(
    source: &SourceMessage,
    text: String,
    workspace_id: &str,
    device_id: &str,
) -> Result<Vec<EventEnvelope>, ()> {
    let occurred_at = apple_date_to_rfc3339(source.apple_date)?;
    let created_at = occurred_at.clone();
    let conversation_id = format!("messages:{}", source.chat_guid);
    let message_id = format!("messages:{}", source.guid);
    let source_key = message_id.clone();
    let scope = if source.member_count > 1 {
        ConversationScope::Group {
            member_count: source.member_count,
        }
    } else {
        ConversationScope::Direct
    };
    let message = CommunicationMessageRecorded::try_new(CommunicationMessageRecordedInput {
        message_id: message_id.clone(),
        conversation_id: conversation_id.clone(),
        sender_id: source.sender_id.clone(),
        sender_display_name: source.sender_display_name.clone(),
        source_key: source_key.clone(),
        occurred_at: occurred_at.clone(),
        direction: if source.is_from_me {
            Direction::Outgoing
        } else {
            Direction::Incoming
        },
        kind: MessageKind::Text,
        conversation: scope.clone(),
        text: Some(text),
        attachments: Vec::new(),
    })
    .map_err(|_| ())?;
    let conversation_payload = serde_json::json!({
        "conversation_id": conversation_id,
        "display_name": source.display_name,
        "observed_at": occurred_at,
        "conversation": scope,
    });
    let sender_payload = serde_json::json!({
        "message_id": message_id,
        "source_key": source_key,
        "sender_id": source.sender_id,
        "sender_display_name": source.sender_display_name,
        "observed_at": occurred_at,
    });
    let conversation_key = format!("{}:{}", source.chat_guid, source.guid);
    let mut recorded = event(
        workspace_id,
        device_id,
        "communication.message_recorded",
        &source.guid,
        occurred_at.clone(),
        created_at.clone(),
        object(&serde_json::to_value(message).map_err(|_| ())?)?,
    );
    recorded.idempotency_key = Some(source_key);
    Ok(vec![
        event(
            workspace_id,
            device_id,
            "communication.conversation_observed",
            &conversation_key,
            occurred_at.clone(),
            created_at.clone(),
            object(&conversation_payload)?,
        ),
        event(
            workspace_id,
            device_id,
            "communication.message_sender_observed",
            &format!("sender:{}", source.guid),
            occurred_at.clone(),
            created_at.clone(),
            object(&sender_payload)?,
        ),
        recorded,
    ])
}

fn event(
    workspace_id: &str,
    device_id: &str,
    event_type: &str,
    stable_key: &str,
    occurred_at: String,
    created_at: String,
    payload: Map<String, Value>,
) -> EventEnvelope {
    let event_id = stable_uuid(workspace_id, device_id, event_type, stable_key);
    EventEnvelope {
        event_id,
        workspace_id: workspace_id.to_owned(),
        device_id: device_id.to_owned(),
        event_type: event_type.to_owned(),
        source: "communication.messages".to_owned(),
        schema_version: 1,
        occurred_at,
        created_at,
        sensitivity: Sensitivity::High,
        payload,
        attachment_refs: Vec::new(),
        idempotency_key: Some(format!("messages:{event_type}:{stable_key}")),
    }
}

fn stable_uuid(workspace_id: &str, device_id: &str, event_type: &str, key: &str) -> String {
    let digest =
        Sha256::digest(format!("{workspace_id}\0{device_id}\0{event_type}\0{key}").as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes).hyphenated().to_string()
}

fn apple_date_to_rfc3339(value: i64) -> Result<String, ()> {
    let apple_nanos = if value.abs() < 10_000_000_000 {
        i128::from(value) * 1_000_000_000
    } else {
        i128::from(value)
    };
    let unix_nanos = apple_nanos + APPLE_EPOCH_UNIX_SECONDS * 1_000_000_000;
    let unix_millis = unix_nanos.div_euclid(1_000_000);
    OffsetDateTime::from_unix_timestamp_nanos(unix_millis * 1_000_000)
        .map_err(|_| ())?
        .format(&Rfc3339)
        .map_err(|_| ())
}

fn object(value: &Value) -> Result<Map<String, Value>, ()> {
    value.as_object().cloned().ok_or(())
}
fn normalize_text(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}
fn messages_database_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Library/Messages/chat.db"))
}

fn cursor_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Library/Application Support/PersonalComputerAgent/messages-cursor"))
}

async fn load_cursor() -> Result<Option<i64>, ()> {
    let Some(path) = cursor_path() else {
        return Err(());
    };
    match tokio::fs::read_to_string(path).await {
        Ok(value) => value.trim().parse::<i64>().map(Some).map_err(|_| ()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(()),
    }
}

async fn persist_cursor(row_id: i64) -> Result<(), ()> {
    let path = cursor_path().ok_or(())?;
    let parent = path.parent().ok_or(())?;
    tokio::fs::create_dir_all(parent).await.map_err(|_| ())?;
    let temporary = path.with_extension("tmp");
    tokio::fs::write(&temporary, row_id.to_string())
        .await
        .map_err(|_| ())?;
    tokio::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))
        .await
        .map_err(|_| ())?;
    tokio::fs::rename(temporary, path).await.map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::{apple_date_to_rfc3339, message_events, stable_uuid, SourceMessage};

    #[test]
    fn outgoing_messages_use_local_sender_and_stable_private_events() {
        let message = SourceMessage {
            row_id: 42,
            guid: "message-guid".to_owned(),
            chat_guid: "chat-guid".to_owned(),
            display_name: "Family".to_owned(),
            sender_id: "me".to_owned(),
            sender_display_name: "Me".to_owned(),
            is_from_me: true,
            apple_date: 0,
            member_count: 20,
            text: Some("hello".to_owned()),
            attributed_body: None,
        };
        let first = message_events(&message, "hello".to_owned(), "workspace", "device")
            .expect("valid message events");
        let second = message_events(&message, "hello".to_owned(), "workspace", "device")
            .expect("stable message events");
        assert_eq!(first.len(), 3);
        assert_eq!(first[2].event_id, second[2].event_id);
        assert_eq!(first[2].source, "communication.messages");
        assert_eq!(first[2].occurred_at, "2001-01-01T00:00:00Z");
        assert_eq!(first[2].payload["sender_id"], "me");
        assert_eq!(
            first[2].idempotency_key.as_deref(),
            first[2].payload["source_key"].as_str()
        );
        assert_eq!(first[2].payload["conversation"]["member_count"], 20);
        assert_eq!(
            first[2].event_id,
            stable_uuid(
                "workspace",
                "device",
                "communication.message_recorded",
                "message-guid"
            )
        );

        let mut later = message;
        later.row_id = 43;
        later.guid = "later-message-guid".to_owned();
        later.apple_date = 1;
        let later_events = message_events(&later, "later".to_owned(), "workspace", "device")
            .expect("later message events");
        assert_ne!(first[0].event_id, later_events[0].event_id);
    }

    #[test]
    fn apple_message_timestamps_are_normalized_to_milliseconds() {
        assert_eq!(
            apple_date_to_rfc3339(800_000_000_123_999_999).as_deref(),
            Ok("2026-05-09T06:13:20.123Z")
        );
    }
}
