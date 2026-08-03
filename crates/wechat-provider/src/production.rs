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
const KIND_BATCH_QUOTA: usize = MAX_BATCH / 5;
const INITIAL_HISTORY_SECONDS: u64 = 60 * 24 * 60 * 60;
const DATABASE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_IMAGE_BYTES: u64 = 100 * 1024 * 1024;
const MAX_AUDIO_BYTES: u64 = 25 * 1024 * 1024;
const MAX_VIDEO_BYTES: u64 = 5 * 1024 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 6 * 1024 * 1024 * 1024;
const VOICE_SAMPLE_RATE: u32 = 24_000;
const IMAGE_CODE_CACHE: &str =
    "Library/Application Support/PersonalComputerAgent/Data/wechat-image-codes-v1";

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
    media_databases: Vec<PathBuf>,
    local_username: String,
    account_id: String,
}

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
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
        let mut media_databases = fs::read_dir(account_root.join(MESSAGE_DIRECTORY))
            .map_err(|_| source_unavailable())?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| is_media_database(path))
            .collect::<Vec<_>>();
        media_databases.sort();
        let account_id = account_id_for_root(&account_root);
        Ok(Self {
            paths: Arc::new(SourcePaths {
                account_root: account_root.clone(),
                session_database: account_root.join(SESSION_DATABASE),
                contact_database: account_root.join(CONTACT_DATABASE),
                message_databases,
                media_databases,
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
    let material = extend_message_database_routes(paths, load_material(paths)?)?;
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

#[allow(
    clippy::too_many_lines,
    reason = "one read transaction coordinates text, identity, and media cursors"
)]
fn read_message_batch(
    paths: &SourcePaths,
    cursors: &Mutex<BTreeMap<String, i64>>,
) -> Result<Vec<SourceRecord>, DomainError> {
    let material = extend_message_database_routes(paths, load_material(paths)?)?;
    let sessions = with_database(&paths.session_database, &material, read_sessions)
        .map_err(|_| read_stage_error("WECHAT_SESSION_READ_FAILED"))?;
    let conversation_metadata = with_database(&paths.contact_database, &material, |connection| {
        read_conversation_metadata(connection, &sessions)
    })
    .map_err(|_| read_stage_error("WECHAT_CONTACT_READ_FAILED"))?;
    let contact_cards = with_database(&paths.contact_database, &material, read_contact_cards)
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
    let video_hardlinks = if hardlink_path.is_file() {
        with_database(&hardlink_path, &material, read_video_hardlinks).unwrap_or_default()
    } else {
        BTreeMap::new()
    };
    let video_files = index_video_files(&paths.account_root);
    let message_files = index_message_files(&paths.account_root, cutoff);
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
            contact_cards: &contact_cards,
            cursors: &cursor_snapshot,
            cutoff,
        };
        let text_limit = remaining.min(KIND_BATCH_QUOTA);
        let text_batch = with_database(database, &material, |connection| {
            read_database_text(connection, &context, &sessions, text_limit)
        })
        .map_err(|_| read_stage_error("WECHAT_MESSAGE_READ_FAILED"))?;
        for (cursor_key, sequence, record) in text_batch {
            cursor_guard
                .entry(cursor_key)
                .and_modify(|current| *current = (*current).max(sequence))
                .or_insert(sequence);
            if let Some(record) = record {
                records.push(SourceRecord::Message(Box::new(record)));
            }
        }
        if records.len() >= MAX_BATCH {
            break;
        }
        let remaining = (MAX_BATCH - records.len()).min(KIND_BATCH_QUOTA);
        let file_batch = with_database(database, &material, |connection| {
            read_database_files(connection, &context, &sessions, &message_files, remaining)
        })
        .map_err(|_| read_stage_error("WECHAT_FILE_READ_FAILED"))?;
        for (cursor_key, sequence) in file_batch.cursor_updates {
            cursor_guard
                .entry(cursor_key)
                .and_modify(|current| *current = (*current).max(sequence))
                .or_insert(sequence);
        }
        for record in file_batch.records {
            records.push(SourceRecord::Message(Box::new(record)));
        }
        if records.len() >= MAX_BATCH {
            break;
        }
        let remaining = (MAX_BATCH - records.len()).min(KIND_BATCH_QUOTA);
        let image_batch = with_database(database, &material, |connection| {
            read_database_images(
                connection,
                &context,
                &sessions,
                &decoded_images,
                &encrypted_images,
                &image_hardlinks,
                &image_keys,
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
        let remaining = (MAX_BATCH - records.len()).min(KIND_BATCH_QUOTA);
        let audio_batch = with_database(database, &material, |connection| {
            read_database_audio(
                connection,
                &context,
                &sessions,
                &paths.media_databases,
                &material,
                remaining,
            )
        })
        .map_err(|_| read_stage_error("WECHAT_AUDIO_READ_FAILED"))?;
        for (cursor_key, sequence) in audio_batch.cursor_updates {
            cursor_guard
                .entry(cursor_key)
                .and_modify(|current| *current = (*current).max(sequence))
                .or_insert(sequence);
        }
        for record in audio_batch.records {
            records.push(SourceRecord::Message(Box::new(record)));
        }
        if records.len() >= MAX_BATCH {
            break;
        }
        let remaining = (MAX_BATCH - records.len()).min(KIND_BATCH_QUOTA);
        let video_batch = with_database(database, &material, |connection| {
            read_database_videos(
                connection,
                &context,
                &sessions,
                &video_hardlinks,
                &video_files,
                remaining,
            )
        })
        .map_err(|_| read_stage_error("WECHAT_VIDEO_READ_FAILED"))?;
        for (cursor_key, sequence) in video_batch.cursor_updates {
            cursor_guard
                .entry(cursor_key)
                .and_modify(|current| *current = (*current).max(sequence))
                .or_insert(sequence);
        }
        for record in video_batch.records {
            records.push(SourceRecord::Message(Box::new(record)));
        }
    }
    if std::env::var_os("PCA_WECHAT_MEDIA_DIAGNOSTIC").is_some() {
        eprintln!(
            "WeChat media scan: media_dbs={} decoded={} encrypted={} hardlinks={} keys={} rows={} cache_matches={} metadata={} direct_dat={} indexed_dat={} hardlink_dat={} records={}",
            paths.media_databases.len(),
            decoded_images
                .values()
                .filter(|path| path.is_some())
                .count(),
            encrypted_images.len(),
            image_hardlinks.len(),
            image_keys.len(),
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
    contact_cards: &'a BTreeMap<String, ContactCardProfile>,
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
) -> Result<Vec<(String, i64, Option<SourceMessageRecord>)>, SqlcipherProbeFailure> {
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
        let cursor_key = format!(
            "{}:{}:display-text-v2",
            context.database_name, session.username
        );
        let after = context.cursors.get(&cursor_key).copied().unwrap_or(0);
        let per_conversation = MAX_PER_CONVERSATION.min(limit - records.len());
        let sql = format!(
            "SELECT local_id, server_id, create_time, real_sender_id, status, local_type, message_content, compress_content \
             FROM \"{table_name}\" \
             WHERE local_type IN (1, 42, 48, 49, 50, 8589934592049, 8594229559345) \
               AND local_id > ?1 AND create_time >= ?2 \
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
                        local_type: row.get(5)?,
                        message_content: row.get(6)?,
                        compress_content: row.get(7)?,
                    })
                },
            )
            .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
        for row in rows {
            let row = row.map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
            if row.local_id <= 0 {
                continue;
            }
            if row.create_time <= 0 {
                records.push((cursor_key.clone(), row.local_id, None));
                continue;
            }
            let direction = if row.real_sender_id == my_rowid {
                if row.server_id <= 0 || row.status < 0 {
                    records.push((cursor_key.clone(), row.local_id, None));
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
                records.push((cursor_key.clone(), row.local_id, None));
                continue;
            };
            let Some(body) =
                display_text_message_with_contacts(row.local_type, &body, context.contact_cards)
            else {
                records.push((cursor_key.clone(), row.local_id, None));
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
                Some(SourceMessageRecord {
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
                }),
            ));
        }
    }
    Ok(records)
}

#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    reason = "image rows use the same fail-closed identity mapping as text rows"
)]
fn read_database_images(
    connection: &Connection,
    context: &TextReadContext<'_>,
    sessions: &[Session],
    decoded_images: &BTreeMap<(i64, i64), Option<PathBuf>>,
    encrypted_images: &BTreeSet<String>,
    image_hardlinks: &BTreeMap<String, PathBuf>,
    image_keys: &[ImageKeys],
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
            let decoded_content =
                decode_message_content(&row.compress_content, &row.message_content);
            let image_md5 = decoded_content.as_deref().and_then(parse_image_md5);
            let dat_name = parse_image_dat_name(&row.packed_info_data);
            if image_md5.is_some() || dat_name.is_some() {
                metadata_matches += 1;
            }
            let direct_dat_path = resolve_image_dat_path(
                account_root,
                &table_name,
                row.create_time,
                image_md5.as_deref(),
                dat_name.as_deref(),
            );
            if direct_dat_path.is_some() {
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
                image_md5
                    .as_ref()
                    .and_then(|md5| image_hardlinks.get(md5))
                    .and_then(|source| {
                        decrypted_image_attachment(
                            source,
                            image_keys,
                            context.database_name,
                            &table_name,
                            row.local_id,
                        )
                    })
            })
            .or_else(|| {
                decrypted_image_attachment(
                    direct_dat_path.as_ref()?,
                    image_keys,
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

struct AudioReadBatch {
    records: Vec<SourceMessageRecord>,
    cursor_updates: Vec<(String, i64)>,
}

struct VideoReadBatch {
    records: Vec<SourceMessageRecord>,
    cursor_updates: Vec<(String, i64)>,
}

struct FileReadBatch {
    records: Vec<SourceMessageRecord>,
    cursor_updates: Vec<(String, i64)>,
}

#[allow(
    clippy::too_many_lines,
    reason = "file rows share the reviewed message identity and scope mapping"
)]
fn read_database_files(
    connection: &Connection,
    read_context: &TextReadContext<'_>,
    sessions: &[Session],
    message_files: &BTreeMap<String, Vec<PathBuf>>,
    limit: usize,
) -> Result<FileReadBatch, SqlcipherProbeFailure> {
    let my_rowid = local_sender_rowid(connection, read_context.local_username)?
        .ok_or(SqlcipherProbeFailure::AccountUnverified)?;
    let mut sender_statement = connection
        .prepare("SELECT user_name FROM Name2Id WHERE rowid = ?1 LIMIT 1")
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
    let mut records = Vec::new();
    let mut cursor_updates = Vec::new();
    for session in sessions {
        if records.len() >= limit {
            break;
        }
        let metadata = read_context
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
        let cursor_key = format!("{}:{}:file", read_context.database_name, session.username);
        let after = read_context.cursors.get(&cursor_key).copied().unwrap_or(0);
        let per_conversation = MAX_PER_CONVERSATION.min(limit - records.len());
        let sql = format!(
            "SELECT local_id, server_id, create_time, real_sender_id, status, message_content, compress_content \
             FROM \"{table_name}\" \
             WHERE local_type = 49 AND local_id > ?1 AND create_time >= ?2 \
             ORDER BY local_id ASC"
        );
        let mut statement = connection
            .prepare(&sql)
            .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
        let rows = statement
            .query_map((after, read_context.cutoff), |row| {
                Ok(MessageRow {
                    local_id: row.get(0)?,
                    server_id: row.get(1)?,
                    create_time: row.get(2)?,
                    real_sender_id: row.get(3)?,
                    status: row.get(4)?,
                    local_type: 49,
                    message_content: row.get(5)?,
                    compress_content: row.get(6)?,
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
            if row.local_id <= 0 || row.create_time <= 0 {
                continue;
            }
            let Some(content) = decode_message_content(&row.compress_content, &row.message_content)
            else {
                scanned_through = row.local_id;
                continue;
            };
            if app_message_type(&content) != Some(6) {
                scanned_through = row.local_id;
                continue;
            }
            let Some(file_name) = xml_text(&content, "title") else {
                scanned_through = row.local_id;
                continue;
            };
            let expected_size = xml_text(&content, "totallen")
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|value| *value > 0);
            let completed_file = resolve_message_file(message_files, &file_name, expected_size)
                .and_then(|source| {
                    file_attachment(
                        source,
                        read_context.database_name,
                        &table_name,
                        row.local_id,
                    )
                });
            let Some((attachment, source_path)) = completed_file else {
                scanned_through = row.local_id;
                continue;
            };
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
                SourceDirection::Outgoing => (
                    read_context.local_username.to_owned(),
                    "You".to_owned(),
                    None,
                ),
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
                    let avatar = metadata.participant_avatar_urls.get(&sender_id).cloned();
                    (sender_id, sender_display_name, avatar)
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
            let attachment_id = attachment.attachment_id().to_owned();
            records.push(SourceMessageRecord {
                account_id: read_context.account_id.to_owned(),
                source_sequence: u64::try_from(row.local_id)
                    .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?,
                message_id: if row.server_id > 0 {
                    row.server_id.to_string()
                } else {
                    format!("local-{}", row.local_id)
                },
                conversation_id: session.username.clone(),
                conversation_display_name: metadata.display_name.clone(),
                conversation_avatar_url: metadata.avatar_url.clone(),
                sender_id,
                sender_display_name,
                sender_avatar_url,
                source_key: format!(
                    "wechat:{}:{table_name}:{}:file",
                    read_context.database_name, row.local_id
                ),
                occurred_at,
                local_account: LocalAccountProof::Verified,
                direction,
                kind: SourceMessageKind::File,
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
    Ok(FileReadBatch {
        records,
        cursor_updates,
    })
}

#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    reason = "video rows share the reviewed message identity and scope mapping"
)]
fn read_database_videos(
    connection: &Connection,
    context: &TextReadContext<'_>,
    sessions: &[Session],
    video_hardlinks: &BTreeMap<String, String>,
    video_files: &BTreeMap<String, PathBuf>,
    limit: usize,
) -> Result<VideoReadBatch, SqlcipherProbeFailure> {
    let my_rowid = local_sender_rowid(connection, context.local_username)?
        .ok_or(SqlcipherProbeFailure::AccountUnverified)?;
    let mut sender_statement = connection
        .prepare("SELECT user_name FROM Name2Id WHERE rowid = ?1 LIMIT 1")
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
    let mut records = Vec::new();
    let mut cursor_updates = Vec::new();
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
        let cursor_key = format!("{}:{}:video", context.database_name, session.username);
        let after = context.cursors.get(&cursor_key).copied().unwrap_or(0);
        let per_conversation = MAX_PER_CONVERSATION.min(limit - records.len());
        let sql = format!(
            "SELECT local_id, server_id, create_time, real_sender_id, status, \
                    message_content, compress_content, packed_info_data \
             FROM \"{table_name}\" \
             WHERE local_type = 43 AND local_id > ?1 AND create_time >= ?2 \
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
            if row.local_id <= 0 || row.create_time <= 0 {
                continue;
            }
            let decoded_content =
                decode_message_content(&row.compress_content, &row.message_content);
            let completed_video = resolve_video_path(
                decoded_content.as_deref(),
                &row.packed_info_data,
                video_hardlinks,
                video_files,
            )
            .and_then(|source| {
                video_attachment(source, context.database_name, &table_name, row.local_id)
            });
            let Some((attachment, source_path)) = completed_video else {
                if row.create_time > media_ready_cutoff {
                    break;
                }
                scanned_through = row.local_id;
                continue;
            };
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
                    let avatar = metadata.participant_avatar_urls.get(&sender_id).cloned();
                    (sender_id, sender_display_name, avatar)
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
            let attachment_id = attachment.attachment_id().to_owned();
            records.push(SourceMessageRecord {
                account_id: context.account_id.to_owned(),
                source_sequence: u64::try_from(row.local_id)
                    .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?,
                message_id: if row.server_id > 0 {
                    row.server_id.to_string()
                } else {
                    format!("local-{}", row.local_id)
                },
                conversation_id: session.username.clone(),
                conversation_display_name: metadata.display_name.clone(),
                conversation_avatar_url: metadata.avatar_url.clone(),
                sender_id,
                sender_display_name,
                sender_avatar_url,
                source_key: format!(
                    "wechat:{}:{table_name}:{}:video",
                    context.database_name, row.local_id
                ),
                occurred_at,
                local_account: LocalAccountProof::Verified,
                direction,
                kind: SourceMessageKind::Video,
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
    Ok(VideoReadBatch {
        records,
        cursor_updates,
    })
}

#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    reason = "voice rows share the reviewed message identity and scope mapping"
)]
fn read_database_audio(
    connection: &Connection,
    context: &TextReadContext<'_>,
    sessions: &[Session],
    media_databases: &[PathBuf],
    material: &WechatKeyMaterial,
    limit: usize,
) -> Result<AudioReadBatch, SqlcipherProbeFailure> {
    let my_rowid = local_sender_rowid(connection, context.local_username)?
        .ok_or(SqlcipherProbeFailure::AccountUnverified)?;
    let mut sender_statement = connection
        .prepare("SELECT user_name FROM Name2Id WHERE rowid = ?1 LIMIT 1")
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
    let mut records = Vec::new();
    let mut cursor_updates = Vec::new();
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
        let cursor_key = format!("{}:{}:audio", context.database_name, session.username);
        let after = context.cursors.get(&cursor_key).copied().unwrap_or(0);
        let per_conversation = MAX_PER_CONVERSATION.min(limit - records.len());
        let sql = format!(
            "SELECT local_id, server_id, create_time, real_sender_id, status FROM \"{table_name}\" \
             WHERE local_type = 34 AND local_id > ?1 AND create_time >= ?2 \
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
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
        let mut scanned_through = after;
        for row in rows {
            let (local_id, server_id, create_time, real_sender_id, status) =
                row.map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
            if local_id <= 0 || create_time <= 0 {
                continue;
            }
            scanned_through = local_id;
            let same_time_index =
                voice_same_time_index(connection, &table_name, create_time, local_id)?;
            let silk = media_databases.iter().find_map(|database| {
                with_recovered_database(database, material, DATABASE_TIMEOUT, |media| {
                    read_voice_blob(
                        media,
                        &session.username,
                        server_id,
                        create_time,
                        same_time_index,
                    )
                })
                .ok()
                .flatten()
            });
            let Some((attachment, source_path)) = silk.and_then(|silk| {
                decode_voice_attachment(silk, context.database_name, &table_name, local_id)
            }) else {
                continue;
            };
            let direction = if real_sender_id == my_rowid {
                if server_id <= 0 || status < 0 {
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
                        .query_row([real_sender_id], |row| row.get::<_, String>(0))
                        .optional()
                        .ok()
                        .flatten()
                        .filter(|value| valid_identity(value));
                    let (sender_id, sender_display_name) = resolve_group_sender(
                        sender_id,
                        real_sender_id,
                        &metadata.participant_names,
                    );
                    let avatar = metadata.participant_avatar_urls.get(&sender_id).cloned();
                    (sender_id, sender_display_name, avatar)
                }
                SourceDirection::Incoming => (
                    session.username.clone(),
                    metadata.display_name.clone(),
                    metadata.avatar_url.clone(),
                ),
                SourceDirection::Unknown => continue,
            };
            let occurred_at = OffsetDateTime::from_unix_timestamp(create_time)
                .ok()
                .and_then(|time| time.format(&Rfc3339).ok())
                .ok_or(SqlcipherProbeFailure::UnsupportedSchema)?;
            let attachment_id = attachment.attachment_id().to_owned();
            records.push(SourceMessageRecord {
                account_id: context.account_id.to_owned(),
                source_sequence: u64::try_from(local_id)
                    .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?,
                message_id: if server_id > 0 {
                    server_id.to_string()
                } else {
                    format!("local-{local_id}")
                },
                conversation_id: session.username.clone(),
                conversation_display_name: metadata.display_name.clone(),
                conversation_avatar_url: metadata.avatar_url.clone(),
                sender_id,
                sender_display_name,
                sender_avatar_url,
                source_key: format!(
                    "wechat:{}:{table_name}:{local_id}:audio",
                    context.database_name
                ),
                occurred_at,
                local_account: LocalAccountProof::Verified,
                direction,
                kind: SourceMessageKind::Audio,
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
    Ok(AudioReadBatch {
        records,
        cursor_updates,
    })
}

#[allow(
    clippy::too_many_lines,
    reason = "voice lookup keeps the ordered exact, conversation-time, and time-only fallbacks together"
)]
fn read_voice_blob(
    connection: &Connection,
    conversation_id: &str,
    server_id: i64,
    create_time: i64,
    same_time_index: usize,
) -> Result<Option<Vec<u8>>, SqlcipherProbeFailure> {
    let mut statement = connection
        .prepare(
            "SELECT name FROM sqlite_master \
             WHERE type='table' AND name LIKE 'VoiceInfo%' ORDER BY name",
        )
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
    let tables = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
    let name_table = connection
        .query_row(
            "SELECT name FROM sqlite_master \
             WHERE type='table' AND name LIKE 'Name2Id%' ORDER BY name LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?
        .filter(|name| valid_sql_identifier(name));
    let chat_name_id = if let Some(name_table) = &name_table {
        let sql = format!("SELECT rowid FROM \"{name_table}\" WHERE user_name=?1 LIMIT 1");
        connection
            .query_row(&sql, [conversation_id], |row| row.get::<_, i64>(0))
            .optional()
            .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?
    } else {
        None
    };

    for table in tables
        .into_iter()
        .filter(|table| valid_sql_identifier(table))
    {
        let columns = table_columns(connection, &table)?;
        let pick = |names: &[&str]| {
            names.iter().find_map(|name| {
                columns
                    .iter()
                    .find(|column| column.eq_ignore_ascii_case(name))
                    .filter(|column| valid_sql_identifier(column))
                    .cloned()
            })
        };
        let Some(data) = pick(&["voice_data", "buf", "voicebuf", "data"]) else {
            continue;
        };
        let chat = pick(&["chat_name_id", "chatnameid", "chat_nameid"]);
        let server = pick(&[
            "msg_svr_id",
            "msgsvrid",
            "svr_id",
            "svrid",
            "server_id",
            "serverid",
        ]);
        let time = pick(&["create_time", "createtime", "time"]);

        if server_id > 0 {
            if let (Some(chat), Some(chat_name_id), Some(server)) =
                (chat.as_ref(), chat_name_id, server.as_ref())
            {
                let sql = format!(
                    "SELECT \"{data}\" FROM \"{table}\" \
                     WHERE \"{chat}\"=?1 AND \"{server}\"=?2 LIMIT 1"
                );
                if let Some(blob) =
                    query_voice_value(connection, &sql, rusqlite::params![chat_name_id, server_id])?
                {
                    return Ok(Some(blob));
                }
            }
            if let Some(server) = server.as_ref() {
                let sql =
                    format!("SELECT \"{data}\" FROM \"{table}\" WHERE \"{server}\"=?1 LIMIT 1");
                if let Some(blob) =
                    query_voice_value(connection, &sql, rusqlite::params![server_id])?
                {
                    return Ok(Some(blob));
                }
            }
        }

        if let (Some(chat), Some(chat_name_id), Some(time)) =
            (chat.as_ref(), chat_name_id, time.as_ref())
        {
            let sql = format!(
                "SELECT \"{data}\" FROM \"{table}\" \
                 WHERE \"{chat}\"=?1 AND \"{time}\"=?2 ORDER BY rowid LIMIT 100"
            );
            let blobs = query_voice_values(
                connection,
                &sql,
                rusqlite::params![chat_name_id, create_time],
            )?;
            if let Some(blob) = blobs.get(same_time_index.min(blobs.len().saturating_sub(1))) {
                return Ok(Some(blob.clone()));
            }
        }

        if let Some(time) = time.as_ref() {
            let sql = format!(
                "SELECT \"{data}\" FROM \"{table}\" \
                 WHERE \"{time}\"=?1 ORDER BY rowid LIMIT 100"
            );
            let blobs = query_voice_values(connection, &sql, rusqlite::params![create_time])?;
            if let Some(blob) = blobs.get(same_time_index.min(blobs.len().saturating_sub(1))) {
                return Ok(Some(blob.clone()));
            }
        }
    }
    Ok(None)
}

fn voice_same_time_index(
    connection: &Connection,
    table: &str,
    create_time: i64,
    local_id: i64,
) -> Result<usize, SqlcipherProbeFailure> {
    let sql = format!(
        "SELECT COUNT(*) FROM \"{table}\" \
         WHERE local_type=34 AND create_time=?1 AND local_id<?2"
    );
    let count = connection
        .query_row(&sql, (create_time, local_id), |row| row.get::<_, i64>(0))
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
    usize::try_from(count).map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)
}

fn resolve_video_path<'a>(
    content: Option<&str>,
    packed_info: &Value,
    hardlinks: &BTreeMap<String, String>,
    files: &'a BTreeMap<String, PathBuf>,
) -> Option<&'a PathBuf> {
    let mut candidates = Vec::new();
    if let Some(key) = parse_video_file_key(packed_info) {
        candidates.push(key);
    }
    let md5s = content.map(parse_video_md5_candidates).unwrap_or_default();
    for md5 in &md5s {
        if let Some(key) = hardlinks.get(md5) {
            candidates.push(key.clone());
            if let Some(base) = key.strip_suffix("_raw") {
                candidates.push(base.to_owned());
            }
        }
        candidates.push(md5.clone());
    }
    for candidate in candidates {
        if let Some(path) = files.get(&candidate) {
            return Some(path);
        }
    }
    let size = content.and_then(parse_video_length)?;
    files.get(&format!("size:{size}"))
}

fn parse_video_md5_candidates(content: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    for name in ["newmd5", "md5", "rawmd5", "originsourcemd5"] {
        if let Some(md5) = parse_named_md5(content, name) {
            if !candidates.contains(&md5) {
                candidates.push(md5);
            }
        }
    }
    candidates
}

fn parse_named_md5(content: &str, name: &str) -> Option<String> {
    let lowercase = content.to_ascii_lowercase();
    let mut search_from = 0;
    while let Some(relative) = lowercase[search_from..].find(name) {
        let start = search_from + relative + name.len();
        let Some(value) = content[start..].trim_start().strip_prefix('=') else {
            search_from = start;
            continue;
        };
        let value = value.trim_start();
        let candidate = match value.as_bytes().first().copied() {
            Some(b'\'') => value[1..].split('\'').next(),
            Some(b'"') => value[1..].split('"').next(),
            _ => value
                .split(|character: char| character.is_whitespace() || character == '>')
                .next(),
        };
        if let Some(md5) = candidate.and_then(valid_md5) {
            return Some(md5);
        }
        search_from = start;
    }
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = lowercase.find(&open)? + open.len();
    let end = lowercase[start..].find(&close)?;
    valid_md5(&content[start..start + end])
}

fn parse_video_length(content: &str) -> Option<u64> {
    let lowercase = content.to_ascii_lowercase();
    let video = lowercase.find("<videomsg")?;
    let end = lowercase[video..].find('>')? + video;
    let tag = &content[video..=end];
    let lower_tag = &lowercase[video..=end];
    let start = lower_tag.find("length")? + "length".len();
    let value = tag[start..].trim_start().strip_prefix('=')?.trim_start();
    let value = match value.as_bytes().first().copied() {
        Some(b'\'') => value[1..].split('\'').next()?,
        Some(b'"') => value[1..].split('"').next()?,
        _ => value
            .split(|character: char| character.is_whitespace() || character == '>')
            .next()?,
    };
    value.parse::<u64>().ok().filter(|size| *size > 0)
}

fn parse_video_file_key(value: &Value) -> Option<String> {
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
    printable
        .split_ascii_whitespace()
        .find(|token| token.to_ascii_lowercase().contains(".mp4"))
        .and_then(video_file_key)
}

fn video_file_key(value: &str) -> Option<String> {
    let filename = value.trim().rsplit(['/', '\\']).next()?;
    let stem = filename.rsplit_once('.').map_or(filename, |(stem, _)| stem);
    let normalized = stem.to_ascii_lowercase();
    (normalized.len() >= 8
        && normalized.len() <= 128
        && normalized
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'))
    .then_some(normalized)
}

fn video_attachment(
    source: &Path,
    database_name: &str,
    table_name: &str,
    local_id: i64,
) -> Option<(CommunicationAttachment, PathBuf)> {
    let source = fs::canonicalize(source).ok()?;
    let metadata = source.symlink_metadata().ok()?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() == 0
        || metadata.len() > MAX_VIDEO_BYTES
    {
        return None;
    }
    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&source)
        .ok()?;
    let mut header = [0_u8; 12];
    input.read_exact(&mut header).ok()?;
    if &header[4..8] != b"ftyp" {
        return None;
    }
    let mut hasher = Sha256::new();
    hasher.update(header);
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut bytes_read = u64::try_from(header.len()).ok()?;
    loop {
        let count = input.read(&mut buffer).ok()?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        bytes_read = bytes_read.checked_add(u64::try_from(count).ok()?)?;
    }
    if bytes_read != metadata.len() {
        return None;
    }
    let table_hash = table_name.strip_prefix("Msg_")?;
    let attachment_id = format!("wechat-video:{database_name}:{table_hash}:{local_id}");
    let attachment = CommunicationAttachment::try_new(
        attachment_id,
        MessageKind::Video,
        format!("{:x}", hasher.finalize()),
        metadata.len(),
        "video/mp4".to_owned(),
    )
    .ok()?;
    Some((attachment, source))
}

fn file_attachment(
    source: &Path,
    database_name: &str,
    table_name: &str,
    local_id: i64,
) -> Option<(CommunicationAttachment, PathBuf)> {
    let source = fs::canonicalize(source).ok()?;
    let metadata = source.symlink_metadata().ok()?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() == 0
        || metadata.len() > MAX_FILE_BYTES
    {
        return None;
    }
    let file_name = source.file_name()?.to_str()?.to_owned();
    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&source)
        .ok()?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut bytes_read = 0_u64;
    loop {
        let count = input.read(&mut buffer).ok()?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        bytes_read = bytes_read.checked_add(u64::try_from(count).ok()?)?;
    }
    if bytes_read != metadata.len() {
        return None;
    }
    let table_hash = table_name.strip_prefix("Msg_")?;
    let attachment_id = format!("wechat-file:{database_name}:{table_hash}:{local_id}");
    let attachment = CommunicationAttachment::try_new(
        attachment_id,
        MessageKind::File,
        format!("{:x}", hasher.finalize()),
        metadata.len(),
        file_mime_type(&source).to_owned(),
    )
    .and_then(|attachment| attachment.with_file_name(&file_name))
    .ok()?;
    Some((attachment, source))
}

fn file_mime_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("pdf") => "application/pdf",
        Some("doc") => "application/msword",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Some("xls") => "application/vnd.ms-excel",
        Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        Some("ppt") => "application/vnd.ms-powerpoint",
        Some("pptx") => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        Some("txt") => "text/plain",
        Some("csv") => "text/csv",
        Some("zip") => "application/zip",
        Some("wav") => "audio/wav",
        Some("mp3") => "audio/mpeg",
        _ => "application/octet-stream",
    }
}

fn query_voice_value(
    connection: &Connection,
    sql: &str,
    parameters: impl rusqlite::Params,
) -> Result<Option<Vec<u8>>, SqlcipherProbeFailure> {
    let value = connection
        .query_row(sql, parameters, |row| row.get::<_, Value>(0))
        .optional()
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
    Ok(value.and_then(voice_value_bytes))
}

fn query_voice_values(
    connection: &Connection,
    sql: &str,
    parameters: impl rusqlite::Params,
) -> Result<Vec<Vec<u8>>, SqlcipherProbeFailure> {
    let mut statement = connection
        .prepare(sql)
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
    let values = statement
        .query_map(parameters, |row| row.get::<_, Value>(0))
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
    Ok(values.into_iter().filter_map(voice_value_bytes).collect())
}

fn voice_value_bytes(value: Value) -> Option<Vec<u8>> {
    match value {
        Value::Blob(bytes) if !bytes.is_empty() => Some(bytes),
        Value::Text(text) => decode_hex(&text),
        _ => None,
    }
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    let value = value.trim();
    if value.is_empty() || !value.len().is_multiple_of(2) {
        return None;
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16)?;
            let low = (pair[1] as char).to_digit(16)?;
            u8::try_from((high << 4) | low).ok()
        })
        .collect()
}

fn valid_sql_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn derive_image_keys(account_root: &Path) -> Vec<ImageKeys> {
    let Some(raw_account) = account_root
        .file_name()
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned)
    else {
        return Vec::new();
    };
    let mut account_candidates = BTreeSet::from([raw_account.clone()]);
    if let Some(cleaned) = clean_account_directory_name(&raw_account) {
        account_candidates.insert(cleaned);
    }
    let codes = collect_kvcomm_codes(account_root);
    let templates = collect_v4_templates(&account_root.join("msg").join("attach"), 32)
        .into_iter()
        .filter_map(|template| {
            fs::read(template)
                .ok()
                .and_then(|bytes| bytes.get(15..31).and_then(|block| block.try_into().ok()))
        })
        .collect::<Vec<[u8; 16]>>();
    let mut verified = BTreeSet::new();
    let mut fallback = BTreeSet::new();
    for account in &account_candidates {
        for code in &codes {
            let digest = Md5::digest(format!("{code}{account}").as_bytes());
            let hex = format!("{digest:x}");
            let Some(aes) = hex
                .as_bytes()
                .get(..16)
                .and_then(|value| value.try_into().ok())
            else {
                continue;
            };
            let keys = ImageKeys {
                xor: (*code & 0xff) as u8,
                aes,
            };
            if templates.iter().any(|ciphertext| {
                decrypt_aes_block(ciphertext, &keys.aes)
                    .is_some_and(|plain| image_or_wxgf_magic(&plain))
            }) {
                verified.insert(keys);
            } else {
                fallback.insert(keys);
            }
        }
    }
    verified.extend(fallback);
    verified.into_iter().collect()
}

