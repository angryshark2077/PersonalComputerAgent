use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    fs::OpenOptions,
    io::{Read, Write},
    os::unix::ffi::OsStrExt,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use aes::Aes128;
use cipher::{generic_array::GenericArray, BlockDecrypt, KeyInit};
use md5::{Digest as _, Md5};
use pca_domain::{CommunicationAttachment, DomainError, MessageKind};
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
const HARDLINK_DATABASE: &str = "db_storage/hardlink/hardlink.db";
const MESSAGE_DIRECTORY: &str = "db_storage/message";
const MAX_BATCH: usize = 200;
const MAX_PER_CONVERSATION: usize = 20;
const INITIAL_HISTORY_SECONDS: u64 = 60 * 24 * 60 * 60;
const DATABASE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_IMAGE_BYTES: u64 = 100 * 1024 * 1024;

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
    account_root: PathBuf,
    session_database: PathBuf,
    contact_database: PathBuf,
    message_databases: Vec<PathBuf>,
    local_username: String,
    account_id: String,
}

#[derive(Clone)]
struct ImageKeys {
    xor: u8,
    aes: [u8; 16],
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
                account_root: account_root.clone(),
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
            tokio::task::spawn_blocking(move || read_message_batch(&paths, &cursors))
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

fn read_message_batch(
    paths: &SourcePaths,
    cursors: &Mutex<BTreeMap<String, i64>>,
) -> Result<Vec<SourceRecord>, DomainError> {
    let material = load_material(paths)?;
    let sessions = with_database(&paths.session_database, &material, read_sessions)
        .map_err(|_| read_stage_error("WECHAT_SESSION_READ_FAILED"))?;
    let conversation_metadata = with_database(&paths.contact_database, &material, |connection| {
        read_conversation_metadata(connection, &sessions)
    })
    .map_err(|_| read_stage_error("WECHAT_CONTACT_READ_FAILED"))?;
    let cutoff = retention_cutoff();
    let decoded_images = index_decoded_images(&paths.account_root, cutoff);
    let encrypted_images = index_encrypted_image_identities(&paths.account_root);
    let hardlink_path = paths.account_root.join(HARDLINK_DATABASE);
    let image_hardlinks = if hardlink_path.is_file() {
        with_database(&hardlink_path, &material, |connection| {
            read_image_hardlinks(connection, &paths.account_root)
        })
        .unwrap_or_default()
    } else {
        BTreeMap::new()
    };
    let image_keys = derive_image_keys(&paths.account_root);
    let mut records = Vec::new();
    let mut image_rows_scanned = 0_usize;
    let mut image_cache_matches = 0_usize;
    let mut image_metadata_matches = 0_usize;
    let mut image_dat_matches = 0_usize;
    let mut image_index_matches = 0_usize;
    let mut image_hardlink_matches = 0_usize;
    let mut image_records = 0_usize;
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
        let cursor_snapshot = cursor_guard.clone();
        let remaining = MAX_BATCH - records.len();
        let context = TextReadContext {
            database_name: &database_name,
            local_username: &paths.local_username,
            account_id: material.account_id(),
            conversation_metadata: &conversation_metadata,
            cursors: &cursor_snapshot,
            cutoff,
        };
        let image_batch = with_database(database, &material, |connection| {
            read_database_images(
                connection,
                &context,
                &sessions,
                &decoded_images,
                &encrypted_images,
                &image_hardlinks,
                image_keys.as_ref(),
                &paths.account_root,
                remaining,
            )
        })
        .map_err(|_| read_stage_error("WECHAT_IMAGE_READ_FAILED"))?;
        image_rows_scanned += image_batch.rows_scanned;
        image_cache_matches += image_batch.cache_matches;
        image_metadata_matches += image_batch.metadata_matches;
        image_dat_matches += image_batch.dat_matches;
        image_index_matches += image_batch.index_matches;
        image_hardlink_matches += image_batch.hardlink_matches;
        image_records += image_batch.records.len();
        for (cursor_key, sequence) in image_batch.cursor_updates {
            cursor_guard
                .entry(cursor_key)
                .and_modify(|current| *current = (*current).max(sequence))
                .or_insert(sequence);
        }
        for record in image_batch.records {
            records.push(SourceRecord::Message(Box::new(record)));
        }
        if records.len() >= MAX_BATCH {
            break;
        }
        let remaining = MAX_BATCH - records.len();
        let batch = with_database(database, &material, |connection| {
            read_database_text(connection, &context, &sessions, remaining)
        })
        .map_err(|_| read_stage_error("WECHAT_MESSAGE_READ_FAILED"))?;
        for (cursor_key, sequence, record) in batch {
            cursor_guard
                .entry(cursor_key)
                .and_modify(|current| *current = (*current).max(sequence))
                .or_insert(sequence);
            records.push(SourceRecord::Message(Box::new(record)));
        }
    }
    if std::env::var_os("PCA_WECHAT_MEDIA_DIAGNOSTIC").is_some() {
        eprintln!(
            "WeChat image scan: decoded={} encrypted={} hardlinks={} keys={} rows={} cache_matches={} metadata={} direct_dat={} indexed_dat={} hardlink_dat={} records={}",
            decoded_images
                .values()
                .filter(|path| path.is_some())
                .count(),
            encrypted_images.len(),
            image_hardlinks.len(),
            usize::from(image_keys.is_some()),
            image_rows_scanned,
            image_cache_matches,
            image_metadata_matches,
            image_dat_matches,
            image_index_matches,
            image_hardlink_matches,
            image_records
        );
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
                avatar_url: None,
                member_count: None,
                participant_names: BTreeMap::new(),
                participant_avatar_urls: BTreeMap::new(),
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
            let (sender_id, sender_display_name, sender_avatar_url) = match direction {
                SourceDirection::Outgoing => {
                    (context.local_username.to_owned(), "You".to_owned(), None)
                }
                SourceDirection::Incoming if session.username.ends_with("@chatroom") => {
                    let sender_id = sender_statement
                        .query_row([row.real_sender_id], |sender_row| {
                            sender_row.get::<_, String>(0)
                        })
                        .optional()
                        .ok()
                        .flatten()
                        .filter(|value| valid_identity(value));
                    let (sender_id, sender_display_name) = resolve_group_sender(
                        sender_id,
                        row.real_sender_id,
                        &metadata.participant_names,
                    );
                    let sender_avatar_url =
                        metadata.participant_avatar_urls.get(&sender_id).cloned();
                    (sender_id, sender_display_name, sender_avatar_url)
                }
                SourceDirection::Incoming => (
                    session.username.clone(),
                    metadata.display_name.clone(),
                    metadata.avatar_url.clone(),
                ),
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
                    conversation_avatar_url: metadata.avatar_url.clone(),
                    sender_id,
                    sender_display_name,
                    sender_avatar_url,
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

#[allow(
    clippy::too_many_lines,
    reason = "image rows use the same fail-closed identity mapping as text rows"
)]
fn read_database_images(
    connection: &Connection,
    context: &TextReadContext<'_>,
    sessions: &[Session],
    decoded_images: &BTreeMap<(i64, i64), Option<PathBuf>>,
    encrypted_images: &BTreeSet<String>,
    image_hardlinks: &BTreeMap<String, PathBuf>,
    image_keys: Option<&ImageKeys>,
    account_root: &Path,
    limit: usize,
) -> Result<ImageReadBatch, SqlcipherProbeFailure> {
    let my_rowid = local_sender_rowid(connection, context.local_username)?
        .ok_or(SqlcipherProbeFailure::AccountUnverified)?;
    let mut sender_statement = connection
        .prepare("SELECT user_name FROM Name2Id WHERE rowid = ?1 LIMIT 1")
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
    let mut records = Vec::new();
    let mut cursor_updates = Vec::new();
    let mut rows_scanned = 0_usize;
    let mut cache_matches = 0_usize;
    let mut metadata_matches = 0_usize;
    let mut dat_matches = 0_usize;
    let mut index_matches = 0_usize;
    let mut hardlink_matches = 0_usize;
    let media_ready_cutoff = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .saturating_sub(5 * 60),
    )
    .unwrap_or(i64::MAX);
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
                avatar_url: None,
                member_count: None,
                participant_names: BTreeMap::new(),
                participant_avatar_urls: BTreeMap::new(),
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
        let cursor_key = format!("{}:{}:image", context.database_name, session.username);
        let after = context.cursors.get(&cursor_key).copied().unwrap_or(0);
        let per_conversation = MAX_PER_CONVERSATION.min(limit - records.len());
        let sql = format!(
            "SELECT local_id, server_id, create_time, real_sender_id, status, \
                    message_content, compress_content, packed_info_data \
             FROM \"{table_name}\" \
             WHERE local_type = 3 AND local_id > ?1 AND create_time >= ?2 \
             ORDER BY local_id ASC"
        );
        let mut statement = connection
            .prepare(&sql)
            .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
        let rows = statement
            .query_map((after, context.cutoff), |row| {
                Ok(ImageMessageRow {
                    local_id: row.get(0)?,
                    server_id: row.get(1)?,
                    create_time: row.get(2)?,
                    real_sender_id: row.get(3)?,
                    status: row.get(4)?,
                    message_content: row.get(5)?,
                    compress_content: row.get(6)?,
                    packed_info_data: row.get(7)?,
                })
            })
            .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
        let mut scanned_through = after;
        let records_before_session = records.len();
        for row in rows {
            if records.len() - records_before_session >= per_conversation {
                break;
            }
            let row = row.map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
            rows_scanned += 1;
            if row.local_id <= 0 || row.create_time <= 0 {
                continue;
            }
            let content = decode_message_content(&row.compress_content, &row.message_content);
            let image_md5 = content.as_deref().and_then(parse_image_md5);
            let dat_name = parse_image_dat_name(&row.packed_info_data);
            if image_md5.is_some() || dat_name.is_some() {
                metadata_matches += 1;
            }
            if resolve_image_dat_path(
                account_root,
                &table_name,
                row.create_time,
                image_md5.as_deref(),
                dat_name.as_deref(),
            )
            .is_some()
            {
                dat_matches += 1;
            }
            if [image_md5.as_deref(), dat_name.as_deref()]
                .into_iter()
                .flatten()
                .any(|identity| encrypted_images.contains(identity))
            {
                index_matches += 1;
            }
            if image_md5
                .as_ref()
                .is_some_and(|md5| image_hardlinks.contains_key(md5))
            {
                hardlink_matches += 1;
            }
            let completed_image = decoded_image_attachment(
                decoded_images,
                context.database_name,
                &table_name,
                row.local_id,
                row.create_time,
            )
            .or_else(|| {
                let source = image_md5
                    .as_ref()
                    .and_then(|md5| image_hardlinks.get(md5))?;
                decrypted_image_attachment(
                    source,
                    image_keys?,
                    context.database_name,
                    &table_name,
                    row.local_id,
                )
            });
            let Some((attachment, source_path)) = completed_image else {
                if row.create_time > media_ready_cutoff {
                    break;
                }
                scanned_through = row.local_id;
                continue;
            };
            cache_matches += 1;
            scanned_through = row.local_id;
            let direction = if row.real_sender_id == my_rowid {
                if row.server_id <= 0 || row.status < 0 {
                    continue;
                }
                SourceDirection::Outgoing
            } else {
                SourceDirection::Incoming
            };
            let (sender_id, sender_display_name, sender_avatar_url) = match direction {
                SourceDirection::Outgoing => {
                    (context.local_username.to_owned(), "You".to_owned(), None)
                }
                SourceDirection::Incoming if session.username.ends_with("@chatroom") => {
                    let sender_id = sender_statement
                        .query_row([row.real_sender_id], |sender_row| {
                            sender_row.get::<_, String>(0)
                        })
                        .optional()
                        .ok()
                        .flatten()
                        .filter(|value| valid_identity(value));
                    let (sender_id, sender_display_name) = resolve_group_sender(
                        sender_id,
                        row.real_sender_id,
                        &metadata.participant_names,
                    );
                    let sender_avatar_url =
                        metadata.participant_avatar_urls.get(&sender_id).cloned();
                    (sender_id, sender_display_name, sender_avatar_url)
                }
                SourceDirection::Incoming => (
                    session.username.clone(),
                    metadata.display_name.clone(),
                    metadata.avatar_url.clone(),
                ),
                SourceDirection::Unknown => continue,
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
                "wechat:{}:{table_name}:{}:image",
                context.database_name, row.local_id
            );
            let attachment_id = attachment.attachment_id().to_owned();
            records.push(SourceMessageRecord {
                account_id: context.account_id.to_owned(),
                source_sequence: u64::try_from(row.local_id)
                    .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?,
                message_id,
                conversation_id: session.username.clone(),
                conversation_display_name: metadata.display_name.clone(),
                conversation_avatar_url: metadata.avatar_url.clone(),
                sender_id,
                sender_display_name,
                sender_avatar_url,
                source_key,
                occurred_at,
                local_account: LocalAccountProof::Verified,
                direction,
                kind: SourceMessageKind::Image,
                conversation: conversation.clone(),
                finality: match direction {
                    SourceDirection::Incoming => SourceFinality::IncomingPersisted,
                    SourceDirection::Outgoing => SourceFinality::OutgoingSent,
                    SourceDirection::Unknown => SourceFinality::Unknown,
                },
                payload: SourcePayload::Media {
                    attachment: Some(attachment),
                    completed_source: Some(crate::source::SourceCompletedMedia {
                        attachment_id,
                        source_path,
                    }),
                },
            });
        }
        if scanned_through > after {
            cursor_updates.push((cursor_key, scanned_through));
        }
    }
    Ok(ImageReadBatch {
        records,
        cursor_updates,
        rows_scanned,
        cache_matches,
        metadata_matches,
        dat_matches,
        index_matches,
        hardlink_matches,
    })
}

