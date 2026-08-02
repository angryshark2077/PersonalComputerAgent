use std::{
    collections::BTreeMap,
    env, fs,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use md5::{Digest as _, Md5};
use pca_domain::DomainError;
use pca_keychain::{load_wechat_key_material, MacOSKeychainStore, WechatKeyMaterial};
use pca_provider_contracts::{CommunicationProvider, CommunicationProviderFactory};
use rusqlite::{types::Value, Connection, OptionalExtension};
use sha2::Sha256;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::{
    source::{
        GroupMembershipEvidence, LocalAccountProof, SourceCapabilities, SourceConversation,
        SourceCursor, SourceDirection, SourceFinality, SourceMessageKind, SourceMessageRecord,
        SourcePayload, SourceProbeFuture, SourceReadFuture, SourceRecord, WechatSource,
    },
    sqlcipher_source::{with_recovered_database, SqlcipherProbeFailure},
    WechatProvider,
};

const SOURCE_ROOT: &str = "Library/Containers/com.tencent.xinWeChat/Data/Documents/xwechat_files";
const SESSION_DATABASE: &str = "db_storage/session/session.db";
const CONTACT_DATABASE: &str = "db_storage/contact/contact.db";
const MESSAGE_DIRECTORY: &str = "db_storage/message";
const MAX_BATCH: usize = 200;
const MAX_PER_CONVERSATION: usize = 20;
const INITIAL_HISTORY_SECONDS: u64 = 60 * 24 * 60 * 60;
const DATABASE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Default)]
pub struct MacOSWechatProviderFactory;

impl CommunicationProviderFactory for MacOSWechatProviderFactory {
    fn create(&self) -> Result<Box<dyn CommunicationProvider>, DomainError> {
        Ok(Box::new(
            WechatProvider::new(MacOSWechatSource::discover()?),
        ))
    }
}

struct SourcePaths {
    session_database: PathBuf,
    contact_database: PathBuf,
    message_databases: Vec<PathBuf>,
    local_username: String,
    account_id: String,
}

struct MacOSWechatSource {
    paths: Arc<SourcePaths>,
    cursors: Arc<Mutex<BTreeMap<String, i64>>>,
    verified: Arc<Mutex<bool>>,
}

impl MacOSWechatSource {
    fn discover() -> Result<Self, DomainError> {
        let home = env::var_os("HOME").ok_or_else(source_unavailable)?;
        let root = PathBuf::from(home).join(SOURCE_ROOT);
        let mut accounts = fs::read_dir(root)
            .map_err(|_| source_unavailable())?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .filter(|path| {
                path.join(SESSION_DATABASE).is_file()
                    && path.join(CONTACT_DATABASE).is_file()
                    && path.join(MESSAGE_DIRECTORY).is_dir()
            });
        let account_root = accounts.next().ok_or_else(source_unavailable)?;
        if accounts.next().is_some() {
            return Err(DomainError::new(
                "WECHAT_MULTIPLE_ACCOUNTS",
                "multiple local WeChat accounts require explicit selection",
                false,
            ));
        }
        let local_username = account_root
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(clean_account_directory_name)
            .ok_or_else(source_unavailable)?;
        let mut message_databases = fs::read_dir(account_root.join(MESSAGE_DIRECTORY))
            .map_err(|_| source_unavailable())?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| is_message_database(path))
            .collect::<Vec<_>>();
        message_databases.sort();
        if message_databases.is_empty() {
            return Err(source_unavailable());
        }
        let account_id = account_id_for_root(&account_root);
        Ok(Self {
            paths: Arc::new(SourcePaths {
                session_database: account_root.join(SESSION_DATABASE),
                contact_database: account_root.join(CONTACT_DATABASE),
                message_databases,
                local_username,
                account_id,
            }),
            cursors: Arc::new(Mutex::new(BTreeMap::new())),
            verified: Arc::new(Mutex::new(false)),
        })
    }
}