fn collect_kvcomm_codes(account_root: &Path) -> BTreeSet<u32> {
    let mut directories = BTreeSet::new();
    if let Ok(home) = env::var("HOME") {
        let container = PathBuf::from(home).join("Library/Containers/com.tencent.xinWeChat/Data");
        directories.insert(container.join("Documents/app_data/net/kvcomm"));
        directories.insert(container.join("Documents/app_data/ilink/kvcomm"));
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
    let mut codes = load_persisted_image_codes();
    for directory in directories {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Some(filename) = entry.file_name().to_str().map(ToOwned::to_owned) else {
                continue;
            };
            codes.extend(kvcomm_codes_from_filename(&filename));
        }
    }
    persist_image_codes(&codes);
    codes
}

fn load_persisted_image_codes() -> BTreeSet<u32> {
    let Some(path) = env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(IMAGE_CODE_CACHE))
    else {
        return BTreeSet::new();
    };
    let Ok(metadata) = path.symlink_metadata() else {
        return BTreeSet::new();
    };
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.len() > 4096
    {
        return BTreeSet::new();
    }
    fs::read_to_string(path)
        .ok()
        .map(|content| {
            content
                .lines()
                .filter_map(|line| line.parse::<u32>().ok())
                .collect()
        })
        .unwrap_or_default()
}