struct ImageReadBatch {
    records: Vec<SourceMessageRecord>,
    cursor_updates: Vec<(String, i64)>,
    rows_scanned: usize,
    cache_matches: usize,
    metadata_matches: usize,
    dat_matches: usize,
    index_matches: usize,
    hardlink_matches: usize,
}

fn derive_image_keys(account_root: &Path) -> Option<ImageKeys> {
    let raw_account = account_root.file_name()?.to_str()?.to_owned();
    let mut account_candidates = BTreeSet::from([raw_account.clone()]);
    if let Some(cleaned) = clean_account_directory_name(&raw_account) {
        account_candidates.insert(cleaned);
    }
    let codes = collect_kvcomm_codes(account_root);
    let templates = collect_v4_templates(&account_root.join("msg").join("attach"), 32);
    for template in templates {
        let bytes = fs::read(template).ok()?;
        let ciphertext: [u8; 16] = bytes.get(15..31)?.try_into().ok()?;
        for account in &account_candidates {
            for code in &codes {
                let digest = Md5::digest(format!("{code}{account}").as_bytes());
                let hex = format!("{digest:x}");
                let aes: [u8; 16] = hex.as_bytes().get(..16)?.try_into().ok()?;
                let keys = ImageKeys {
                    xor: (*code & 0xff) as u8,
                    aes,
                };
                if decrypt_aes_block(&ciphertext, &keys.aes)
                    .is_some_and(|plain| image_or_wxgf_magic(&plain))
                {
                    return Some(keys);
                }
            }
        }
    }
    None
}