impl WechatSource for MacOSWechatSource {
    fn probe(&self) -> SourceProbeFuture<'_> {
        let paths = Arc::clone(&self.paths);
        let verified = Arc::clone(&self.verified);
        Box::pin(async move {
            let capabilities = tokio::task::spawn_blocking(move || probe_source(&paths))
                .await
                .map_err(|_| capability_unavailable())??;
            *verified.lock().map_err(|_| capability_unavailable())? = true;
            Ok(capabilities)
        })
    }

    fn read_after(&self, _: &SourceCursor) -> SourceReadFuture<'_> {
        let paths = Arc::clone(&self.paths);
        let cursors = Arc::clone(&self.cursors);
        let verified = Arc::clone(&self.verified);
        Box::pin(async move {
            if !verified.lock().is_ok_and(|state| *state) {
                return Err(waiting_source());
            }
            tokio::task::spawn_blocking(move || read_text_batch(&paths, &cursors))
                .await
                .map_err(|_| capability_unavailable())?
        })
    }
}

fn probe_source(paths: &SourcePaths) -> Result<SourceCapabilities, DomainError> {
    let material = load_material(paths)?;
    with_database(&paths.session_database, &material, |connection| {
        require_columns(connection, "SessionTable", &["username", "last_timestamp"])
    })?;
    with_database(&paths.contact_database, &material, |connection| {
        require_columns(connection, "name2id", &["username"])?;
        require_columns(connection, "chatroom_member", &["room_id", "member_id"])?;
        require_columns(
            connection,
            "contact",
            &["username", "remark", "nick_name", "alias"],
        )
    })?;
    let mut schema_version = 0_u32;
    for database in &paths.message_databases {
        schema_version = with_database(database, &material, |connection| {
            require_columns(connection, "Name2Id", &["user_name"])?;
            let table_count: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name GLOB 'Msg_[0-9a-fA-F]*'",
                    [],
                    |row| row.get(0),
                )
                .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
            if table_count == 0 || local_sender_rowid(connection, &paths.local_username)?.is_none()
            {
                return Err(SqlcipherProbeFailure::AccountUnverified);
            }
            let version = connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
            Ok(version)
        })?;
    }
    Ok(SourceCapabilities {
        source_version: "wechat-4.x-structural".to_owned(),
        schema_version,
    })
}

fn read_text_batch(
    paths: &SourcePaths,
    cursors: &Mutex<BTreeMap<String, i64>>,
) -> Result<Vec<SourceRecord>, DomainError> {
    let material = load_material(paths)?;
    let sessions = with_database(&paths.session_database, &material, read_sessions)?;
    let conversation_metadata = with_database(&paths.contact_database, &material, |connection| {
        read_conversation_metadata(connection, &sessions)
    })?;
    let cutoff = retention_cutoff();
    let mut records = Vec::new();
    let mut cursor_guard = cursors.lock().map_err(|_| capability_unavailable())?;

    for database in &paths.message_databases {
        if records.len() >= MAX_BATCH {
            break;
        }
        let database_name = database
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(source_unavailable)?
            .to_owned();
        let remaining = MAX_BATCH - records.len();
        let context = TextReadContext {
            database_name: &database_name,
            local_username: &paths.local_username,
            account_id: material.account_id(),
            conversation_metadata: &conversation_metadata,
            cursors: &cursor_guard,
            cutoff,
        };
        let batch = with_database(database, &material, |connection| {
            read_database_text(connection, &context, &sessions, remaining)
        })?;
        for (cursor_key, sequence, record) in batch {
            cursor_guard
                .entry(cursor_key)
                .and_modify(|current| *current = (*current).max(sequence))
                .or_insert(sequence);
            records.push(SourceRecord::Message(Box::new(record)));
        }
    }
    Ok(records)
}

struct TextReadContext<'a> {
    database_name: &'a str,
    local_username: &'a str,
    account_id: &'a str,
    conversation_metadata: &'a BTreeMap<String, ConversationMetadata>,
    cursors: &'a BTreeMap<String, i64>,
    cutoff: i64,
}

