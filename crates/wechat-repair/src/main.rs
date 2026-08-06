//! Explicit one-time repair utility for the reviewed official `WeChat` process.
//!
//! This binary is never called by `agentd`. Setup can authorize a detached one-time worker while
//! `WeChat` is closed; the worker waits for the next official launch, captures one reviewed WCDB
//! key call, validates it against private `SQLCipher` snapshots, and stores only a validated result
//! in the PCA Keychain item. The official process is never quit and its bundle is never modified.
#![deny(unsafe_code)]

mod lldb_capture;

use std::{
    collections::BTreeMap,
    env,
    ffi::OsStr,
    fs::{self, File},
    io::{BufRead, BufReader, Read, Write},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use pca_domain::MessageKind;
use pca_keychain::{
    load_wechat_key_material, CredentialError, MacOSKeychainStore, WechatDatabaseKeyMaterial,
    WechatKeyMaterial,
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
    let is_automatic_worker = command.as_deref() == Some(OsStr::new("automatic-worker"));
    let result = match command {
        None => run(),
        Some(command) if command == "prepare-automatic" => prepare_automatic(),
        Some(command) if command == "automatic-worker" => run_automatic_worker(),
        Some(command) if command == "probe-schema" => probe_schema(),
        Some(command) if command == "probe-messages" => probe_messages(),
        Some(_) => Err(RepairError::Usage),
    };
    match result {
        Ok(()) => {
            if is_automatic_worker {
                // The detached worker has no interactive output after its authorization handshake.
            } else if is_message_probe {
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
    let mut provider = MacOSWechatProviderFactory::default()
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
    let mut file = 0;
    let special_diagnostic = env::var_os("PCA_WECHAT_SPECIAL_SESSION_DIAGNOSTIC").is_some();
    let mut special_counts = BTreeMap::from([
        ("gh_3dfda90e39d6", [0_usize; 6]),
        ("notifymessage", [0_usize; 6]),
    ]);
    for record in &records {
        let kind_index = match record.message().kind() {
            MessageKind::Text => {
                text += 1;
                1
            }
            MessageKind::Image => {
                image += 1;
                2
            }
            MessageKind::Audio => {
                audio += 1;
                3
            }
            MessageKind::Video => {
                video += 1;
                4
            }
            MessageKind::File => {
                file += 1;
                5
            }
        };
        if special_diagnostic {
            if let Some(counts) = special_counts.get_mut(record.message().conversation_id()) {
                counts[0] += 1;
                counts[kind_index] += 1;
            }
        }
        if env::var_os("PCA_WECHAT_MEDIA_DIAGNOSTIC").is_some()
            && record.message().kind() != MessageKind::Text
        {
            println!(
                "MEDIA conversation={} sequence={} kind={:?} attachments={}",
                record.message().conversation_id(),
                record.source_sequence(),
                record.message().kind(),
                record.completed_media().len()
            );
        }
    }
    if special_diagnostic {
        for (conversation, counts) in special_counts {
            println!(
                "SPECIAL_RECORDS conversation={conversation} total={} text={} image={} audio={} video={} file={}",
                counts[0], counts[1], counts[2], counts[3], counts[4], counts[5]
            );
        }
    }
    println!(
        "Eligible records: total={} text={} image={} audio={} video={} file={}",
        records.len(),
        text,
        image,
        audio,
        video,
        file
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
    validate_and_store(&source, &candidates)
}

fn validate_and_store(source: &Source, candidates: &[Candidate]) -> Result<(), RepairError> {
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

fn prepare_automatic() -> Result<(), RepairError> {
    if !automatic_recovery_required(&load_wechat_key_material(&MacOSKeychainStore))? {
        return Ok(());
    }
    reviewed_capture_profile()?;
    lldb_capture::preflight().map_err(map_capture_error)?;

    let executable = env::current_exe().map_err(|_| RepairError::CaptureFailed)?;
    let mut child = Command::new(executable)
        .arg("automatic-worker")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| RepairError::CaptureFailed)?;
    let stdout = child.stdout.take().ok_or(RepairError::CaptureFailed)?;
    let mut line = String::new();
    BufReader::new(stdout)
        .read_line(&mut line)
        .map_err(|_| RepairError::CaptureFailed)?;
    if line.trim() != "AUTHORIZED" {
        let _ = child.wait();
        return Err(RepairError::CaptureFailed);
    }
    // `Child` does not terminate the process on drop. Setup can now exit while the one-time worker
    // remains under launchd and waits for the first official WeChat launch.
    drop(child);
    Ok(())
}

fn run_automatic_worker() -> Result<(), RepairError> {
    if !automatic_recovery_required(&load_wechat_key_material(&MacOSKeychainStore))? {
        return Err(RepairError::Keychain);
    }
    let profile = reviewed_capture_profile()?;
    let raw_key = lldb_capture::capture_key_on_next_launch(profile, || {
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, "AUTHORIZED");
        let _ = stdout.flush();
    })
    .map_err(map_capture_error)?;
    let source = wait_for_source(Duration::from_mins(5))?;
    validate_and_store(
        &source,
        &[Candidate {
            raw_key,
            salt: None,
        }],
    )?;
    restart_agent_after_automatic_recovery();
    Ok(())
}

fn restart_agent_after_automatic_recovery() {
    let Ok(output) = Command::new("/usr/bin/id").arg("-u").output() else {
        return;
    };
    let Some(target) = current_user_launch_agent_target(&output.stdout) else {
        return;
    };
    let _ = Command::new("/bin/launchctl")
        .args(["kickstart", "-k", &target])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn current_user_launch_agent_target(uid_output: &[u8]) -> Option<String> {
    let uid = std::str::from_utf8(uid_output).ok()?.trim();
    (!uid.is_empty() && uid.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| format!("gui/{uid}/com.pca.agentd"))
}

fn automatic_recovery_required(
    credential: &Result<Option<WechatKeyMaterial>, CredentialError>,
) -> Result<bool, RepairError> {
    match credential {
        Ok(Some(_)) => Ok(false),
        Err(CredentialError::CorruptSecret | CredentialError::InvalidCredential) => Ok(true),
        Ok(None) | Err(_) => Err(RepairError::Keychain),
    }
}

fn wait_for_source(timeout: Duration) -> Result<Source, RepairError> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or(RepairError::SourceUnavailable)?;
    loop {
        match discover_source() {
            Ok(source) => return Ok(source),
            Err(RepairError::SourceUnavailable) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(500));
            }
            Err(error) => return Err(error),
        }
    }
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
    let profile = reviewed_capture_profile()?;
    let pid = running_wechat_pid()?;
    println!(
        "Capturing the reviewed WeChat {} WCDB key call from the running official app.",
        profile.version
    );
    let raw_key = lldb_capture::capture_key(pid, profile).map_err(map_capture_error)?;
    Ok(vec![Candidate {
        raw_key,
        salt: None,
    }])
}

fn reviewed_capture_profile() -> Result<lldb_capture::CaptureProfile, RepairError> {
    let version = wechat_version()?;
    if !version.starts_with("4.") {
        return Err(RepairError::UnsupportedBuild);
    }
    lldb_capture::profile_for_build(&version, Path::new(WECHAT_DYLIB_PATH))
        .map_err(map_capture_error)?
        .ok_or(RepairError::UnsupportedBuild)
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
        lldb_capture::CaptureError::SipEnabled => RepairError::SipEnabled,
        lldb_capture::CaptureError::SipStatusUnavailable => RepairError::SipStatusUnavailable,
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
    SipEnabled,
    SipStatusUnavailable,
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
            Self::SipEnabled | Self::SipStatusUnavailable => 9,
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
            Self::Usage => {
                "usage: pca-wechat-repair [prepare-automatic|probe-schema|probe-messages]"
            }
            Self::SourceUnavailable => "WeChat source databases are unavailable",
            Self::MultipleAccounts => {
                "multiple local WeChat accounts require explicit repair support"
            }
            Self::WeChatUnavailable => "WeChat is not running",
            Self::SipEnabled => {
                "System Integrity Protection must be temporarily disabled before WeChat key recovery"
            }
            Self::SipStatusUnavailable => "System Integrity Protection status could not be verified",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_recovery_accepts_the_installer_invalid_placeholder() {
        assert!(matches!(
            automatic_recovery_required(&Err(CredentialError::InvalidCredential)),
            Ok(true)
        ));
    }

    #[test]
    fn automatic_recovery_accepts_legacy_corrupt_placeholders() {
        assert!(matches!(
            automatic_recovery_required(&Err(CredentialError::CorruptSecret)),
            Ok(true)
        ));
    }

    #[test]
    fn automatic_recovery_skips_an_existing_validated_key() {
        let material =
            WechatKeyMaterial::new("local-account-proof", [0x42; 32]).expect("valid fixture");

        assert!(matches!(
            automatic_recovery_required(&Ok(Some(material))),
            Ok(false)
        ));
    }

    #[test]
    fn automatic_recovery_rejects_missing_or_unavailable_keychain_items() {
        assert!(matches!(
            automatic_recovery_required(&Ok(None)),
            Err(RepairError::Keychain)
        ));
        assert!(matches!(
            automatic_recovery_required(&Err(CredentialError::OperationFailed)),
            Err(RepairError::Keychain)
        ));
    }

    #[test]
    fn recovered_key_targets_the_current_users_agent() {
        assert_eq!(
            current_user_launch_agent_target(b"501\n").as_deref(),
            Some("gui/501/com.pca.agentd")
        );
        assert_eq!(current_user_launch_agent_target(b"root\n"), None);
    }
}