fn collect_kvcomm_codes(account_root: &Path) -> BTreeSet<u32> {
    let mut directories = BTreeSet::new();
    if let Ok(home) = env::var("HOME") {
        let container = PathBuf::from(home).join("Library/Containers/com.tencent.xinWeChat/Data");
        directories.insert(container.join("Documents/app_data/net/kvcomm"));
        directories.insert(
            container.join("Library/Application Support/com.tencent.xinWeChat/xwechat/net/kvcomm"),
        );
        directories
            .insert(container.join("Library/Application Support/com.tencent.xinWeChat/net/kvcomm"));
        directories.insert(container.join("Documents/xwechat/net/kvcomm"));
    }
    let mut cursor = Some(account_root);
    for _ in 0..6 {
        let Some(path) = cursor else {
            break;
        };
        directories.insert(path.join("net/kvcomm"));
        cursor = path.parent();
    }
    let mut codes = BTreeSet::new();
    for directory in directories {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Some(filename) = entry.file_name().to_str().map(ToOwned::to_owned) else {
                continue;
            };
            let Some(rest) = filename.strip_prefix("key_") else {
                continue;
            };
            let Some((code, suffix)) = rest.split_once('_') else {
                continue;
            };
            if !suffix.ends_with(".statistic") {
                continue;
            }
            if let Ok(code) = code.parse::<u32>() {
                codes.insert(code);
            }
        }
    }
    codes
}

fn collect_v4_templates(root: &Path, limit: usize) -> Vec<PathBuf> {
    fn visit(directory: &Path, limit: usize, templates: &mut Vec<PathBuf>) {
        if templates.len() >= limit {
            return;
        }
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            if templates.len() >= limit {
                break;
            }
            let path = entry.path();
            let Ok(metadata) = path.symlink_metadata() else {
                continue;
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                visit(&path, limit, templates);
            } else if metadata.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with("_t.dat"))
                && fs::read(&path)
                    .ok()
                    .is_some_and(|bytes| bytes.starts_with(&[0x07, 0x08, b'V', b'2', 0x08, 0x07]))
            {
                templates.push(path);
            }
        }
    }
    let mut templates = Vec::new();
    visit(root, limit, &mut templates);
    templates
}

fn decrypt_aes_block(ciphertext: &[u8; 16], key: &[u8; 16]) -> Option<[u8; 16]> {
    let cipher = Aes128::new_from_slice(key).ok()?;
    let mut block = GenericArray::clone_from_slice(ciphertext);
    cipher.decrypt_block(&mut block);
    Some(block.into())
}

fn image_or_wxgf_magic(bytes: &[u8]) -> bool {
    image_mime_type(bytes).is_some() || bytes.starts_with(b"wxgf")
}

fn decrypted_image_attachment(
    encrypted_path: &Path,
    keys: &ImageKeys,
    database_name: &str,
    table_name: &str,
    local_id: i64,
) -> Option<(CommunicationAttachment, PathBuf)> {
    let encrypted = fs::read(encrypted_path).ok()?;
    let decrypted = decrypt_v4_image(&encrypted, keys)?;
    let mime_type = image_mime_type(&decrypted)?.to_owned();
    let sha256 = format!("{:x}", Sha256::digest(&decrypted));
    let source_path = stage_decrypted_image(&decrypted, &sha256, &mime_type)?;
    let table_hash = table_name.strip_prefix("Msg_")?;
    let attachment_id = format!("wechat-image:{database_name}:{table_hash}:{local_id}");
    let attachment = CommunicationAttachment::try_new(
        attachment_id,
        MessageKind::Image,
        sha256,
        u64::try_from(decrypted.len()).ok()?,
        mime_type,
    )
    .ok()?;
    Some((attachment, source_path))
}

