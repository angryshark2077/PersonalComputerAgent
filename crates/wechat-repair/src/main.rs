//! Explicit developer repair utility for a locally running `WeChat` process.
//!
//! This binary is never called by `agentd`. When an owner runs it directly, it captures one
//! reviewed WCDB key call from an ephemeral debug copy, validates the result against private
//! `SQLCipher` snapshots, and stores only a validated result in the PCA Keychain item. The official
//! `/Applications/WeChat.app` bundle is never modified.
#![deny(unsafe_code)]

mod lldb_capture;

use std::{
    env,
    ffi::OsStr,
    fs::{self, File},
    io::Read,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use pca_domain::MessageKind;
use pca_keychain::{
    load_wechat_key_material, MacOSKeychainStore, WechatDatabaseKeyMaterial, WechatKeyMaterial,
};
use pca_provider_contracts::CommunicationProviderFactory;
use pca_wechat_provider::sqlcipher_source::{inspect_recovered_schema, validate_recovered_key};
use pca_wechat_provider::MacOSWechatProviderFactory;
use sha2::{Digest, Sha256};

const WECHAT_DYLIB_PATH: &str = "/Applications/WeChat.app/Contents/Resources/wechat.dylib";
const WECHAT_INFO_PATH: &str = "/Applications/WeChat.app/Contents/Info.plist";

#[derive(Clone, Eq, PartialEq)]
struct Candidate {
    raw_key: [u8; 32],
    salt: Option<[u8; 16]>,
}

fn main() -> std::process::ExitCode {
    let command = env::args_os().nth(1);
    let is_schema_probe = command.as_deref() == Some(OsStr::new("probe-schema"));
    let is_message_probe = command.as_deref() == Some(OsStr::new("probe-messages"));
    let result = match command {
        None => run(),
        Some(command) if command == "probe-schema" => probe_schema(),
        Some(command) if command == "probe-messages" => probe_messages(),
        Some(_) => Err(RepairError::Usage),
    };
    match result {
        Ok(()) => {
            if is_message_probe {
                println!("WeChat message probe completed.");
            } else if is_schema_probe {
                println!("WeChat schema probe completed.");
            } else {
                println!("WeChat key repair completed.");
            }
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("pca-wechat-repair: {}", error.message());
            std::process::ExitCode::from(error.exit_code())
        }
    }
}

fn probe_messages() -> Result<(), RepairError> {
    let mut provider = MacOSWechatProviderFactory
        .create()
        .map_err(|_| RepairError::MessageProbeFailed)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| RepairError::MessageProbeFailed)?;
    let records = runtime
        .block_on(async {
            provider.discover().await?;
            provider.poll_once().await
        })
        .map_err(|_| RepairError::MessageProbeFailed)?;
    let mut text = 0;
    let mut image = 0;
    let mut audio = 0;
    let mut video = 0;
    for record in &records {
        match record.message().kind() {
            MessageKind::Text => text += 1,
            MessageKind::Image => image += 1,
            MessageKind::Audio => audio += 1,
            MessageKind::Video => video += 1,
        }
    }
    println!(
        "Eligible records: total={} text={} image={} audio={} video={}",
        records.len(),
        text,
        image,
        audio,
        video
    );
    Ok(())
}

fn probe_schema() -> Result<(), RepairError> {
    let source = discover_source()?;
    let material = load_wechat_key_material(&MacOSKeychainStore)
        .map_err(|_| RepairError::Keychain)?
        .ok_or(RepairError::Keychain)?;
    for database in &source.databases {
        let tables =
            inspect_recovered_schema(&database.absolute_path, &material, Duration::from_secs(10))
                .map_err(|_| RepairError::SchemaProbeFailed)?;
        println!("DATABASE {}", database.relative_path);
        for table in tables {
            println!("table {}: {}", table.name(), table.columns().join(","));
        }
    }
    Ok(())
}

fn run() -> Result<(), RepairError> {
    let source = discover_source()?;
    if reuse_existing_key_for_hardlink(&source)? {
        println!("Validated the existing WCDB credential for hardlink.db.");
        return Ok(());
    }
    let candidates = collect_candidates()?;
    println!(
        "Found {} bounded WCDB key candidates; validating required databases.",
        candidates.len()
    );
    let account_id = account_id_for_root(&source.account_root);
    let mut database_keys = Vec::with_capacity(source.databases.len());
    for database in &source.databases {
        let candidate = candidates
            .iter()
            .filter(|candidate| candidate.salt.is_none_or(|salt| salt == database.salt))
            .find(|candidate| {
                candidate_material(&account_id, database, candidate).is_ok_and(|material| {
                    validate_recovered_key(
                        &database.absolute_path,
                        &material,
                        Duration::from_secs(2),
                    )
                    .is_ok()
                })
            })
            .ok_or(RepairError::NoValidatedCandidate)?;
        let database_key = match candidate.salt {
            Some(salt) => {
                WechatDatabaseKeyMaterial::new(database.relative_path, candidate.raw_key, salt)
            }
            None => {
                WechatDatabaseKeyMaterial::new_passphrase(database.relative_path, candidate.raw_key)
            }
        }
        .map_err(|_| RepairError::Keychain)?;
        database_keys.push(database_key);
    }
    let material = WechatKeyMaterial::new_for_databases(&account_id, database_keys)
        .map_err(|_| RepairError::Keychain)?;
    MacOSKeychainStore
        .store_validated_wechat_key_material(&material)
        .map_err(|_| RepairError::Keychain)?;
    Ok(())
}