#[allow(
    clippy::too_many_lines,
    reason = "the bounded row-to-domain mapping is kept together for fail-closed review"
)]
fn read_database_text(
    connection: &Connection,
    context: &TextReadContext<'_>,
    sessions: &[Session],
    limit: usize,
) -> Result<Vec<(String, i64, SourceMessageRecord)>, SqlcipherProbeFailure> {
    let my_rowid = local_sender_rowid(connection, context.local_username)?
        .ok_or(SqlcipherProbeFailure::AccountUnverified)?;
    let mut sender_statement = connection
        .prepare("SELECT user_name FROM Name2Id WHERE rowid = ?1 LIMIT 1")
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
    let mut records = Vec::new();
    for session in sessions {
        if records.len() >= limit {
            break;
        }
        let metadata = context
            .conversation_metadata
            .get(&session.username)
            .cloned()
            .unwrap_or_else(|| ConversationMetadata {
                display_name: session.username.clone(),
                member_count: None,
                participant_names: BTreeMap::new(),
            });
        let conversation = if session.username.ends_with("@chatroom") {
            let Some(member_count) = metadata.member_count else {
                continue;
            };
            if !(1..=15).contains(&member_count) {
                continue;
            }
            SourceConversation::Group {
                membership: GroupMembershipEvidence::Verified(member_count),
            }
        } else if is_direct_conversation(&session.username) {
            SourceConversation::Direct
        } else {
            continue;
        };
        let table_name = message_table_name(&session.username);
        if !table_exists(connection, &table_name)? {
            continue;
        }
        let cursor_key = format!("{}:{}:text", context.database_name, session.username);
        let after = context.cursors.get(&cursor_key).copied().unwrap_or(0);
        let per_conversation = MAX_PER_CONVERSATION.min(limit - records.len());
        let sql = format!(
            "SELECT local_id, server_id, create_time, real_sender_id, status, message_content, compress_content \
             FROM \"{table_name}\" \
             WHERE local_type = 1 AND local_id > ?1 AND create_time >= ?2 \
             ORDER BY local_id ASC LIMIT ?3"
        );
        let mut statement = connection
            .prepare(&sql)
            .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
        let rows = statement
            .query_map(
                (
                    after,
                    context.cutoff,
                    i64::try_from(per_conversation).unwrap_or(20),
                ),
                |row| {
                    Ok(MessageRow {
                        local_id: row.get(0)?,
                        server_id: row.get(1)?,
                        create_time: row.get(2)?,
                        real_sender_id: row.get(3)?,
                        status: row.get(4)?,
                        message_content: row.get(5)?,
                        compress_content: row.get(6)?,
                    })
                },
            )
            .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
        for row in rows {
            let row = row.map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
            if row.local_id <= 0 || row.create_time <= 0 {
                continue;
            }
            let direction = if row.real_sender_id == my_rowid {
                if row.server_id <= 0 || row.status < 0 {
                    continue;
                }
                SourceDirection::Outgoing
            } else {
                SourceDirection::Incoming
            };
            let (sender_id, sender_display_name) = match direction {
                SourceDirection::Outgoing => (context.local_username.to_owned(), "You".to_owned()),
                SourceDirection::Incoming if session.username.ends_with("@chatroom") => {
                    let sender_id = sender_statement
                        .query_row([row.real_sender_id], |sender_row| {
                            sender_row.get::<_, String>(0)
                        })
                        .optional()
                        .ok()
                        .flatten()
                        .filter(|value| valid_identity(value));
                    resolve_group_sender(sender_id, row.real_sender_id, &metadata.participant_names)
                }
                SourceDirection::Incoming => {
                    (session.username.clone(), metadata.display_name.clone())
                }
                SourceDirection::Unknown => continue,
            };
            let Some(body) = decode_message_content(&row.compress_content, &row.message_content)
            else {
                continue;
            };
            let occurred_at = OffsetDateTime::from_unix_timestamp(row.create_time)
                .ok()
                .and_then(|time| time.format(&Rfc3339).ok())
                .ok_or(SqlcipherProbeFailure::UnsupportedSchema)?;
            let message_id = if row.server_id > 0 {
                row.server_id.to_string()
            } else {
                format!("local-{}", row.local_id)
            };
            let source_key = format!(
                "wechat:{}:{table_name}:{}",
                context.database_name, row.local_id
            );
            records.push((
                cursor_key.clone(),
                row.local_id,
                SourceMessageRecord {
                    account_id: context.account_id.to_owned(),
                    source_sequence: u64::try_from(row.local_id)
                        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?,
                    message_id,
                    conversation_id: session.username.clone(),
                    conversation_display_name: metadata.display_name.clone(),
                    sender_id,
                    sender_display_name,
                    source_key,
                    occurred_at,
                    local_account: LocalAccountProof::Verified,
                    direction,
                    kind: SourceMessageKind::Text,
                    conversation: conversation.clone(),
                    finality: match direction {
                        SourceDirection::Incoming => SourceFinality::IncomingPersisted,
                        SourceDirection::Outgoing => SourceFinality::OutgoingSent,
                        SourceDirection::Unknown => SourceFinality::Unknown,
                    },
                    payload: SourcePayload::Text { body },
                },
            ));
        }
    }
    Ok(records)
}