fn decrypt_v4_image(encrypted: &[u8], keys: &ImageKeys) -> Option<Vec<u8>> {
    if encrypted.len() < 31 || !encrypted.starts_with(&[0x07, 0x08, b'V', b'2', 0x08, 0x07]) {
        return None;
    }
    let aes_size =
        usize::try_from(i32::from_le_bytes(encrypted.get(6..10)?.try_into().ok()?)).ok()?;
    let xor_size =
        usize::try_from(i32::from_le_bytes(encrypted.get(10..14)?.try_into().ok()?)).ok()?;
    let payload = encrypted.get(15..)?;
    let aligned_aes_size = aes_size.checked_add(16 - (aes_size % 16))?;
    if aligned_aes_size > payload.len() || aligned_aes_size % 16 != 0 {
        return None;
    }
    let cipher = Aes128::new_from_slice(&keys.aes).ok()?;
    let mut aes_plain = Vec::with_capacity(aligned_aes_size);
    for chunk in payload.get(..aligned_aes_size)?.chunks_exact(16) {
        let mut block = GenericArray::clone_from_slice(chunk);
        cipher.decrypt_block(&mut block);
        aes_plain.extend_from_slice(&block);
    }
    let padding = usize::from(*aes_plain.last()?);
    if padding == 0
        || padding > 16
        || padding > aes_plain.len()
        || !aes_plain[aes_plain.len() - padding..]
            .iter()
            .all(|byte| usize::from(*byte) == padding)
    {
        return None;
    }
    aes_plain.truncate(aes_plain.len() - padding);
    let remaining = payload.get(aligned_aes_size..)?;
    if xor_size > remaining.len() {
        return None;
    }
    let raw_size = remaining.len() - xor_size;
    let mut output = aes_plain;
    output.extend_from_slice(remaining.get(..raw_size)?);
    output.extend(
        remaining
            .get(raw_size..)?
            .iter()
            .map(|byte| byte ^ keys.xor),
    );
    while output.last() == Some(&0) {
        output.pop();
    }
    (output.len() <= usize::try_from(MAX_IMAGE_BYTES).ok()? && image_or_wxgf_magic(&output))
        .then_some(output)
}

fn stage_decrypted_image(bytes: &[u8], sha256: &str, mime_type: &str) -> Option<PathBuf> {
    let root = fs::canonicalize(env::temp_dir())
        .ok()?
        .join("pca-wechat-media");
    match root.symlink_metadata() {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return None,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(&root).ok()?;
        }
        Err(_) => return None,
    }
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).ok()?;
    cleanup_staged_images(&root);
    let extension = match mime_type {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => return None,
    };
    let path = root.join(format!("{sha256}.{extension}"));
    if path.is_file() && fs::metadata(&path).ok()?.len() == u64::try_from(bytes.len()).ok()? {
        return Some(path);
    }
    let _ = fs::remove_file(&path);
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .ok()?;
    if output.write_all(bytes).is_err() || output.sync_all().is_err() {
        let _ = fs::remove_file(&path);
        return None;
    }
    Some(path)
}

fn cleanup_staged_images(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = path.symlink_metadata() else {
            continue;
        };
        if metadata.file_type().is_file()
            && !metadata.file_type().is_symlink()
            && metadata
                .modified()
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age > Duration::from_secs(60 * 60))
        {
            let _ = fs::remove_file(path);
        }
    }
}

fn read_image_hardlinks(
    connection: &Connection,
    account_root: &Path,
) -> Result<BTreeMap<String, PathBuf>, SqlcipherProbeFailure> {
    let image_table = connection
        .query_row(
            "SELECT name FROM sqlite_master \
             WHERE type='table' AND name LIKE 'image_hardlink_info%' \
             ORDER BY name DESC LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?
        .filter(|name| {
            name.starts_with("image_hardlink_info")
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        })
        .ok_or(SqlcipherProbeFailure::UnsupportedSchema)?;
    require_columns(
        connection,
        &image_table,
        &["md5", "file_name", "dir1", "dir2"],
    )?;
    require_columns(connection, "dir2id", &["username"])?;
    let sql = format!(
        "SELECT lower(h.md5), h.file_name, d1.username, d2.username \
         FROM \"{image_table}\" h \
         JOIN dir2id d1 ON d1.rowid = h.dir1 \
         JOIN dir2id d2 ON d2.rowid = h.dir2 \
         WHERE length(h.md5) BETWEEN 16 AND 32 \
         ORDER BY CASE WHEN lower(h.file_name) LIKE '%_t.dat' THEN 1 ELSE 0 END, h.rowid DESC \
         LIMIT 100000"
    );
    let mut statement = connection
        .prepare(&sql)
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
    let attach_root = account_root.join("msg").join("attach");
    let mut paths = BTreeMap::new();
    for row in rows {
        let (md5, file_name, dir1, dir2) =
            row.map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
        if valid_md5(&md5).is_none()
            || !valid_path_component(&file_name)
            || !valid_path_component(&dir1)
            || !valid_path_component(&dir2)
        {
            continue;
        }
        for relative in [
            PathBuf::from(&dir1)
                .join(&dir2)
                .join("Img")
                .join(&file_name),
            PathBuf::from(&dir1).join(&dir2).join("mg").join(&file_name),
            PathBuf::from(&dir1).join(&dir2).join(&file_name),
        ] {
            let candidate = attach_root.join(relative);
            let Ok(metadata) = candidate.symlink_metadata() else {
                continue;
            };
            if metadata.file_type().is_file()
                && !metadata.file_type().is_symlink()
                && metadata.len() > 0
                && metadata.len() <= MAX_IMAGE_BYTES
            {
                paths.entry(md5.clone()).or_insert(candidate);
                break;
            }
        }
    }
    Ok(paths)
}

fn valid_path_component(value: &str) -> bool {
    if value.is_empty() || value.len() > 255 {
        return false;
    }
    let mut components = Path::new(value).components();
    matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none()
}

fn decoded_image_attachment(
    decoded_images: &BTreeMap<(i64, i64), Option<PathBuf>>,
    database_name: &str,
    table_name: &str,
    local_id: i64,
    create_time: i64,
) -> Option<(CommunicationAttachment, PathBuf)> {
    let table_hash = table_name.strip_prefix("Msg_")?;
    let source_path = decoded_images
        .get(&(local_id, create_time))?
        .as_ref()?
        .clone();
    let metadata = fs::symlink_metadata(&source_path).ok()?;
    if !metadata.file_type().is_file() || metadata.len() == 0 || metadata.len() > MAX_IMAGE_BYTES {
        return None;
    }
    let mut file = fs::File::open(&source_path).ok()?;
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).ok()?);
    file.read_to_end(&mut bytes).ok()?;
    if u64::try_from(bytes.len()).ok()? != metadata.len() {
        return None;
    }
    let mime_type = image_mime_type(&bytes)?.to_owned();
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    let attachment_id = format!("wechat-image:{database_name}:{table_hash}:{local_id}");
    let attachment = CommunicationAttachment::try_new(
        attachment_id,
        MessageKind::Image,
        sha256,
        metadata.len(),
        mime_type,
    )
    .ok()?;
    Some((attachment, source_path))
}