fn reuse_existing_key_for_hardlink(source: &Source) -> Result<bool, RepairError> {
    const MESSAGE_PATH: &str = "db_storage/message/message_0.db";
    const HARDLINK_PATH: &str = "db_storage/hardlink/hardlink.db";

    let Some(material) =
        load_wechat_key_material(&MacOSKeychainStore).map_err(|_| RepairError::Keychain)?
    else {
        return Ok(false);
    };
    let message = source
        .databases
        .iter()
        .find(|database| database.relative_path == MESSAGE_PATH)
        .ok_or(RepairError::SourceUnavailable)?;
    let hardlink = source
        .databases
        .iter()
        .find(|database| database.relative_path == HARDLINK_PATH)
        .ok_or(RepairError::SourceUnavailable)?;
    let extended = if material.key_for_database(&hardlink.absolute_path).is_some() {
        material
    } else {
        material
            .with_database_route_from(&message.absolute_path, HARDLINK_PATH, hardlink.salt)
            .map_err(|_| RepairError::Keychain)?
    };
    if validate_recovered_key(&hardlink.absolute_path, &extended, Duration::from_secs(2)).is_err() {
        return Ok(false);
    }
    MacOSKeychainStore
        .store_validated_wechat_key_material(&extended)
        .map_err(|_| RepairError::Keychain)?;
    Ok(true)
}

struct Source {
    account_root: PathBuf,
    databases: [SourceDatabase; 4],
}

struct SourceDatabase {
    relative_path: &'static str,
    absolute_path: PathBuf,
    salt: [u8; 16],
}

fn collect_candidates() -> Result<Vec<Candidate>, RepairError> {
    let version = wechat_version()?;
    if !version.starts_with("4.") {
        return Err(RepairError::UnsupportedBuild);
    }
    let dylib_path = Path::new(WECHAT_DYLIB_PATH);
    if let Some(profile) =
        lldb_capture::profile_for_build(&version, dylib_path).map_err(map_capture_error)?
    {
        let pid = running_wechat_pid()?;
        println!("Capturing the reviewed WeChat {version} WCDB key call from a temporary copy.");
        let raw_key = lldb_capture::capture_key(pid, profile).map_err(map_capture_error)?;
        return Ok(vec![Candidate {
            raw_key,
            salt: None,
        }]);
    }
    Err(RepairError::UnsupportedBuild)
}

fn wechat_version() -> Result<String, RepairError> {
    let output = Command::new("/usr/bin/plutil")
        .args([
            "-extract",
            "CFBundleShortVersionString",
            "raw",
            "-o",
            "-",
            WECHAT_INFO_PATH,
        ])
        .output()
        .map_err(|_| RepairError::SourceUnavailable)?;
    if !output.status.success() {
        return Err(RepairError::SourceUnavailable);
    }
    std::str::from_utf8(&output.stdout)
        .ok()
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 32)
        .map(ToOwned::to_owned)
        .ok_or(RepairError::SourceUnavailable)
}

fn running_wechat_pid() -> Result<libc::pid_t, RepairError> {
    let output = Command::new("pgrep")
        .args(["-xo", "WeChat"])
        .output()
        .map_err(|_| RepairError::WeChatUnavailable)?;
    if !output.status.success() {
        return Err(RepairError::WeChatUnavailable);
    }
    std::str::from_utf8(&output.stdout)
        .ok()
        .and_then(|value| value.trim().parse::<libc::pid_t>().ok())
        .filter(|pid| *pid > 0)
        .ok_or(RepairError::WeChatUnavailable)
}

const fn map_capture_error(error: lldb_capture::CaptureError) -> RepairError {
    match error {
        lldb_capture::CaptureError::BuildUnavailable => RepairError::SourceUnavailable,
        lldb_capture::CaptureError::DebuggerUnavailable => RepairError::DebuggerUnavailable,
        lldb_capture::CaptureError::DebuggerFailed => RepairError::CaptureFailed,
        lldb_capture::CaptureError::TimedOut => RepairError::CaptureTimedOut,
    }
}