struct Session {
    username: String,
}

#[derive(Clone)]
struct ConversationMetadata {
    display_name: String,
    member_count: Option<u8>,
    participant_names: BTreeMap<String, String>,
}

struct MessageRow {
    local_id: i64,
    server_id: i64,
    create_time: i64,
    real_sender_id: i64,
    status: i64,
    message_content: Value,
    compress_content: Value,
}

fn read_sessions(connection: &Connection) -> Result<Vec<Session>, SqlcipherProbeFailure> {
    let mut statement = connection
        .prepare(
            "SELECT username FROM SessionTable \
             WHERE username <> '' ORDER BY last_timestamp DESC LIMIT 4096",
        )
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
    let sessions = statement
        .query_map([], |row| {
            Ok(Session {
                username: row.get(0)?,
            })
        })
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?
        .filter_map(|row| match row {
            Ok(session) if !session.username.chars().any(char::is_control) => Some(Ok(session)),
            Ok(_) => None,
            Err(_) => Some(Err(SqlcipherProbeFailure::UnsupportedSchema)),
        })
        .collect();
    sessions
}

fn read_conversation_metadata(
    connection: &Connection,
    sessions: &[Session],
) -> Result<BTreeMap<String, ConversationMetadata>, SqlcipherProbeFailure> {
    let mut metadata = BTreeMap::new();
    let mut contact_statement = connection
        .prepare("SELECT remark, nick_name, alias FROM contact WHERE username = ?1 LIMIT 1")
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
    let mut stranger_statement = connection
        .prepare("SELECT remark, nick_name, alias FROM stranger WHERE username = ?1 LIMIT 1")
        .ok();
    let mut count_statement = connection
        .prepare(
            "SELECT COUNT(*) FROM chatroom_member \
             WHERE room_id = (SELECT rowid FROM name2id WHERE username = ?1)",
        )
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
    let mut member_statement = connection
        .prepare(
            "SELECT n.username FROM chatroom_member m \
             JOIN name2id n ON m.member_id = n.rowid \
             WHERE m.room_id = (SELECT rowid FROM name2id WHERE username = ?1)",
        )
        .ok();
    let mut room_buffer_statement = connection
        .prepare("SELECT ext_buffer FROM chat_room WHERE username = ?1 LIMIT 1")
        .ok();
    for session in sessions {
        let display_name = read_display_name(&mut contact_statement, &session.username)?
            .or_else(|| {
                stranger_statement.as_mut().and_then(|statement| {
                    read_display_name(statement, &session.username)
                        .ok()
                        .flatten()
                })
            })
            .unwrap_or_else(|| session.username.clone());
        let (member_count, participant_names) = if session.username.ends_with("@chatroom") {
            let count: i64 = count_statement
                .query_row([&session.username], |row| row.get(0))
                .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
            let members = member_statement
                .as_mut()
                .and_then(|statement| {
                    statement
                        .query_map([&session.username], |row| row.get::<_, String>(0))
                        .ok()
                        .map(|rows| {
                            rows.filter_map(Result::ok)
                                .filter(|member| valid_identity(member))
                                .collect::<Vec<_>>()
                        })
                })
                .unwrap_or_default();
            let room_buffer = room_buffer_statement
                .as_mut()
                .and_then(|statement| {
                    statement
                        .query_row([&session.username], |row| row.get::<_, Value>(0))
                        .optional()
                        .ok()
                        .flatten()
                })
                .and_then(value_bytes);
            let group_nicknames = room_buffer
                .as_deref()
                .map(|buffer| parse_group_nicknames(buffer, &members))
                .unwrap_or_default();
            let mut participant_names = BTreeMap::new();
            for member in members {
                let name = group_nicknames
                    .get(&member)
                    .cloned()
                    .or_else(|| {
                        read_display_name(&mut contact_statement, &member)
                            .ok()
                            .flatten()
                    })
                    .or_else(|| {
                        stranger_statement.as_mut().and_then(|statement| {
                            read_display_name(statement, &member).ok().flatten()
                        })
                    })
                    .unwrap_or_else(|| member.clone());
                participant_names.insert(member, name);
            }
            (u8::try_from(count).ok(), participant_names)
        } else {
            (None, BTreeMap::new())
        };
        metadata.insert(
            session.username.clone(),
            ConversationMetadata {
                display_name,
                member_count,
                participant_names,
            },
        );
    }
    Ok(metadata)
}