fn index_decoded_images(account_root: &Path, cutoff: i64) -> BTreeMap<(i64, i64), Option<PathBuf>> {
    let mut images = BTreeMap::new();
    let Ok(months) = fs::read_dir(account_root.join("cache")) else {
        return images;
    };
    for month in months.flatten() {
        let Ok(month_metadata) = month.path().symlink_metadata() else {
            continue;
        };
        if !month_metadata.is_dir() || month_metadata.file_type().is_symlink() {
            continue;
        }
        let message_root = month.path().join("Message");
        let Ok(conversations) = fs::read_dir(message_root) else {
            continue;
        };
        for conversation in conversations.flatten() {
            let Ok(conversation_metadata) = conversation.path().symlink_metadata() else {
                continue;
            };
            if !conversation_metadata.is_dir() || conversation_metadata.file_type().is_symlink() {
                continue;
            }
            let thumb_root = conversation.path().join("Thumb");
            let Ok(thumbs) = fs::read_dir(thumb_root) else {
                continue;
            };
            for thumb in thumbs.flatten() {
                let path = thumb.path();
                let Ok(metadata) = path.symlink_metadata() else {
                    continue;
                };
                if !metadata.is_file() || metadata.file_type().is_symlink() {
                    continue;
                }
                let Some((local_id, create_time)) = decoded_image_file_identity(&path) else {
                    continue;
                };
                if create_time < cutoff {
                    continue;
                }
                images
                    .entry((local_id, create_time))
                    .and_modify(|candidate| *candidate = None)
                    .or_insert_with(|| Some(path));
            }
        }
    }
    images
}

fn index_encrypted_image_identities(account_root: &Path) -> BTreeSet<String> {
    fn visit(directory: &Path, depth: usize, identities: &mut BTreeSet<String>) {
        if depth > 5 || identities.len() >= 100_000 {
            return;
        }
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = path.symlink_metadata() else {
                continue;
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                visit(&path, depth + 1, identities);
            } else if metadata.is_file() {
                if let Some(identity) = normalized_dat_identity(&path) {
                    identities.insert(identity);
                }
            }
        }
    }

    let mut identities = BTreeSet::new();
    visit(&account_root.join("msg").join("attach"), 0, &mut identities);
    identities
}

fn normalized_dat_identity(path: &Path) -> Option<String> {
    let filename = path.file_name()?.to_str()?.to_ascii_lowercase();
    let mut base = filename.strip_suffix(".dat")?;
    for suffix in [
        "_thumb", ".thumb", "_hd", ".hd", "_h", ".h", "_b", ".b", "_w", ".w", "_t", ".t", "_c",
        ".c",
    ] {
        if let Some(stripped) = base.strip_suffix(suffix) {
            base = stripped;
            break;
        }
    }
    (!base.is_empty()
        && base.len() <= 255
        && base
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'_'))
    .then(|| base.to_owned())
}

fn decoded_image_file_identity(path: &Path) -> Option<(i64, i64)> {
    let file_name = path.file_name()?.to_str()?;
    let identity = file_name.strip_suffix("_thumb.jpg")?;
    let (local_id, create_time) = identity.split_once('_')?;
    let local_id = local_id.parse().ok()?;
    let create_time = create_time.parse().ok()?;
    (local_id > 0 && create_time > 0).then_some((local_id, create_time))
}

fn image_mime_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        Some("image/png")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

struct Session {
    username: String,
}