fn discover_source() -> Result<Source, RepairError> {
    let home = env::var_os("HOME").ok_or(RepairError::SourceUnavailable)?;
    let root = PathBuf::from(home)
        .join("Library/Containers/com.tencent.xinWeChat/Data/Documents/xwechat_files");
    let mut accounts = fs::read_dir(root)
        .map_err(|_| RepairError::SourceUnavailable)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| name.starts_with("wxid_"))
        })
        .filter(|path| {
            path.join("db_storage/session/session.db").is_file()
                && path.join("db_storage/contact/contact.db").is_file()
                && path.join("db_storage/message/message_0.db").is_file()
                && path.join("db_storage/hardlink/hardlink.db").is_file()
        });
    let account_root = accounts.next().ok_or(RepairError::SourceUnavailable)?;
    if accounts.next().is_some() {
        return Err(RepairError::MultipleAccounts);
    }
    let databases = [
        source_database(&account_root, "db_storage/session/session.db")?,
        source_database(&account_root, "db_storage/contact/contact.db")?,
        source_database(&account_root, "db_storage/message/message_0.db")?,
        source_database(&account_root, "db_storage/hardlink/hardlink.db")?,
    ];
    Ok(Source {
        account_root,
        databases,
    })
}

fn source_database(
    account_root: &Path,
    relative_path: &'static str,
) -> Result<SourceDatabase, RepairError> {
    let absolute_path = account_root.join(relative_path);
    let mut salt = [0_u8; 16];
    File::open(&absolute_path)
        .and_then(|mut file| file.read_exact(&mut salt))
        .map_err(|_| RepairError::SourceUnavailable)?;
    Ok(SourceDatabase {
        relative_path,
        absolute_path,
        salt,
    })
}

fn account_id_for_root(account_root: &Path) -> String {
    let fingerprint = Sha256::digest(account_root.as_os_str().as_bytes());
    format!("wechat-db-v1:{fingerprint:x}")
}

fn candidate_material(
    account_id: &str,
    database: &SourceDatabase,
    candidate: &Candidate,
) -> Result<WechatKeyMaterial, RepairError> {
    let database_key = match candidate.salt {
        Some(salt) => {
            WechatDatabaseKeyMaterial::new(database.relative_path, candidate.raw_key, salt)
        }
        None => {
            WechatDatabaseKeyMaterial::new_passphrase(database.relative_path, candidate.raw_key)
        }
    }
    .map_err(|_| RepairError::Keychain)?;
    WechatKeyMaterial::new_for_databases(account_id, vec![database_key])
        .map_err(|_| RepairError::Keychain)
}

enum RepairError {
    Usage,
    SourceUnavailable,
    MultipleAccounts,
    WeChatUnavailable,
    DebuggerUnavailable,
    CaptureFailed,
    CaptureTimedOut,
    NoValidatedCandidate,
    Keychain,
    UnsupportedBuild,
    SchemaProbeFailed,
    MessageProbeFailed,
}

impl RepairError {
    const fn exit_code(&self) -> u8 {
        match self {
            Self::Usage => 2,
            Self::SourceUnavailable | Self::MultipleAccounts | Self::WeChatUnavailable => 3,
            Self::DebuggerUnavailable => 4,
            Self::CaptureFailed | Self::CaptureTimedOut | Self::NoValidatedCandidate => 5,
            Self::Keychain => 1,
            Self::UnsupportedBuild => 6,
            Self::SchemaProbeFailed => 7,
            Self::MessageProbeFailed => 8,
        }
    }

    const fn message(&self) -> &'static str {
        match self {
            Self::Usage => "usage: pca-wechat-repair [probe-schema|probe-messages]",
            Self::SourceUnavailable => "WeChat source databases are unavailable",
            Self::MultipleAccounts => {
                "multiple local WeChat accounts require explicit repair support"
            }
            Self::WeChatUnavailable => "WeChat is not running",
            Self::DebuggerUnavailable => "LLDB is unavailable for this reviewed WeChat build",
            Self::CaptureFailed => "the reviewed WeChat key capture could not start",
            Self::CaptureTimedOut => {
                "no new WeChat database handle opened before the key capture timed out"
            }
            Self::NoValidatedCandidate => "a required database key is not loaded in WeChat memory",
            Self::Keychain => "the validated key could not be stored in Keychain",
            Self::UnsupportedBuild => "this WeChat 4.x build has no reviewed key-capture profile",
            Self::SchemaProbeFailed => "the validated WeChat schema could not be inspected",
            Self::MessageProbeFailed => "eligible WeChat messages could not be read",
        }
    }
}