fn read_display_name(
    statement: &mut rusqlite::Statement<'_>,
    username: &str,
) -> Result<Option<String>, SqlcipherProbeFailure> {
    let names = statement
        .query_row([username], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .optional()
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
    Ok(names
        .into_iter()
        .flat_map(|(remark, nickname, alias)| [remark, nickname, alias])
        .flatten()
        .find_map(valid_display_name))
}

fn value_bytes(value: Value) -> Option<Vec<u8>> {
    match value {
        Value::Blob(value) => Some(value),
        Value::Text(value) => Some(value.into_bytes()),
        _ => None,
    }
}

fn valid_identity(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && !value.chars().any(char::is_control)
        && !value.ends_with("@chatroom")
}

fn resolve_group_sender(
    sender_id: Option<String>,
    sender_rowid: i64,
    participant_names: &BTreeMap<String, String>,
) -> (String, String) {
    let Some(sender_id) = sender_id else {
        return (
            format!("wechat-rowid:{sender_rowid}"),
            "Unknown member".to_owned(),
        );
    };
    let display_name = participant_names
        .get(&sender_id)
        .cloned()
        .unwrap_or_else(|| sender_id.clone());
    (sender_id, display_name)
}

fn parse_group_nicknames(buffer: &[u8], candidates: &[String]) -> BTreeMap<String, String> {
    let candidate_set = candidates
        .iter()
        .map(|candidate| candidate.to_ascii_lowercase())
        .collect::<std::collections::BTreeSet<_>>();
    let mut names = BTreeMap::new();
    let mut index = 0;
    while index + 2 < buffer.len() {
        if buffer[index] != 0x0a {
            index += 1;
            continue;
        }
        let Some((id_length, id_start)) = read_varint(buffer, index + 1) else {
            index += 1;
            continue;
        };
        let Some(id_end) = id_start.checked_add(id_length) else {
            index += 1;
            continue;
        };
        if id_length == 0 || id_length > 96 || id_end >= buffer.len() {
            index += 1;
            continue;
        }
        let Ok(member_id) = std::str::from_utf8(&buffer[id_start..id_end]) else {
            index += 1;
            continue;
        };
        let member_id = member_id.trim();
        if !valid_identity(member_id)
            || !candidate_set.contains(&member_id.to_ascii_lowercase())
            || buffer[id_end] != 0x12
        {
            index = id_end;
            continue;
        }
        let Some((name_length, name_start)) = read_varint(buffer, id_end + 1) else {
            index = id_end;
            continue;
        };
        let Some(name_end) = name_start.checked_add(name_length) else {
            index = id_end;
            continue;
        };
        if name_length == 0 || name_length > 128 || name_end > buffer.len() {
            index = id_end;
            continue;
        }
        if let Ok(name) = std::str::from_utf8(&buffer[name_start..name_end]) {
            let cleaned = name
                .chars()
                .filter(|character| !character.is_control())
                .collect::<String>();
            if let Some(name) = valid_display_name(cleaned) {
                if !name.eq_ignore_ascii_case(member_id)
                    && !name.ends_with("@chatroom")
                    && !name.starts_with("wxid_")
                {
                    names.entry(member_id.to_owned()).or_insert(name);
                }
            }
        }
        index = name_end;
    }
    names
}

fn read_varint(buffer: &[u8], start: usize) -> Option<(usize, usize)> {
    let mut value = 0_usize;
    let mut shift = 0_u32;
    let mut position = start;
    while position < buffer.len() && shift <= 28 {
        let byte = buffer[position];
        value = value.checked_add(usize::from(byte & 0x7f).checked_shl(shift)?)?;
        position += 1;
        if byte & 0x80 == 0 {
            return Some((value, position));
        }
        shift += 7;
    }
    None
}

fn valid_display_name(value: String) -> Option<String> {
    let value = value.trim().to_owned();
    (!value.is_empty() && value.len() <= 1024 && !value.chars().any(char::is_control))
        .then_some(value)
}

fn load_material(paths: &SourcePaths) -> Result<WechatKeyMaterial, DomainError> {
    let material = load_wechat_key_material(&MacOSKeychainStore)
        .map_err(|_| capability_unavailable())?
        .ok_or_else(waiting_source)?;
    if material.account_id() != paths.account_id {
        return Err(DomainError::new(
            "WECHAT_ACCOUNT_UNVERIFIED",
            "stored WeChat key belongs to a different local account",
            false,
        ));
    }
    Ok(material)
}

fn with_database<T>(
    path: &Path,
    material: &WechatKeyMaterial,
    operation: impl FnOnce(&Connection) -> Result<T, SqlcipherProbeFailure>,
) -> Result<T, DomainError> {
    with_recovered_database(path, material, DATABASE_TIMEOUT, operation).map_err(map_failure)
}

fn require_columns(
    connection: &Connection,
    table: &str,
    required: &[&str],
) -> Result<(), SqlcipherProbeFailure> {
    let mut statement = connection
        .prepare("SELECT name FROM pragma_table_info(?1)")
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
    let columns = statement
        .query_map([table], |row| row.get::<_, String>(0))
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
    if required
        .iter()
        .all(|required| columns.iter().any(|column| column == required))
    {
        Ok(())
    } else {
        Err(SqlcipherProbeFailure::UnsupportedSchema)
    }
}

fn local_sender_rowid(
    connection: &Connection,
    local_username: &str,
) -> Result<Option<i64>, SqlcipherProbeFailure> {
    connection
        .query_row(
            "SELECT rowid FROM Name2Id WHERE user_name = ?1 LIMIT 1",
            [local_username],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| SqlcipherProbeFailure::AccountUnverified)
}

fn table_exists(connection: &Connection, table: &str) -> Result<bool, SqlcipherProbeFailure> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [table],
            |row| row.get(0),
        )
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)
}