#[derive(Clone)]
struct ConversationMetadata {
    display_name: String,
    avatar_url: Option<String>,
    member_count: Option<u8>,
    participant_names: BTreeMap<String, String>,
    participant_avatar_urls: BTreeMap<String, String>,
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

struct ImageMessageRow {
    local_id: i64,
    server_id: i64,
    create_time: i64,
    real_sender_id: i64,
    status: i64,
    message_content: Value,
    compress_content: Value,
    packed_info_data: Value,
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
    let contact_names = read_display_names(connection, "contact")?;
    let stranger_names = read_display_names(connection, "stranger").unwrap_or_default();
    let contact_avatar_urls = read_avatar_urls(connection, "contact").unwrap_or_default();
    let stranger_avatar_urls = read_avatar_urls(connection, "stranger").unwrap_or_default();
    let group_members = read_group_members(connection).unwrap_or_default();
    let room_buffers = read_room_buffers(connection).unwrap_or_default();
    let mut metadata = BTreeMap::new();
    for session in sessions {
        let display_name = contact_names
            .get(&session.username)
            .or_else(|| stranger_names.get(&session.username))
            .cloned()
            .unwrap_or_else(|| session.username.clone());
        let avatar_url = contact_avatar_urls
            .get(&session.username)
            .or_else(|| stranger_avatar_urls.get(&session.username))
            .cloned();
        let (member_count, participant_names, participant_avatar_urls) =
            if session.username.ends_with("@chatroom") {
                let members = group_members
                    .get(&session.username)
                    .cloned()
                    .unwrap_or_default();
                let count = u8::try_from(members.len()).ok();
                let group_nicknames = room_buffers
                    .get(&session.username)
                    .map(Vec::as_slice)
                    .map(|buffer| parse_group_nicknames(buffer, &members))
                    .unwrap_or_default();
                let mut participant_names = BTreeMap::new();
                let mut participant_avatar_urls = BTreeMap::new();
                for member in members {
                    let name = group_nicknames
                        .get(&member)
                        .cloned()
                        .or_else(|| contact_names.get(&member).cloned())
                        .or_else(|| stranger_names.get(&member).cloned())
                        .unwrap_or_else(|| member.clone());
                    participant_names.insert(member.clone(), name);
                    if let Some(avatar_url) = contact_avatar_urls
                        .get(&member)
                        .or_else(|| stranger_avatar_urls.get(&member))
                        .cloned()
                    {
                        participant_avatar_urls.insert(member.clone(), avatar_url);
                    }
                }
                (count, participant_names, participant_avatar_urls)
            } else {
                (None, BTreeMap::new(), BTreeMap::new())
            };
        metadata.insert(
            session.username.clone(),
            ConversationMetadata {
                display_name,
                avatar_url,
                member_count,
                participant_names,
                participant_avatar_urls,
            },
        );
    }
    Ok(metadata)
}

fn read_avatar_urls(
    connection: &Connection,
    table: &str,
) -> Result<BTreeMap<String, String>, SqlcipherProbeFailure> {
    let columns = table_columns(connection, table)?;
    let big = columns.iter().any(|column| column == "big_head_url");
    let small = columns.iter().any(|column| column == "small_head_url");
    if !big && !small {
        return Ok(BTreeMap::new());
    }
    let big_expression = if big { "big_head_url" } else { "NULL" };
    let small_expression = if small { "small_head_url" } else { "NULL" };
    let sql = format!("SELECT username, {big_expression}, {small_expression} FROM \"{table}\"");
    let mut statement = connection
        .prepare(&sql)
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
    let mut avatars = BTreeMap::new();
    for (username, big, small) in rows.flatten() {
        if username.is_empty() || username.chars().any(char::is_control) {
            continue;
        }
        if let Some(url) = [big, small]
            .into_iter()
            .flatten()
            .find_map(|value| normalize_avatar_url(&value))
        {
            avatars.insert(username, url);
        }
    }
    Ok(avatars)
}

fn normalize_avatar_url(value: &str) -> Option<String> {
    let value = value.trim();
    let normalized = value
        .strip_prefix("http://")
        .map_or_else(|| value.to_owned(), |rest| format!("https://{rest}"));
    (normalized.starts_with("https://")
        && normalized.len() <= 4096
        && !normalized.chars().any(char::is_control))
    .then_some(normalized)
}

fn read_display_names(
    connection: &Connection,
    table: &str,
) -> Result<BTreeMap<String, String>, SqlcipherProbeFailure> {
    let sql = format!("SELECT username, remark, nick_name, alias FROM \"{table}\"");
    let mut statement = connection
        .prepare(&sql)
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
    let mut names = BTreeMap::new();
    for (username, remark, nickname, alias) in rows.flatten() {
        if username.is_empty() || username.chars().any(char::is_control) {
            continue;
        }
        if let Some(name) = [remark, nickname, alias]
            .into_iter()
            .flatten()
            .find_map(|value| valid_display_name(&value))
        {
            names.insert(username, name);
        }
    }
    Ok(names)
}

fn read_group_members(
    connection: &Connection,
) -> Result<BTreeMap<String, Vec<String>>, SqlcipherProbeFailure> {
    let mut statement = connection
        .prepare(
            "SELECT room.username, member.username FROM chatroom_member membership \
             JOIN name2id room ON membership.room_id = room.rowid \
             JOIN name2id member ON membership.member_id = member.rowid",
        )
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
    let mut groups = BTreeMap::<String, Vec<String>>::new();
    for (room, member) in rows.flatten() {
        if room.ends_with("@chatroom") && valid_identity(&member) {
            groups.entry(room).or_default().push(member);
        }
    }
    Ok(groups)
}

fn read_room_buffers(
    connection: &Connection,
) -> Result<BTreeMap<String, Vec<u8>>, SqlcipherProbeFailure> {
    let mut statement = connection
        .prepare("SELECT username, ext_buffer FROM chat_room")
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Value>(1)?))
        })
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
    Ok(rows
        .flatten()
        .filter_map(|(username, value)| value_bytes(value).map(|buffer| (username, buffer)))
        .collect())
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
            if let Some(name) = valid_display_name(&cleaned) {
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

fn valid_display_name(value: &str) -> Option<String> {
    let value = value.trim().to_owned();
    (!value.is_empty() && value.len() <= 1024 && !value.chars().any(char::is_control))
        .then_some(value)
}

fn read_stage_error(code: &str) -> DomainError {
    DomainError::new(code, "WeChat read stage failed", true)
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
    let columns = table_columns(connection, table)?;
    if required
        .iter()
        .all(|required| columns.iter().any(|column| column == required))
    {
        Ok(())
    } else {
        Err(SqlcipherProbeFailure::UnsupportedSchema)
    }
}

fn table_columns(
    connection: &Connection,
    table: &str,
) -> Result<Vec<String>, SqlcipherProbeFailure> {
    let mut statement = connection
        .prepare("SELECT name FROM pragma_table_info(?1)")
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
    let columns = statement
        .query_map([table], |row| row.get::<_, String>(0))
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
    Ok(columns)
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

fn parse_image_md5(content: &str) -> Option<String> {
    const ATTRIBUTES: [&str; 5] = ["md5", "cdnthumbmd5", "thumbfullmd5", "fullmd5", "newmd5"];
    let lowercase = content.to_ascii_lowercase();
    for attribute in ATTRIBUTES {
        let mut search_from = 0;
        while let Some(relative) = lowercase[search_from..].find(attribute) {
            let start = search_from + relative + attribute.len();
            let tail = &content[start..];
            let trimmed = tail.trim_start();
            if let Some(value) = trimmed.strip_prefix('=') {
                let value = value.trim_start();
                let candidate = match value.as_bytes().first().copied() {
                    Some(b'\'') => value[1..].split('\'').next(),
                    Some(b'"') => value[1..].split('"').next(),
                    _ => value
                        .split(|character: char| character.is_whitespace() || character == '>')
                        .next(),
                };
                if let Some(candidate) = candidate.and_then(valid_md5) {
                    return Some(candidate);
                }
            }
            search_from = start;
        }
    }
    for tag in ["md5", "cdnthumbmd5", "thumbfullmd5", "fullmd5", "newmd5"] {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        if let Some(start) = lowercase.find(&open) {
            let value_start = start + open.len();
            if let Some(relative_end) = lowercase[value_start..].find(&close) {
                if let Some(candidate) =
                    valid_md5(&content[value_start..value_start + relative_end])
                {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

fn valid_md5(value: &str) -> Option<String> {
    let value = value.trim();
    (value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| value.to_ascii_lowercase())
}

fn parse_image_dat_name(value: &Value) -> Option<String> {
    let bytes = match value {
        Value::Blob(bytes) => bytes.as_slice(),
        Value::Text(text) => text.as_bytes(),
        _ => return None,
    };
    let printable = bytes
        .iter()
        .map(|byte| {
            if byte.is_ascii_graphic() {
                char::from(*byte)
            } else {
                ' '
            }
        })
        .collect::<String>();
    printable.split_ascii_whitespace().find_map(|token| {
        let filename = token.rsplit(['/', '\\']).next()?;
        let lowercase = filename.to_ascii_lowercase();
        let dat_end = lowercase.find(".dat")?;
        let base = &filename[..dat_end];
        let base = base.strip_suffix(".t").unwrap_or(base);
        let base = base.strip_suffix("_t").unwrap_or(base);
        (!base.is_empty()
            && base.len() <= 255
            && base
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || byte == b'_'))
        .then(|| base.to_ascii_lowercase())
    })
}

fn resolve_image_dat_path(
    account_root: &Path,
    table_name: &str,
    create_time: i64,
    image_md5: Option<&str>,
    dat_name: Option<&str>,
) -> Option<PathBuf> {
    let table_hash = table_name.strip_prefix("Msg_")?;
    let timestamp = OffsetDateTime::from_unix_timestamp(create_time).ok()?;
    let month = format!("{:04}-{:02}", timestamp.year(), u8::from(timestamp.month()));
    let image_root = account_root
        .join("msg")
        .join("attach")
        .join(table_hash)
        .join(month)
        .join("Img");
    for identity in [dat_name, image_md5].into_iter().flatten() {
        for filename in [
            format!("{identity}_t.dat"),
            format!("{identity}.dat"),
            format!("{identity}.t.dat"),
        ] {
            let candidate = image_root.join(filename);
            let Ok(metadata) = candidate.symlink_metadata() else {
                continue;
            };
            if metadata.file_type().is_file()
                && !metadata.file_type().is_symlink()
                && metadata.len() > 0
                && metadata.len() <= MAX_IMAGE_BYTES
            {
                return Some(candidate);
            }
        }
    }
    None
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
        clean_account_directory_name, decode_value, decoded_image_attachment, decrypt_v4_image,
        derive_image_keys, image_mime_type, index_decoded_images, is_direct_conversation,
        is_message_database, message_table_name, parse_group_nicknames, read_conversation_metadata,
        read_database_images, resolve_group_sender, retention_cutoff_from, stage_decrypted_image,
        ConversationMetadata, ImageKeys, Session, TextReadContext,
    };
    use aes::Aes128;
    use cipher::{generic_array::GenericArray, BlockEncrypt, KeyInit};
    use md5::{Digest as _, Md5};
    use rusqlite::{types::Value, Connection};
    use sha2::Sha256;
    use std::{fs, path::Path};

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
    fn production_derives_verified_v4_image_keys_from_cache_metadata() {
        let fixture = tempfile::tempdir().expect("create image key fixture");
        let account = fixture.path().join("xwechat_files/wxid_example_a556");
        let template_dir = account.join("msg/attach/session/2026-08/Img");
        std::fs::create_dir_all(&template_dir).expect("create template directory");
        let kvcomm = fixture.path().join("net/kvcomm");
        std::fs::create_dir_all(&kvcomm).expect("create kvcomm directory");
        std::fs::write(kvcomm.join("key_259_session.statistic"), []).expect("write code marker");

        let digest = Md5::digest(b"259wxid_example");
        let hex = format!("{digest:x}");
        let cipher = Aes128::new_from_slice(&hex.as_bytes()[..16]).expect("fixture AES key");
        let mut block = GenericArray::clone_from_slice(&[
            0xff, 0xd8, 0xff, 0xe0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ]);
        cipher.encrypt_block(&mut block);
        let mut template = vec![
            0x07, 0x08, b'V', b'2', 0x08, 0x07, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        template.extend_from_slice(&block);
        std::fs::write(template_dir.join("sample_t.dat"), template)
            .expect("write encrypted template");

        let keys = derive_image_keys(&account).expect("derive verified keys");
        assert_eq!(keys.xor, 3);
        assert_eq!(&keys.aes, &hex.as_bytes()[..16]);
    }

    #[test]
    fn production_decrypts_v4_aes_raw_and_xor_segments() {
        let keys = ImageKeys {
            xor: 0x53,
            aes: *b"0123456789abcdef",
        };
        let aes_plain = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3, 4];
        let mut padded = [0_u8; 16];
        padded[..aes_plain.len()].copy_from_slice(&aes_plain);
        padded[aes_plain.len()..].fill(4);
        let cipher = Aes128::new_from_slice(&keys.aes).expect("fixture AES key");
        let mut block = GenericArray::clone_from_slice(&padded);
        cipher.encrypt_block(&mut block);
        let mut encrypted = vec![0x07, 0x08, b'V', b'2', 0x08, 0x07];
        encrypted.extend_from_slice(&(aes_plain.len() as i32).to_le_bytes());
        encrypted.extend_from_slice(&2_i32.to_le_bytes());
        encrypted.push(0);
        encrypted.extend_from_slice(&block);
        encrypted.extend_from_slice(&[5, 6]);
        encrypted.extend([7 ^ keys.xor, 8 ^ keys.xor]);

        let decrypted = decrypt_v4_image(&encrypted, &keys).expect("decrypt V4 fixture");
        assert_eq!(decrypted, [aes_plain.as_slice(), &[5, 6, 7, 8]].concat());
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
                "CREATE TABLE contact (username TEXT, remark TEXT, nick_name TEXT, alias TEXT, big_head_url TEXT, small_head_url TEXT);\
                 CREATE TABLE stranger (username TEXT, remark TEXT, nick_name TEXT, alias TEXT);\
                 CREATE TABLE name2id (username TEXT);\
                 CREATE TABLE chatroom_member (room_id INTEGER, member_id INTEGER);\
                 CREATE TABLE chat_room (username TEXT, ext_buffer BLOB);\
                 INSERT INTO contact VALUES ('wxid_friend', 'Friend Remark', 'Friend Nick', 'friend_alias', 'https://avatar.example/friend-big.jpg', 'https://avatar.example/friend-small.jpg');\
                 INSERT INTO contact VALUES ('room@chatroom', '', 'Group Name', 'room_alias', '', 'http://avatar.example/group-small.jpg');\
                 INSERT INTO contact VALUES ('wxid_member1', 'Member Remark', 'Member Nick', 'member_alias', 'https://avatar.example/member.jpg', '');\
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
        assert_eq!(
            metadata["wxid_friend"].avatar_url.as_deref(),
            Some("https://avatar.example/friend-big.jpg")
        );
        assert_eq!(metadata["room@chatroom"].display_name, "Group Name");
        assert_eq!(
            metadata["room@chatroom"].avatar_url.as_deref(),
            Some("https://avatar.example/group-small.jpg")
        );
        assert_eq!(metadata["room@chatroom"].member_count, Some(15));
        assert_eq!(
            metadata["room@chatroom"].participant_names["wxid_member1"],
            "Group Alias"
        );
        assert_eq!(
            metadata["room@chatroom"].participant_avatar_urls["wxid_member1"],
            "https://avatar.example/member.jpg"
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

    #[test]
    fn production_image_probe_accepts_only_decoded_image_magic() {
        assert_eq!(
            image_mime_type(&[0xff, 0xd8, 0xff, 0x00]),
            Some("image/jpeg")
        );
        assert_eq!(
            image_mime_type(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]),
            Some("image/png")
        );
        assert_eq!(image_mime_type(b"not an image"), None);
    }

    #[test]
    fn production_image_manifest_uses_the_real_thumbnail_location_and_bytes() {
        let account = tempfile::tempdir().expect("create account fixture");
        let thumb = account
            .path()
            .join("cache/1970-01/Message/unrelated-directory/Thumb/7_1_thumb.jpg");
        std::fs::create_dir_all(thumb.parent().expect("thumbnail parent"))
            .expect("create thumbnail parent");
        std::fs::write(&thumb, [0xff, 0xd8, 0xff, 0x00]).expect("write thumbnail fixture");
        let images = index_decoded_images(account.path(), 0);
        let (attachment, source) =
            decoded_image_attachment(&images, "message_0.db", "Msg_abc", 7, 1)
                .expect("build decoded image manifest");
        assert_eq!(source, thumb);
        assert_eq!(attachment.mime_type(), "image/jpeg");
        assert_eq!(attachment.size_bytes(), 4);
        assert_eq!(
            attachment.attachment_id(),
            "wechat-image:message_0.db:abc:7"
        );
    }

    #[test]
    fn production_decrypted_image_stage_returns_a_canonical_source_path() {
        let bytes = [0xff, 0xd8, 0xff, 0x00];
        let sha256 = format!("{:x}", Sha256::digest(bytes));
        let path =
            stage_decrypted_image(&bytes, &sha256, "image/jpeg").expect("stage decrypted image");

        assert_eq!(
            fs::canonicalize(&path).expect("canonical staged image"),
            path
        );
        fs::remove_file(path).expect("remove staged image fixture");
    }

    #[test]
    fn production_image_index_rejects_ambiguous_local_id_and_timestamp() {
        let account = tempfile::tempdir().expect("create account fixture");
        for directory in ["first", "second"] {
            let thumb = account.path().join(format!(
                "cache/1970-01/Message/{directory}/Thumb/7_1_thumb.jpg"
            ));
            std::fs::create_dir_all(thumb.parent().expect("thumbnail parent"))
                .expect("create thumbnail parent");
            std::fs::write(thumb, [0xff, 0xd8, 0xff, 0x00]).expect("write thumbnail fixture");
        }
        let images = index_decoded_images(account.path(), 0);
        assert!(decoded_image_attachment(&images, "message_0.db", "Msg_abc", 7, 1).is_none());
    }

    #[test]
    fn production_image_scan_is_bounded_by_matches_not_missing_source_rows() {
        let connection = Connection::open_in_memory().expect("open message fixture");
        let conversation_id = "wxid_friend";
        let table_name = message_table_name(conversation_id);
        connection
            .execute_batch(&format!(
                "CREATE TABLE Name2Id (user_name TEXT);\
                 INSERT INTO Name2Id(rowid, user_name) VALUES (1, 'wxid_local');\
                 INSERT INTO Name2Id(rowid, user_name) VALUES (2, '{conversation_id}');\
                 CREATE TABLE \"{table_name}\" (\
                    local_id INTEGER, server_id INTEGER, create_time INTEGER,\
                    real_sender_id INTEGER, status INTEGER, local_type INTEGER,\
                    message_content TEXT, compress_content BLOB, packed_info_data BLOB\
                 );"
            ))
            .expect("create message fixture");
        for local_id in 1_i64..=21 {
            connection
                .execute(
                    &format!(
                        "INSERT INTO \"{table_name}\" VALUES (?1, ?2, ?3, 2, 0, 3, '', X'', X'')"
                    ),
                    (local_id, local_id + 100, local_id + 1_000),
                )
                .expect("insert image row");
        }
        let account = tempfile::tempdir().expect("create account fixture");
        let thumb = account.path().join("21_1021_thumb.jpg");
        std::fs::write(&thumb, [0xff, 0xd8, 0xff, 0x00]).expect("write thumbnail fixture");
        let images = std::collections::BTreeMap::from([((21, 1_021), Some(thumb))]);
        let encrypted_images = std::collections::BTreeSet::new();
        let image_hardlinks = std::collections::BTreeMap::new();
        let metadata = std::collections::BTreeMap::from([(
            conversation_id.to_owned(),
            ConversationMetadata {
                display_name: "Friend".to_owned(),
                avatar_url: None,
                member_count: None,
                participant_names: std::collections::BTreeMap::new(),
                participant_avatar_urls: std::collections::BTreeMap::new(),
            },
        )]);
        let cursors = std::collections::BTreeMap::new();
        let context = TextReadContext {
            database_name: "message_0.db",
            local_username: "wxid_local",
            account_id: "account",
            conversation_metadata: &metadata,
            cursors: &cursors,
            cutoff: 0,
        };
        let batch = read_database_images(
            &connection,
            &context,
            &[Session {
                username: conversation_id.to_owned(),
            }],
            &images,
            &encrypted_images,
            &image_hardlinks,
            None,
            account.path(),
            20,
        )
        .expect("scan image rows");
        assert_eq!(batch.records.len(), 1);
        assert_eq!(batch.records[0].source_sequence, 21);
        assert_eq!(batch.cursor_updates[0].1, 21);
    }
}