fn persist_image_codes(codes: &BTreeSet<u32>) {
    if codes.is_empty() {
        return;
    }
    let Some(path) = env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(IMAGE_CODE_CACHE))
    else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    let temporary = parent.join(format!(".wechat-image-codes-v1.{}.tmp", std::process::id()));
    let content = codes
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let result = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .and_then(|mut output| {
            output.write_all(content.as_bytes())?;
            output.sync_all()
        })
        .and_then(|()| fs::rename(&temporary, &path));
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
}

fn kvcomm_codes_from_filename(filename: &str) -> Vec<u32> {
    let Some(rest) = filename
        .strip_prefix("key_")
        .and_then(|rest| rest.strip_suffix(".statistic"))
    else {
        return Vec::new();
    };
    rest.split('_')
        .filter_map(|component| component.parse::<u32>().ok())
        .collect()
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
    keys: &[ImageKeys],
    database_name: &str,
    table_name: &str,
    local_id: i64,
) -> Option<(CommunicationAttachment, PathBuf)> {
    let encrypted = fs::read(encrypted_path).ok()?;
    let decrypted = keys
        .iter()
        .find_map(|candidate| decrypt_v4_image(&encrypted, candidate))?;
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
    stage_media(bytes, sha256, mime_type)
}

fn stage_media(bytes: &[u8], sha256: &str, mime_type: &str) -> Option<PathBuf> {
    let root = fs::canonicalize(env::temp_dir())
        .ok()?
        .join("pca-wechat-media");
    match root.symlink_metadata() {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(&root).ok()?;
        }
        Ok(_) | Err(_) => return None,
    }
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).ok()?;
    cleanup_staged_images(&root);
    let extension = match mime_type {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "audio/wav" => "wav",
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

fn decode_voice_attachment(
    silk: Vec<u8>,
    database_name: &str,
    table_name: &str,
    local_id: i64,
) -> Option<(CommunicationAttachment, PathBuf)> {
    if silk.is_empty() || silk.len() > usize::try_from(MAX_AUDIO_BYTES).ok()? {
        return None;
    }
    let pcm = silk_rs::decode_silk(silk, i32::try_from(VOICE_SAMPLE_RATE).ok()?).ok()?;
    let wav = pcm_to_wav(&pcm, VOICE_SAMPLE_RATE)?;
    let sha256 = format!("{:x}", Sha256::digest(&wav));
    let source_path = stage_media(&wav, &sha256, "audio/wav")?;
    let table_hash = table_name.strip_prefix("Msg_")?;
    let attachment_id = format!("wechat-audio:{database_name}:{table_hash}:{local_id}");
    let attachment = CommunicationAttachment::try_new(
        attachment_id,
        MessageKind::Audio,
        sha256,
        u64::try_from(wav.len()).ok()?,
        "audio/wav".to_owned(),
    )
    .ok()?;
    Some((attachment, source_path))
}

fn pcm_to_wav(pcm: &[u8], sample_rate: u32) -> Option<Vec<u8>> {
    if pcm.is_empty() || !pcm.len().is_multiple_of(2) {
        return None;
    }
    let data_size = u32::try_from(pcm.len()).ok()?;
    let riff_size = 36_u32.checked_add(data_size)?;
    let byte_rate = sample_rate.checked_mul(2)?;
    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&riff_size.to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&2_u16.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_size.to_le_bytes());
    wav.extend_from_slice(pcm);
    Some(wav)
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
                .is_some_and(|age| age > Duration::from_hours(1))
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

fn read_video_hardlinks(
    connection: &Connection,
) -> Result<BTreeMap<String, String>, SqlcipherProbeFailure> {
    let table = connection
        .query_row(
            "SELECT name FROM sqlite_master \
             WHERE type='table' AND name LIKE 'video_hardlink_info%' \
             ORDER BY name DESC LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?
        .filter(|name| valid_sql_identifier(name))
        .ok_or(SqlcipherProbeFailure::UnsupportedSchema)?;
    require_columns(connection, &table, &["md5", "file_name"])?;
    let sql = format!(
        "SELECT lower(md5), file_name FROM \"{table}\" \
         WHERE length(md5)=32 ORDER BY rowid DESC LIMIT 100000"
    );
    let mut statement = connection
        .prepare(&sql)
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
    let mut hardlinks = BTreeMap::new();
    for row in rows {
        let (md5, file_name) = row.map_err(|_| SqlcipherProbeFailure::UnsupportedSchema)?;
        let Some(md5) = valid_md5(&md5) else {
            continue;
        };
        let Some(file_key) = video_file_key(&file_name) else {
            continue;
        };
        hardlinks.entry(md5).or_insert(file_key);
    }
    Ok(hardlinks)
}

fn index_video_files(account_root: &Path) -> BTreeMap<String, PathBuf> {
    let root = account_root.join("msg/video");
    let Ok(months) = fs::read_dir(&root) else {
        return BTreeMap::new();
    };
    let mut paths = BTreeMap::new();
    let mut raw_aliases = Vec::new();
    let mut sizes = BTreeMap::<u64, Option<PathBuf>>::new();
    for month in months.flatten().take(120) {
        let month_path = month.path();
        let Ok(metadata) = month_path.symlink_metadata() else {
            continue;
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        let Ok(entries) = fs::read_dir(month_path) else {
            continue;
        };
        for entry in entries.flatten().take(10_000) {
            let path = entry.path();
            let Ok(metadata) = path.symlink_metadata() else {
                continue;
            };
            if !metadata.file_type().is_file()
                || metadata.file_type().is_symlink()
                || metadata.len() == 0
                || metadata.len() > MAX_VIDEO_BYTES
                || path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_none_or(|extension| !extension.eq_ignore_ascii_case("mp4"))
            {
                continue;
            }
            let Some(key) = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(video_file_key)
            else {
                continue;
            };
            paths.entry(key.clone()).or_insert_with(|| path.clone());
            if let Some(base) = key.strip_suffix("_raw") {
                raw_aliases.push((base.to_owned(), path.clone()));
            }
            sizes
                .entry(metadata.len())
                .and_modify(|candidate| *candidate = None)
                .or_insert_with(|| Some(path));
        }
    }
    for (base, path) in raw_aliases {
        paths.entry(base).or_insert(path);
    }
    for (size, path) in sizes {
        if let Some(path) = path {
            paths.insert(format!("size:{size}"), path);
        }
    }
    paths
}

fn index_message_files(account_root: &Path, _cutoff: i64) -> BTreeMap<String, Vec<PathBuf>> {
    let root = account_root.join("msg/file");
    let Ok(months) = fs::read_dir(root) else {
        return BTreeMap::new();
    };
    let mut files = BTreeMap::<String, Vec<PathBuf>>::new();
    for month in months.flatten().take(120) {
        let month_path = month.path();
        let Ok(metadata) = month_path.symlink_metadata() else {
            continue;
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        let Ok(entries) = fs::read_dir(month_path) else {
            continue;
        };
        for entry in entries.flatten().take(10_000) {
            let path = entry.path();
            let Ok(metadata) = path.symlink_metadata() else {
                continue;
            };
            if !metadata.file_type().is_file()
                || metadata.file_type().is_symlink()
                || metadata.len() == 0
                || metadata.len() > MAX_FILE_BYTES
            {
                continue;
            }
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            files.entry(file_name.to_owned()).or_default().push(path);
        }
    }
    files
}

fn resolve_message_file<'a>(
    files: &'a BTreeMap<String, Vec<PathBuf>>,
    file_name: &str,
    expected_size: Option<u64>,
) -> Option<&'a Path> {
    let candidates = files.get(file_name)?;
    candidates
        .iter()
        .find(|path| {
            expected_size.is_none_or(|expected| {
                path.symlink_metadata()
                    .is_ok_and(|metadata| metadata.len() == expected)
            })
        })
        .map(PathBuf::as_path)
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

#[derive(Clone)]
struct ContactCardProfile {
    display_name: String,
    wechat_id: String,
    avatar_url: Option<String>,
}

struct MessageRow {
    local_id: i64,
    server_id: i64,
    create_time: i64,
    real_sender_id: i64,
    status: i64,
    local_type: i64,
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

fn read_contact_cards(
    connection: &Connection,
) -> Result<BTreeMap<String, ContactCardProfile>, SqlcipherProbeFailure> {
    let avatars = read_avatar_urls(connection, "contact").unwrap_or_default();
    let mut statement = connection
        .prepare("SELECT username, remark, nick_name, alias FROM contact")
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
    let mut profiles = BTreeMap::new();
    for (username, remark, nickname, alias) in rows.flatten() {
        if !valid_identity(&username) {
            continue;
        }
        let display_name = [nickname, remark]
            .into_iter()
            .flatten()
            .find_map(|value| valid_display_name(&value))
            .unwrap_or_else(|| username.clone());
        let wechat_id = alias
            .and_then(|value| valid_wechat_id(&value))
            .unwrap_or_else(|| username.clone());
        profiles.insert(
            username.clone(),
            ContactCardProfile {
                display_name,
                wechat_id,
                avatar_url: avatars.get(&username).cloned(),
            },
        );
    }
    Ok(profiles)
}

fn valid_wechat_id(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control))
        .then(|| value.to_owned())
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

fn extend_message_database_routes(
    paths: &SourcePaths,
    mut material: WechatKeyMaterial,
) -> Result<WechatKeyMaterial, DomainError> {
    let source_database = paths.account_root.join("db_storage/message/message_0.db");
    for database in paths
        .message_databases
        .iter()
        .chain(paths.media_databases.iter())
    {
        if material.key_for_database(database).is_some() {
            continue;
        }
        let relative = database
            .strip_prefix(&paths.account_root)
            .ok()
            .and_then(Path::to_str)
            .filter(|path| !path.is_empty())
            .ok_or_else(source_unavailable)?;
        let mut salt = [0_u8; 16];
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(database)
            .and_then(|mut file| file.read_exact(&mut salt))
            .map_err(|_| source_unavailable())?;
        material = material
            .with_database_route_from(&source_database, relative, salt)
            .map_err(|_| capability_unavailable())?;
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
    let lowercase = printable.to_ascii_lowercase();
    let bytes = lowercase.as_bytes();
    let mut search_from = 0_usize;
    while let Some(relative) = lowercase[search_from..].find(".dat") {
        let dat_start = search_from + relative;
        let mut end = dat_start;
        if matches!(bytes.get(end.saturating_sub(2)..end), Some(b".t" | b"_t")) {
            end = end.saturating_sub(2);
        }
        let mut start = end;
        while start > 0 && bytes[start - 1].is_ascii_hexdigit() {
            start -= 1;
        }
        if (8..=64).contains(&(end - start)) {
            return Some(lowercase[start..end].to_owned());
        }
        search_from = dat_start + 4;
    }
    lowercase
        .split(|character: char| !character.is_ascii_hexdigit())
        .find(|candidate| (16..=64).contains(&candidate.len()))
        .map(ToOwned::to_owned)
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

fn display_text_message_with_contacts(
    local_type: i64,
    content: &str,
    contact_cards: &BTreeMap<String, ContactCardProfile>,
) -> Option<String> {
    match local_type {
        1 => nonempty_text(content),
        42 => format_contact_card_message(content, contact_cards),
        48 => format_location_message(content),
        50 => format_call_message(content),
        49 if app_message_type(content) == Some(2000) => format_transfer_message(content),
        49 if app_message_type(content) == Some(2001) || content.contains("hongbao") => {
            format_red_packet_message(content)
        }
        49 if is_location_app_message(content) => format_location_message(content),
        49 if app_message_type(content) == Some(6) => None,
        49 => format_app_message(content),
        8_589_934_592_049 => format_transfer_message(content),
        8_594_229_559_345 => format_red_packet_message(content),
        _ => None,
    }
}

#[cfg(test)]
fn display_text_message(local_type: i64, content: &str) -> Option<String> {
    display_text_message_with_contacts(local_type, content, &BTreeMap::new())
}

fn format_contact_card_message(
    content: &str,
    contact_cards: &BTreeMap<String, ContactCardProfile>,
) -> Option<String> {
    let username =
        xml_attribute(content, "username").or_else(|| xml_attribute(content, "encryptusername"));
    let profile = username
        .as_ref()
        .and_then(|username| contact_cards.get(username));
    let display_name = profile
        .map(|profile| profile.display_name.clone())
        .or_else(|| xml_attribute(content, "nickname"));
    let wechat_id = profile
        .map(|profile| profile.wechat_id.clone())
        .or_else(|| xml_attribute(content, "alias"))
        .or(username);
    let avatar_url = profile
        .and_then(|profile| profile.avatar_url.clone())
        .or_else(|| xml_attribute(content, "bigheadimgurl"))
        .or_else(|| xml_attribute(content, "smallheadimgurl"))
        .and_then(|value| normalize_avatar_url(&value));
    let details = unique_nonempty([
        display_name,
        wechat_id.map(|wechat_id| format!("微信号：{wechat_id}")),
        avatar_url.map(|avatar_url| format!("头像：{avatar_url}")),
    ]);
    nonempty_text(if details.is_empty() {
        "[联系人名片]".to_owned()
    } else {
        format!("[联系人名片] {}", details.join(" · "))
    })
}

fn format_location_message(content: &str) -> Option<String> {
    let point_name = xml_attribute(content, "poiname")
        .or_else(|| xml_text(content, "poiname"))
        .or_else(|| xml_text(content, "poiName"))
        .or_else(|| xml_text(content, "title"));
    let address = xml_attribute(content, "label")
        .or_else(|| xml_text(content, "label"))
        .or_else(|| xml_text(content, "des"));
    let source = xml_text(content, "sourcedisplayname")
        .or_else(|| xml_text(content, "sourcename"))
        .or_else(|| xml_text(content, "appname"));
    let latitude = xml_attribute(content, "x")
        .or_else(|| xml_attribute(content, "latitude"))
        .or_else(|| xml_text(content, "latitude"));
    let longitude = xml_attribute(content, "y")
        .or_else(|| xml_attribute(content, "longitude"))
        .or_else(|| xml_text(content, "longitude"));
    let coordinate = latitude
        .zip(longitude)
        .map(|(latitude, longitude)| format!("坐标：{latitude},{longitude}"));
    let url = xml_text(content, "url");
    let details = unique_nonempty([point_name, address, source, coordinate, url]);
    nonempty_text(if details.is_empty() {
        "[位置]".to_owned()
    } else {
        format!("[位置] {}", details.join(" · "))
    })
}

fn is_location_app_message(content: &str) -> bool {
    let lower = content.to_ascii_lowercase();
    lower.contains("<location")
        || lower.contains("poiname=")
        || lower.contains("<poiname>")
        || [
            "高德地图",
            "百度地图",
            "腾讯地图",
            "大众点评",
            "amap.com",
            "map.baidu.com",
            "map.qq.com",
            "dianping.com",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
}

fn format_call_message(content: &str) -> Option<String> {
    let kind = match xml_text(content, "room_type").as_deref() {
        Some("0") => "视频通话",
        _ => "语音通话",
    };
    let detail = xml_text(content, "msg")
        .or_else(|| xml_text(content, "display_content"))
        .filter(|value| value != kind);
    nonempty_text(match detail {
        Some(detail) => format!("[{kind}] {detail}"),
        None => format!("[{kind}]"),
    })
}

fn format_transfer_message(content: &str) -> Option<String> {
    let amount = xml_text(content, "feedesc")
        .or_else(|| xml_text(content, "pay_memo"))
        .or_else(|| xml_text(content, "title"));
    let memo =
        xml_text(content, "pay_memo").filter(|memo| amount.as_deref() != Some(memo.as_str()));
    let detail = [amount, memo]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
    nonempty_text(if detail.is_empty() {
        "[转账]".to_owned()
    } else {
        format!("[转账] {detail}")
    })
}

fn format_red_packet_message(content: &str) -> Option<String> {
    let details = unique_nonempty([
        xml_text(content, "receivertitle"),
        xml_text(content, "sendertitle"),
        xml_text(content, "wishing"),
        xml_text(content, "pay_memo"),
        xml_text(content, "title"),
    ]);
    nonempty_text(if details.is_empty() {
        "[红包]".to_owned()
    } else {
        format!("[红包] {}", details.join(" · "))
    })
}

fn format_app_message(content: &str) -> Option<String> {
    let message_type = app_message_type(content);
    let source_username = xml_text(content, "sourceusername").unwrap_or_default();
    let source_name = xml_text(content, "sourcedisplayname")
        .or_else(|| xml_text(content, "sourcename"))
        .or_else(|| xml_text(content, "appname"));
    let label = match message_type {
        Some(3) => "音乐分享",
        Some(5) if source_username.starts_with("gh_") || source_name.is_some() => "公众号卡片",
        Some(5) => "链接分享",
        Some(6) => "文件分享",
        Some(19) => "聊天记录",
        Some(33 | 36) => "小程序",
        Some(51) => "视频号",
        Some(87) => "群公告",
        Some(115) => "微信礼物",
        _ => "卡片分享",
    };
    let title = if message_type == Some(51) {
        xml_text(content, "desc").or_else(|| xml_text(content, "title"))
    } else {
        xml_text(content, "title")
            .or_else(|| xml_text(content, "textannouncement"))
            .or_else(|| xml_text(content, "wishmessage"))
    };
    let description = xml_text(content, "des");
    let finder_name = xml_text(content, "nickname")
        .or_else(|| xml_text(content, "findernickname"))
        .or_else(|| xml_text(content, "finder_nickname"));
    let url = xml_text(content, "url");
    let details = unique_nonempty([title, description, finder_name, source_name, url]);
    nonempty_text(if details.is_empty() {
        format!("[{label}]")
    } else {
        format!("[{label}] {}", details.join(" · "))
    })
}

fn app_message_type(content: &str) -> Option<i64> {
    let appmsg = content
        .find("<appmsg")
        .and_then(|start| content[start..].find('>').map(|end| start + end + 1))?;
    xml_text(&content[appmsg..], "type")?.parse().ok()
}

fn xml_text(content: &str, tag: &str) -> Option<String> {
    let start_tag = format!("<{tag}>");
    let end_tag = format!("</{tag}>");
    let start = content.find(&start_tag)? + start_tag.len();
    let end = content[start..].find(&end_tag)? + start;
    let value = content[start..end].trim();
    let value = value
        .strip_prefix("<![CDATA[")
        .and_then(|value| value.strip_suffix("]]>"))
        .unwrap_or(value);
    nonempty_text(value)
}

fn xml_attribute(content: &str, attribute: &str) -> Option<String> {
    for quote in ['"', '\''] {
        let needle = format!("{attribute}={quote}");
        let Some(start) = content.find(&needle).map(|start| start + needle.len()) else {
            continue;
        };
        let Some(end) = content[start..].find(quote).map(|end| end + start) else {
            continue;
        };
        if let Some(value) = nonempty_text(&content[start..end]) {
            return Some(value);
        }
    }
    None
}

fn unique_nonempty<const N: usize>(values: [Option<String>; N]) -> Vec<String> {
    let mut unique = Vec::new();
    for value in values.into_iter().flatten() {
        if !unique.iter().any(|existing| existing == &value) {
            unique.push(value);
        }
    }
    unique
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

fn is_media_database(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("media_"))
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
        clean_account_directory_name, decode_value, decode_voice_attachment,
        decoded_image_attachment, decrypt_v4_image, derive_image_keys, display_text_message,
        extend_message_database_routes, file_attachment, image_mime_type, index_decoded_images,
        index_message_files, index_video_files, is_direct_conversation, is_message_database,
        kvcomm_codes_from_filename, message_table_name, parse_group_nicknames,
        parse_image_dat_name, parse_video_file_key, parse_video_length, parse_video_md5_candidates,
        read_contact_cards, read_conversation_metadata, read_database_images, read_database_text,
        read_database_videos, read_voice_blob, resolve_group_sender, resolve_image_dat_path,
        resolve_video_path, retention_cutoff_from, stage_decrypted_image, video_attachment,
        voice_same_time_index, ContactCardProfile, ConversationMetadata, ImageKeys, Session,
        SourcePaths, SourcePayload, TextReadContext,
    };
    use pca_keychain::{WechatDatabaseKeyMaterial, WechatKeyMaterial};

    #[test]
    fn production_formats_calls_payments_and_shared_cards_as_text() {
        assert_eq!(
            display_text_message(
                50,
                "<voip><room_type>1</room_type><msg><![CDATA[通话时长 00:32]]></msg></voip>"
            )
            .as_deref(),
            Some("[语音通话] 通话时长 00:32")
        );
        assert_eq!(
            display_text_message(
                50,
                "<voip><room_type>0</room_type><msg>对方无应答</msg></voip>"
            )
            .as_deref(),
            Some("[视频通话] 对方无应答")
        );
        assert_eq!(
            display_text_message(
                49,
                "<msg><appmsg><title>微信转账</title><type>2000</type><wcpayinfo><feedesc>￥20.00</feedesc><pay_memo>晚饭</pay_memo></wcpayinfo></appmsg></msg>"
            )
            .as_deref(),
            Some("[转账] ￥20.00 · 晚饭")
        );
        assert_eq!(
            display_text_message(
                49,
                "<msg><appmsg><type>2001</type><wcpayinfo><receivertitle>恭喜发财</receivertitle><pay_memo>生日快乐</pay_memo></wcpayinfo></appmsg></msg>"
            )
            .as_deref(),
            Some("[红包] 恭喜发财 · 生日快乐")
        );
        assert_eq!(
            display_text_message(
                42,
                "<msg username=\"wxid_contact123\" nickname=\"联系人\" alias=\"alias123\" />"
            )
            .as_deref(),
            Some("[联系人名片] 联系人 · 微信号：alias123")
        );
        let contacts = std::collections::BTreeMap::from([(
            "wxid_contact123".to_owned(),
            ContactCardProfile {
                display_name: "真实昵称".to_owned(),
                wechat_id: "A5202544".to_owned(),
                avatar_url: Some("https://avatar.example/contact.jpg".to_owned()),
            },
        )]);
        assert_eq!(
            super::display_text_message_with_contacts(
                42,
                "<msg username=\"wxid_contact123\" nickname=\"旧昵称\" />",
                &contacts,
            )
            .as_deref(),
            Some(
                "[联系人名片] 真实昵称 · 微信号：A5202544 · 头像：https://avatar.example/contact.jpg"
            )
        );
        assert_eq!(
            display_text_message(
                48,
                "<msg><location x=\"31.2304\" y=\"121.4737\" poiname=\"人民广场\" label=\"上海市黄浦区\" /></msg>"
            )
            .as_deref(),
            Some("[位置] 人民广场 · 上海市黄浦区 · 坐标：31.2304,121.4737")
        );
        assert_eq!(
            display_text_message(
                49,
                "<msg><appmsg><title>示例餐厅</title><des>上海市黄浦区示例路 1 号</des><type>5</type><url>https://www.dianping.com/shop/example</url><appname>大众点评</appname></appmsg></msg>"
            )
            .as_deref(),
            Some("[位置] 示例餐厅 · 上海市黄浦区示例路 1 号 · 大众点评 · https://www.dianping.com/shop/example")
        );
        assert_eq!(
            display_text_message(
                49,
                "<msg><appmsg><title>产品更新</title><des>更新说明</des><type>5</type><url>https://example.test/post</url><sourceusername>gh_source</sourceusername><sourcedisplayname>示例公众号</sourcedisplayname></appmsg></msg>"
            )
            .as_deref(),
            Some("[公众号卡片] 产品更新 · 更新说明 · 示例公众号 · https://example.test/post")
        );
        assert_eq!(
            display_text_message(
                49,
                "<msg><appmsg><title>当前版本不支持该内容</title><type>51</type><finderFeed><desc>视频标题</desc><nickname>视频作者</nickname></finderFeed></appmsg></msg>"
            )
            .as_deref(),
            Some("[视频号] 视频标题 · 视频作者")
        );
        assert_eq!(
            display_text_message(
                49,
                "<msg><appmsg><title>订单详情</title><des>来自示例 App</des><type>33</type><appname>示例 App</appname></appmsg></msg>"
            )
            .as_deref(),
            Some("[小程序] 订单详情 · 来自示例 App · 示例 App")
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the fixture covers every newly supported database message shape"
    )]
    fn production_reads_special_messages_and_advances_the_new_cursor() {
        let connection = Connection::open_in_memory().expect("open special message fixture");
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
                    message_content TEXT, compress_content BLOB\
                 );"
            ))
            .expect("create special message fixture schema");
        let rows = [
            (1_i64, 1_i64, "普通文本"),
            (
                2,
                42,
                "<msg username=\"wxid_card\" nickname=\"名片联系人\" />",
            ),
            (
                3,
                50,
                "<voip><room_type>1</room_type><msg>通话时长 00:05</msg></voip>",
            ),
            (
                4,
                49,
                "<msg><appmsg><type>2000</type><wcpayinfo><feedesc>￥8.88</feedesc><pay_memo>咖啡</pay_memo></wcpayinfo></appmsg></msg>",
            ),
            (
                5,
                8_594_229_559_345,
                "<msg><wcpayinfo><sendertitle>节日快乐</sendertitle></wcpayinfo></msg>",
            ),
            (
                6,
                49,
                "<msg><appmsg><title>视频标题</title><type>51</type><finderFeed><nickname>视频作者</nickname></finderFeed></appmsg></msg>",
            ),
            (
                7,
                48,
                "<msg><location x=\"31.2304\" y=\"121.4737\" poiname=\"人民广场\" label=\"上海市黄浦区\" /></msg>",
            ),
        ];
        for (local_id, local_type, content) in rows {
            connection
                .execute(
                    &format!(
                        "INSERT INTO \"{table_name}\" VALUES (?1, ?2, 1000, 2, 0, ?3, ?4, NULL)"
                    ),
                    rusqlite::params![local_id, 100 + local_id, local_type, content],
                )
                .expect("insert special message fixture");
        }
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
        let contact_cards = std::collections::BTreeMap::new();
        let cursors = std::collections::BTreeMap::new();
        let context = TextReadContext {
            database_name: "message_0.db",
            local_username: "wxid_local",
            account_id: "account",
            conversation_metadata: &metadata,
            contact_cards: &contact_cards,
            cursors: &cursors,
            cutoff: 0,
        };
        let records = read_database_text(
            &connection,
            &context,
            &[Session {
                username: conversation_id.to_owned(),
            }],
            20,
        )
        .expect("read special messages");
        assert_eq!(records.len(), 7);
        assert!(records.iter().all(|(cursor, _, record)| {
            cursor == "message_0.db:wxid_friend:display-text-v2" && record.is_some()
        }));
        let bodies = records
            .into_iter()
            .filter_map(|(_, _, record)| record)
            .filter_map(|record| match record.payload {
                SourcePayload::Text { body } => Some(body),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            bodies,
            vec![
                "普通文本",
                "[联系人名片] 名片联系人 · 微信号：wxid_card",
                "[语音通话] 通话时长 00:05",
                "[转账] ￥8.88 · 咖啡",
                "[红包] 节日快乐",
                "[视频号] 视频标题 · 视频作者",
                "[位置] 人民广场 · 上海市黄浦区 · 坐标：31.2304,121.4737",
            ]
        );
    }

    #[test]
    fn production_decodes_silk_voice_to_browser_wav() {
        let pcm = vec![0_i16; 2_400]
            .into_iter()
            .flat_map(i16::to_le_bytes)
            .collect::<Vec<_>>();
        let silk = silk_rs::encode_silk(pcm, 24_000, 24_000, true).expect("encode Silk fixture");
        let (attachment, path) = decode_voice_attachment(silk, "message_0.db", "Msg_abc", 7)
            .expect("decode voice fixture");
        let wav = std::fs::read(&path).expect("read staged WAV");
        assert_eq!(attachment.mime_type(), "audio/wav");
        assert!(wav.starts_with(b"RIFF"));
        assert_eq!(&wav[8..12], b"WAVE");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn production_matches_voice_across_tables_and_same_second_rows() {
        let connection = Connection::open_in_memory().expect("open voice fixture");
        connection
            .execute_batch(
                "CREATE TABLE Name2Id(user_name TEXT NOT NULL); \
                 INSERT INTO Name2Id(rowid, user_name) VALUES(7, 'wxid_friend'); \
                 CREATE TABLE VoiceInfo_0(unrelated BLOB); \
                 CREATE TABLE VoiceInfo_1( \
                    chat_name_id INTEGER, create_time INTEGER, msg_svr_id INTEGER, voice_data BLOB \
                 ); \
                 INSERT INTO VoiceInfo_1 VALUES(7, 100, 11, X'0102'); \
                 INSERT INTO VoiceInfo_1 VALUES(7, 100, 12, X'0304'); \
                 CREATE TABLE VoiceInfo_2( \
                    chat_name_id INTEGER, create_time INTEGER, msg_svr_id INTEGER, data TEXT \
                 ); \
                 INSERT INTO VoiceInfo_2 VALUES(7, 200, 13, '0506'); \
                 CREATE TABLE Msg_fixture( \
                    local_id INTEGER, local_type INTEGER, create_time INTEGER \
                 ); \
                 INSERT INTO Msg_fixture VALUES(20, 34, 100); \
                 INSERT INTO Msg_fixture VALUES(21, 34, 100);",
            )
            .expect("create voice fixture schema");

        assert_eq!(
            read_voice_blob(&connection, "wxid_friend", 12, 100, 0).expect("match exact server id"),
            Some(vec![3, 4])
        );
        assert_eq!(
            voice_same_time_index(&connection, "Msg_fixture", 100, 21)
                .expect("derive same-time index"),
            1
        );
        assert_eq!(
            read_voice_blob(&connection, "wxid_friend", 0, 100, 1).expect("match same-time index"),
            Some(vec![3, 4])
        );
        assert_eq!(
            read_voice_blob(&connection, "wxid_friend", 13, 200, 0)
                .expect("match later table and hex text"),
            Some(vec![5, 6])
        );
    }

    #[test]
    fn production_resolves_and_manifests_browser_video() {
        let account = tempfile::tempdir().expect("create video fixture");
        let directory = account.path().join("msg/video/2026-08");
        std::fs::create_dir_all(&directory).expect("create video directory");
        let file_key = "1234567890abcdef1234567890abcdef";
        let video = directory.join(format!("{file_key}.mp4"));
        let mut bytes = vec![0, 0, 0, 24];
        bytes.extend_from_slice(b"ftypisom");
        bytes.extend_from_slice(b"video fixture");
        std::fs::write(&video, &bytes).expect("write MP4 fixture");
        let files = index_video_files(account.path());
        let source_md5 = "abcdefabcdefabcdefabcdefabcdefab";
        let hardlinks =
            std::collections::BTreeMap::from([(source_md5.to_owned(), file_key.to_owned())]);
        let content = format!(
            "<videomsg newmd5=\"{source_md5}\" length=\"{}\" />",
            bytes.len()
        );

        assert_eq!(parse_video_md5_candidates(&content), vec![source_md5]);
        assert_eq!(
            parse_video_length(&content),
            Some(u64::try_from(bytes.len()).expect("fixture size"))
        );
        assert_eq!(
            parse_video_file_key(&Value::Text(format!("cache/{file_key}.mp4"))).as_deref(),
            Some(file_key)
        );
        assert_eq!(
            resolve_video_path(Some(&content), &Value::Null, &hardlinks, &files),
            Some(&video)
        );
        let (attachment, source) =
            video_attachment(&video, "message_0.db", "Msg_abc", 9).expect("create video manifest");
        assert_eq!(attachment.mime_type(), "video/mp4");
        assert_eq!(
            attachment.size_bytes(),
            u64::try_from(bytes.len()).expect("fixture size")
        );
        assert_eq!(source, video.canonicalize().expect("canonical video path"));
    }

    #[test]
    fn production_indexes_and_manifests_downloaded_file_messages() {
        let account = tempfile::tempdir().expect("create account fixture");
        let month = account.path().join("msg/file/2026-08");
        std::fs::create_dir_all(&month).expect("create file directory");
        let source = month.join("report.pdf");
        std::fs::write(&source, b"%PDF-1.7 fixture").expect("write file fixture");
        let files = index_message_files(account.path(), 0);
        assert_eq!(files["report.pdf"], vec![source.clone()]);
        let (attachment, resolved) =
            file_attachment(&source, "message_0.db", "Msg_abc", 9).expect("manifest file");
        assert_eq!(
            resolved,
            std::fs::canonicalize(source).expect("canonical file")
        );
        assert_eq!(attachment.kind(), pca_domain::MessageKind::File);
        assert_eq!(attachment.file_name(), Some("report.pdf"));
        assert_eq!(attachment.mime_type(), "application/pdf");
    }

    #[test]
    fn production_emits_video_only_for_an_eligible_conversation() {
        let connection = Connection::open_in_memory().expect("open video message fixture");
        let conversation_id = "wxid_friend";
        let table_name = message_table_name(conversation_id);
        let file_key = "1234567890abcdef1234567890abcdef";
        connection
            .execute_batch(&format!(
                "CREATE TABLE Name2Id (user_name TEXT);\
                 INSERT INTO Name2Id(rowid, user_name) VALUES (1, 'wxid_local');\
                 INSERT INTO Name2Id(rowid, user_name) VALUES (2, '{conversation_id}');\
                 CREATE TABLE \"{table_name}\" (\
                    local_id INTEGER, server_id INTEGER, create_time INTEGER,\
                    real_sender_id INTEGER, status INTEGER, local_type INTEGER,\
                    message_content TEXT, compress_content BLOB, packed_info_data TEXT\
                 );\
                 INSERT INTO \"{table_name}\" VALUES (7, 107, 1000, 2, 0, 43,\
                    '<videomsg md5=\"{file_key}\" length=\"24\" />', X'', '');"
            ))
            .expect("create video message schema");
        let account = tempfile::tempdir().expect("create video source fixture");
        let directory = account.path().join("msg/video/1970-01");
        std::fs::create_dir_all(&directory).expect("create video source directory");
        let video = directory.join(format!("{file_key}.mp4"));
        let mut bytes = vec![0, 0, 0, 24];
        bytes.extend_from_slice(b"ftypisom");
        bytes.extend_from_slice(b"fixture-data");
        std::fs::write(video, bytes).expect("write video source");
        let files = index_video_files(account.path());
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
        let contact_cards = std::collections::BTreeMap::new();
        let cursors = std::collections::BTreeMap::new();
        let context = TextReadContext {
            database_name: "message_0.db",
            local_username: "wxid_local",
            account_id: "account",
            conversation_metadata: &metadata,
            contact_cards: &contact_cards,
            cursors: &cursors,
            cutoff: 0,
        };
        let batch = read_database_videos(
            &connection,
            &context,
            &[Session {
                username: conversation_id.to_owned(),
            }],
            &std::collections::BTreeMap::new(),
            &files,
            20,
        )
        .expect("read video message");
        assert_eq!(batch.records.len(), 1);
        assert_eq!(batch.records[0].source_sequence, 7);
        assert_eq!(
            batch.cursor_updates,
            vec![(format!("message_0.db:{conversation_id}:video"), 7)]
        );
    }

    #[test]
    fn production_derives_media_database_route_from_validated_message_key() {
        let fixture = tempfile::tempdir().expect("create route fixture");
        let account = fixture.path().join("wxid_example");
        let directory = account.join("db_storage/message");
        std::fs::create_dir_all(&directory).expect("create database directory");
        let message = directory.join("message_0.db");
        let media = directory.join("media_0.db");
        std::fs::write(&message, [1_u8; 32]).expect("write message fixture");
        std::fs::write(&media, [2_u8; 32]).expect("write media fixture");
        let paths = SourcePaths {
            account_root: account,
            session_database: PathBuf::new(),
            contact_database: PathBuf::new(),
            message_databases: vec![message.clone()],
            media_databases: vec![media.clone()],
            local_username: "wxid_example".to_owned(),
            account_id: "account-proof".to_owned(),
        };
        let message_key = WechatDatabaseKeyMaterial::new(
            "db_storage/message/message_0.db",
            [7_u8; 32],
            [1_u8; 16],
        )
        .expect("create message route");
        let material = WechatKeyMaterial::new_for_databases("account-proof", vec![message_key])
            .expect("create routed material");

        let extended =
            extend_message_database_routes(&paths, material).expect("derive media route");
        let media_key = extended
            .key_for_database(&media)
            .expect("media route exists");
        assert_eq!(media_key.raw_key(), &[7_u8; 32]);
        assert_eq!(media_key.salt(), Some(&[2_u8; 16]));
    }
    use aes::Aes128;
    use cipher::{generic_array::GenericArray, BlockEncrypt, KeyInit};
    use md5::{Digest as _, Md5};
    use rusqlite::{types::Value, Connection};
    use sha2::Sha256;
    use std::{
        fs,
        path::{Path, PathBuf},
    };

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

        let keys = derive_image_keys(&account);
        assert!(keys
            .iter()
            .any(|keys| keys.xor == 3 && keys.aes == hex.as_bytes()[..16]));
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
        encrypted.extend_from_slice(
            &i32::try_from(aes_plain.len())
                .expect("fixture length")
                .to_le_bytes(),
        );
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
        let cards = read_contact_cards(&connection).expect("read contact cards");

        assert_eq!(metadata["wxid_friend"].display_name, "Friend Remark");
        assert_eq!(cards["wxid_friend"].display_name, "Friend Nick");
        assert_eq!(cards["wxid_friend"].wechat_id, "friend_alias");
        assert_eq!(
            cards["wxid_friend"].avatar_url.as_deref(),
            Some("https://avatar.example/friend-big.jpg")
        );
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
    fn production_resolves_a_direct_image_dat_name_embedded_in_packed_info() {
        let account = tempfile::tempdir().expect("create direct image fixture");
        let identity = "ed300ffbabff6feee7217d4df7d05fe5";
        let source = account
            .path()
            .join(format!("msg/attach/abc/1970-01/Img/{identity}_t.dat"));
        std::fs::create_dir_all(source.parent().expect("direct image parent"))
            .expect("create direct image directory");
        std::fs::write(&source, [1_u8, 2, 3]).expect("write encrypted image fixture");
        let packed =
            Value::Blob(format!("metadata(path=cache/{identity}_t.dat);other=value").into_bytes());

        assert_eq!(parse_image_dat_name(&packed).as_deref(), Some(identity));
        assert_eq!(
            resolve_image_dat_path(
                account.path(),
                "Msg_abc",
                1,
                None,
                parse_image_dat_name(&packed).as_deref(),
            ),
            Some(source)
        );
    }

    #[test]
    fn production_accepts_legacy_and_reportnow_kvcomm_filenames() {
        assert_eq!(
            kvcomm_codes_from_filename("key_123_456.statistic"),
            vec![123, 456]
        );
        assert_eq!(
            kvcomm_codes_from_filename("key_reportnow_164965891_4066647068_-1_60_ready.statistic"),
            vec![164_965_891, 4_066_647_068, 60]
        );
        assert!(kvcomm_codes_from_filename("not-a-key.statistic").is_empty());
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
        let contact_cards = std::collections::BTreeMap::new();
        let cursors = std::collections::BTreeMap::new();
        let context = TextReadContext {
            database_name: "message_0.db",
            local_username: "wxid_local",
            account_id: "account",
            conversation_metadata: &metadata,
            contact_cards: &contact_cards,
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
            &[],
            account.path(),
            20,
        )
        .expect("scan image rows");
        assert_eq!(batch.records.len(), 1);
        assert_eq!(batch.records[0].source_sequence, 21);
        assert_eq!(batch.cursor_updates[0].1, 21);
    }
}