fn decode_message_content(compressed: &Value, plain: &Value) -> Option<String> {
    decode_value(compressed).or_else(|| decode_value(plain))
}

fn decode_value(value: &Value) -> Option<String> {
    let bytes = match value {
        Value::Text(text) => return nonempty_text(text.clone()),
        Value::Blob(bytes) => bytes.as_slice(),
        _ => return None,
    };
    let decoded = if bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        zstd::stream::decode_all(bytes).ok()?
    } else {
        bytes.to_vec()
    };
    nonempty_text(String::from_utf8(decoded).ok()?)
}

fn nonempty_text(text: impl AsRef<str>) -> Option<String> {
    let text = text.as_ref().trim_matches('\0').trim().to_owned();
    (!text.is_empty() && text.len() <= 4 * 1024 * 1024).then_some(text)
}

fn message_table_name(username: &str) -> String {
    let digest = Md5::digest(username.as_bytes());
    format!("Msg_{digest:x}")
}

fn clean_account_directory_name(name: &str) -> Option<String> {
    let name = name.trim();
    if let Some(rest) = name.strip_prefix("wxid_") {
        let core = rest.split('_').next()?;
        if !core.is_empty()
            && core
                .chars()
                .all(|character| character.is_ascii_alphanumeric())
        {
            return Some(format!("wxid_{core}"));
        }
    }
    None
}

fn is_message_database(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("message_"))
        .and_then(|name| name.strip_suffix(".db"))
        .is_some_and(|index| {
            !index.is_empty() && index.chars().all(|character| character.is_ascii_digit())
        })
}

fn is_direct_conversation(username: &str) -> bool {
    const SYSTEM: [&str; 11] = [
        "filehelper",
        "fmessage",
        "floatbottle",
        "medianote",
        "newsapp",
        "qmessage",
        "qqmail",
        "weixin",
        "brandsessionholder",
        "brandservicesessionholder",
        "notifymessage",
    ];
    let lower = username.to_ascii_lowercase();
    !lower.is_empty()
        && !SYSTEM.contains(&lower.as_str())
        && !lower.starts_with("gh_")
        && !lower.starts_with("fake_")
        && !lower.contains("@kefu.openim")
        && !lower.contains("service_")
}

fn retention_cutoff() -> i64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    retention_cutoff_from(now)
}

fn retention_cutoff_from(now: u64) -> i64 {
    i64::try_from(now.saturating_sub(INITIAL_HISTORY_SECONDS)).unwrap_or(0)
}

fn account_id_for_root(root: &Path) -> String {
    let fingerprint = Sha256::digest(root.as_os_str().as_bytes());
    format!("wechat-db-v1:{fingerprint:x}")
}

fn map_failure(failure: SqlcipherProbeFailure) -> DomainError {
    match failure {
        SqlcipherProbeFailure::CapabilityUnavailable => capability_unavailable(),
        SqlcipherProbeFailure::DatabaseUnavailable => source_unavailable(),
        SqlcipherProbeFailure::KeyRejected => DomainError::new(
            "WECHAT_KEY_REJECTED",
            "stored WeChat key material was rejected",
            false,
        ),
        SqlcipherProbeFailure::TimedOut => DomainError::new(
            "WECHAT_PROBE_TIMEOUT",
            "WeChat source verification timed out",
            true,
        ),
        SqlcipherProbeFailure::UnsupportedSourceVersion => DomainError::new(
            "WECHAT_UNSUPPORTED_SOURCE_VERSION",
            "WeChat source version is unsupported",
            false,
        ),
        SqlcipherProbeFailure::UnsupportedSchema => DomainError::new(
            "WECHAT_UNSUPPORTED_SCHEMA",
            "WeChat source schema is unsupported",
            false,
        ),
        SqlcipherProbeFailure::AccountUnverified => DomainError::new(
            "WECHAT_ACCOUNT_UNVERIFIED",
            "WeChat source account could not be verified",
            false,
        ),
    }
}

fn waiting_source() -> DomainError {
    DomainError::new(
        "WECHAT_WAITING_SOURCE",
        "WeChat source key material is not available",
        true,
    )
}

fn source_unavailable() -> DomainError {
    DomainError::new(
        "WECHAT_DATABASE_UNAVAILABLE",
        "WeChat source database is unavailable",
        true,
    )
}

fn capability_unavailable() -> DomainError {
    DomainError::new(
        "WECHAT_CAPABILITY_UNAVAILABLE",
        "WeChat source verification capability is unavailable",
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::{
        clean_account_directory_name, decode_value, is_direct_conversation, is_message_database,
        message_table_name, parse_group_nicknames, read_conversation_metadata,
        resolve_group_sender, retention_cutoff_from, Session,
    };
    use rusqlite::{types::Value, Connection};
    use std::path::Path;

    #[test]
    fn production_naming_rules_match_wechat_four_schema() {
        assert_eq!(
            message_table_name("wxid_example"),
            "Msg_1aa88f1d7aab95ef6f25f46053432add"
        );
        assert_eq!(
            clean_account_directory_name("wxid_example_a556").as_deref(),
            Some("wxid_example")
        );
        assert!(is_message_database(Path::new("message_0.db")));
        assert!(!is_message_database(Path::new("message_fts.db")));
    }

    #[test]
    fn production_scope_excludes_system_and_official_sessions() {
        assert!(is_direct_conversation("wxid_friend"));
        assert!(!is_direct_conversation("filehelper"));
        assert!(!is_direct_conversation("gh_official"));
        assert!(!is_direct_conversation("service_account"));
    }

    #[test]
    fn production_contact_names_prefer_remark_then_nickname_and_map_group_size() {
        let connection = Connection::open_in_memory().expect("open contact fixture");
        connection
            .execute_batch(
                "CREATE TABLE contact (username TEXT, remark TEXT, nick_name TEXT, alias TEXT);\
                 CREATE TABLE stranger (username TEXT, remark TEXT, nick_name TEXT, alias TEXT);\
                 CREATE TABLE name2id (username TEXT);\
                 CREATE TABLE chatroom_member (room_id INTEGER, member_id INTEGER);\
                 CREATE TABLE chat_room (username TEXT, ext_buffer BLOB);\
                 INSERT INTO contact VALUES ('wxid_friend', 'Friend Remark', 'Friend Nick', 'friend_alias');\
                 INSERT INTO contact VALUES ('room@chatroom', '', 'Group Name', 'room_alias');\
                 INSERT INTO contact VALUES ('wxid_member1', 'Member Remark', 'Member Nick', 'member_alias');\
                 INSERT INTO name2id(rowid, username) VALUES (100, 'room@chatroom');",
            )
            .expect("create contact fixture");
        for member in 1..=15 {
            connection
                .execute(
                    "INSERT INTO name2id(rowid, username) VALUES (?1, ?2)",
                    rusqlite::params![member, format!("wxid_member{member}")],
                )
                .expect("insert member identity");
            connection
                .execute(
                    "INSERT INTO chatroom_member(room_id, member_id) VALUES (100, ?1)",
                    [member],
                )
                .expect("insert group member");
        }
        let mut room_buffer = vec![0x0a, 12];
        room_buffer.extend_from_slice(b"wxid_member1");
        room_buffer.extend_from_slice(&[0x12, 11]);
        room_buffer.extend_from_slice(b"Group Alias");
        connection
            .execute(
                "INSERT INTO chat_room(username, ext_buffer) VALUES ('room@chatroom', ?1)",
                [room_buffer],
            )
            .expect("insert group nickname buffer");
        let sessions = vec![
            Session {
                username: "wxid_friend".to_owned(),
            },
            Session {
                username: "room@chatroom".to_owned(),
            },
        ];
        let metadata = read_conversation_metadata(&connection, &sessions).expect("read metadata");

        assert_eq!(metadata["wxid_friend"].display_name, "Friend Remark");
        assert_eq!(metadata["room@chatroom"].display_name, "Group Name");
        assert_eq!(metadata["room@chatroom"].member_count, Some(15));
        assert_eq!(
            metadata["room@chatroom"].participant_names["wxid_member1"],
            "Group Alias"
        );
        assert_eq!(
            metadata["room@chatroom"].participant_names["wxid_member2"],
            "wxid_member2"
        );
    }

    #[test]
    fn production_group_nickname_parser_rejects_unknown_members() {
        let mut buffer = vec![0x0a, 12];
        buffer.extend_from_slice(b"wxid_member1");
        buffer.extend_from_slice(&[0x12, 5]);
        buffer.extend_from_slice(b"Alice");
        let names = parse_group_nicknames(&buffer, &["wxid_other".to_owned()]);
        assert!(names.is_empty());
    }

    #[test]
    fn production_metadata_keeps_base_collection_when_optional_name_tables_are_absent() {
        let connection = Connection::open_in_memory().expect("open contact fixture");
        connection
            .execute_batch(
                "CREATE TABLE contact (username TEXT, remark TEXT, nick_name TEXT, alias TEXT);\
                 CREATE TABLE name2id (username TEXT);\
                 CREATE TABLE chatroom_member (room_id INTEGER, member_id INTEGER);\
                 INSERT INTO contact VALUES ('room@chatroom', '', 'Group Name', '');\
                 INSERT INTO contact VALUES ('wxid_member1', '', 'Member Name', '');\
                 INSERT INTO name2id(rowid, username) VALUES (1, 'wxid_member1');\
                 INSERT INTO name2id(rowid, username) VALUES (2, 'room@chatroom');\
                 INSERT INTO chatroom_member(room_id, member_id) VALUES (2, 1);",
            )
            .expect("create base contact fixture");
        let metadata = read_conversation_metadata(
            &connection,
            &[Session {
                username: "room@chatroom".to_owned(),
            }],
        )
        .expect("optional nickname tables do not gate base collection");
        assert_eq!(metadata["room@chatroom"].member_count, Some(1));
        assert_eq!(
            metadata["room@chatroom"].participant_names["wxid_member1"],
            "Member Name"
        );
    }

    #[test]
    fn production_group_sender_keeps_reading_when_one_row_has_no_identity_mapping() {
        let names = std::collections::BTreeMap::from([(
            "wxid_member1".to_owned(),
            "Group Alias".to_owned(),
        )]);
        assert_eq!(
            resolve_group_sender(Some("wxid_member1".to_owned()), 7, &names),
            ("wxid_member1".to_owned(), "Group Alias".to_owned())
        );
        assert_eq!(
            resolve_group_sender(None, 8, &names),
            ("wechat-rowid:8".to_owned(), "Unknown member".to_owned())
        );
    }

    #[test]
    fn production_initial_history_window_is_sixty_days() {
        let now = 200 * 24 * 60 * 60;
        assert_eq!(retention_cutoff_from(now), 140 * 24 * 60 * 60);
    }

    #[test]
    fn production_text_decoder_accepts_plain_utf8() {
        assert_eq!(
            decode_value(&Value::Blob("hello".as_bytes().to_vec())).as_deref(),
            Some("hello")
        );
        assert!(decode_value(&Value::Null).is_none());
    }

    #[test]
    fn production_text_decoder_accepts_zstd_utf8() {
        let compressed = zstd::stream::encode_all("compressed hello".as_bytes(), 1)
            .expect("compress fixture text");
        assert_eq!(
            decode_value(&Value::Blob(compressed)).as_deref(),
            Some("compressed hello")
        );
    }
}
