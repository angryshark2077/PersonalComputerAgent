use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::{
        fs::{DirBuilderExt, OpenOptionsExt},
        net::UnixDatagram,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use sha2::{Digest, Sha256};
use tempfile::TempDir;

const OFFICIAL_WECHAT_APP: &str = "/Applications/WeChat.app";
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(20);
const QUIT_TIMEOUT: Duration = Duration::from_secs(10);
const APPLE_EVENT_QUIT_GRACE: Duration = Duration::from_secs(2);
const CHILD_EXIT_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CaptureProfile {
    pub version: &'static str,
    pub dylib_sha256: &'static str,
    pub symbol: &'static str,
    pub call_slide: u64,
}

const CAPTURE_PROFILES: &[CaptureProfile] = &[CaptureProfile {
    version: "4.1.12",
    dylib_sha256: "f28329ed2599e8567f6b0a09f031d05666a48abd97dbc7a3380891ddbcff6cdc",
    symbol: "___lldb_unnamed_symbol_4f242e0",
    call_slide: 60,
}];

pub(super) fn profile_for_build(
    version: &str,
    dylib_path: &Path,
) -> Result<Option<CaptureProfile>, CaptureError> {
    let mut file = File::open(dylib_path).map_err(|_| CaptureError::BuildUnavailable)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| CaptureError::BuildUnavailable)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    let digest = format!("{:x}", digest.finalize());
    Ok(CAPTURE_PROFILES
        .iter()
        .copied()
        .find(|profile| profile.version == version && profile.dylib_sha256 == digest))
}

pub(super) fn capture_key(
    pid: libc::pid_t,
    profile: CaptureProfile,
) -> Result<[u8; 32], CaptureError> {
    let debug_copy = DebugWechatCopy::create()?;
    quit_wechat(pid)?;
    let _restart = RestartOfficialWechat;
    let lldb_path = find_lldb()?;
    let directory = PrivateCaptureDirectory::create(pid)?;
    let socket_path = directory.path.join("key.sock");
    let socket = UnixDatagram::bind(&socket_path).map_err(|_| CaptureError::DebuggerFailed)?;
    socket
        .set_read_timeout(Some(POLL_INTERVAL))
        .map_err(|_| CaptureError::DebuggerFailed)?;

    let script_path = directory.path.join("pca_capture.py");
    write_private_file(&script_path, &capture_script(&socket_path))?;
    let command_path = directory.path.join("capture.lldb");
    write_private_file(
        &command_path,
        &capture_commands(&script_path, &debug_copy.executable(), profile),
    )?;

    let diagnostic_output = std::env::var_os("PCA_WECHAT_REPAIR_DEBUG").is_some();
    let mut debugger = Command::new(lldb_path);
    debugger
        .arg("--no-lldbinit")
        .arg("-s")
        .arg(command_path)
        .stdin(Stdio::null());
    if diagnostic_output {
        debugger.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    } else {
        debugger.stdout(Stdio::null()).stderr(Stdio::null());
    }
    let mut child = debugger
        .spawn()
        .map_err(|_| CaptureError::DebuggerUnavailable)?;

    let result = receive_key(&socket, &mut child);
    finish_debugger(&mut child);
    debug_copy.terminate_running_instances();
    result
}

struct DebugWechatCopy {
    _directory: TempDir,
    app_path: PathBuf,
}

impl DebugWechatCopy {
    fn create() -> Result<Self, CaptureError> {
        let directory = tempfile::Builder::new()
            .prefix("pca-wechat-debug-")
            .tempdir()
            .map_err(|_| CaptureError::DebuggerFailed)?;
        let app_path = directory.path().join("WeChat.app");
        command_status(
            "/usr/bin/ditto",
            &[
                Path::new(OFFICIAL_WECHAT_APP).as_os_str(),
                app_path.as_os_str(),
            ],
        )?;

        let entitlements = directory.path().join("entitlements.plist");
        let output = Command::new("/usr/bin/codesign")
            .args(["-d", "--entitlements", ":-", OFFICIAL_WECHAT_APP])
            .output()
            .map_err(|_| CaptureError::DebuggerFailed)?;
        if !output.status.success() || output.stdout.is_empty() {
            return Err(CaptureError::DebuggerFailed);
        }
        fs::write(&entitlements, output.stdout).map_err(|_| CaptureError::DebuggerFailed)?;
        command_status(
            "/usr/libexec/PlistBuddy",
            &[
                std::ffi::OsStr::new("-c"),
                std::ffi::OsStr::new("Add :com.apple.security.get-task-allow bool true"),
                entitlements.as_os_str(),
            ],
        )?;
        command_status(
            "/usr/libexec/PlistBuddy",
            &[
                std::ffi::OsStr::new("-c"),
                std::ffi::OsStr::new(
                    "Add :com.apple.security.cs.disable-library-validation bool true",
                ),
                entitlements.as_os_str(),
            ],
        )?;
        command_status(
            "/usr/bin/codesign",
            &[
                std::ffi::OsStr::new("--force"),
                std::ffi::OsStr::new("--options"),
                std::ffi::OsStr::new("runtime"),
                std::ffi::OsStr::new("--sign"),
                std::ffi::OsStr::new("-"),
                std::ffi::OsStr::new("--entitlements"),
                entitlements.as_os_str(),
                app_path.as_os_str(),
            ],
        )?;
        Ok(Self {
            _directory: directory,
            app_path,
        })
    }

    fn executable(&self) -> PathBuf {
        self.app_path.join("Contents/MacOS/WeChat")
    }

    fn terminate_running_instances(&self) {
        let output = Command::new("/bin/ps")
            .args(["-axo", "pid=,command="])
            .output();
        let Ok(output) = output else {
            return;
        };
        let Ok(processes) = std::str::from_utf8(&output.stdout) else {
            return;
        };
        let pids = pids_under_app(processes, &self.app_path);
        for pid in &pids {
            let _ = signal_process(*pid, "TERM");
        }
        let deadline = Instant::now()
            .checked_add(CHILD_EXIT_TIMEOUT)
            .unwrap_or_else(Instant::now);
        while Instant::now() < deadline && pids.iter().any(|pid| process_exists(*pid)) {
            thread::sleep(POLL_INTERVAL);
        }
        for pid in pids {
            if process_exists(pid) {
                let _ = signal_process(pid, "KILL");
            }
        }
    }
}

fn pids_under_app(processes: &str, app_path: &Path) -> Vec<libc::pid_t> {
    let prefix = app_path.to_string_lossy();
    processes
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let split_at = line.find(char::is_whitespace)?;
            let pid = line[..split_at].parse::<libc::pid_t>().ok()?;
            let command = line[split_at..].trim_start();
            (pid > 0 && command.starts_with(prefix.as_ref())).then_some(pid)
        })
        .collect()
}

fn command_status(program: &str, arguments: &[&std::ffi::OsStr]) -> Result<(), CaptureError> {
    let status = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|_| CaptureError::DebuggerFailed)?;
    status
        .success()
        .then_some(())
        .ok_or(CaptureError::DebuggerFailed)
}

struct RestartOfficialWechat;

impl Drop for RestartOfficialWechat {
    fn drop(&mut self) {
        let _ = Command::new("/usr/bin/open")
            .arg(OFFICIAL_WECHAT_APP)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn quit_wechat(pid: libc::pid_t) -> Result<(), CaptureError> {
    let status = Command::new("osascript")
        .args([
            "-e",
            "tell application id \"com.tencent.xinWeChat\" to quit",
        ])
        .status()
        .map_err(|_| CaptureError::DebuggerFailed)?;
    if !status.success() {
        return Err(CaptureError::DebuggerFailed);
    }

    let graceful_deadline = Instant::now()
        .checked_add(APPLE_EVENT_QUIT_GRACE)
        .ok_or(CaptureError::DebuggerFailed)?;
    while Instant::now() < graceful_deadline {
        if !process_exists(pid) {
            return Ok(());
        }
        thread::sleep(POLL_INTERVAL);
    }

    if !signal_process(pid, "TERM") {
        return Err(CaptureError::DebuggerFailed);
    }
    let deadline = Instant::now()
        .checked_add(QUIT_TIMEOUT)
        .ok_or(CaptureError::DebuggerFailed)?;
    while Instant::now() < deadline {
        if !process_exists(pid) {
            return Ok(());
        }
        thread::sleep(POLL_INTERVAL);
    }
    Err(CaptureError::TimedOut)
}

fn process_exists(pid: libc::pid_t) -> bool {
    signal_process(pid, "0")
}

fn signal_process(pid: libc::pid_t, signal: &str) -> bool {
    Command::new("/bin/kill")
        .args(["-s", signal, &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn find_lldb() -> Result<PathBuf, CaptureError> {
    let output = Command::new("xcrun")
        .args(["--find", "lldb"])
        .output()
        .map_err(|_| CaptureError::DebuggerUnavailable)?;
    if !output.status.success() {
        return Err(CaptureError::DebuggerUnavailable);
    }
    let path = std::str::from_utf8(&output.stdout)
        .ok()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or(CaptureError::DebuggerUnavailable)?;
    path.is_file()
        .then_some(path)
        .ok_or(CaptureError::DebuggerUnavailable)
}

fn receive_key(socket: &UnixDatagram, child: &mut Child) -> Result<[u8; 32], CaptureError> {
    let deadline = Instant::now()
        .checked_add(CAPTURE_TIMEOUT)
        .ok_or(CaptureError::DebuggerFailed)?;
    let mut key = [0_u8; 32];
    loop {
        match socket.recv(&mut key) {
            Ok(32) => return Ok(key),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Ok(_) | Err(_) => return Err(CaptureError::DebuggerFailed),
        }
        if child
            .try_wait()
            .map_err(|_| CaptureError::DebuggerFailed)?
            .is_some()
        {
            return Err(CaptureError::DebuggerFailed);
        }
        if Instant::now() >= deadline {
            return Err(CaptureError::TimedOut);
        }
    }
}

fn finish_debugger(child: &mut Child) {
    let deadline = Instant::now()
        .checked_add(CHILD_EXIT_TIMEOUT)
        .unwrap_or_else(Instant::now);
    while Instant::now() < deadline {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        thread::sleep(POLL_INTERVAL);
    }

    let pid = i32::try_from(child.id()).unwrap_or_default();
    if pid > 0 {
        let _ = signal_process(pid, "INT");
    }
    thread::sleep(POLL_INTERVAL);
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
}

fn capture_script(socket_path: &Path) -> String {
    let socket_path = socket_path.to_string_lossy();
    format!(
        r#"import lldb
import socket

SOCKET_PATH = {socket_path:?}

def capture(frame, _bp_loc, _internal_dict):
    length = frame.FindRegister("x2").GetValueAsUnsigned()
    if length != 32:
        return False
    address = frame.FindRegister("x1").GetValueAsUnsigned()
    error = lldb.SBError()
    data = frame.GetThread().GetProcess().ReadMemory(address, length, error)
    if not error.Success() or data is None or len(data) != 32:
        return True
    sender = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
    try:
        sender.sendto(bytes(data), SOCKET_PATH)
    finally:
        sender.close()
    return True

"#
    )
}

fn capture_commands(script_path: &Path, executable: &Path, profile: CaptureProfile) -> String {
    format!(
        "target create {}\n\
breakpoint set -s wechat.dylib -n {} -R {} -c '$x2 == 32' -o true\n\
command script import {}\n\
breakpoint command add -F pca_capture.capture 1\n\
run\n\
process kill\n\
quit\n",
        executable.display(),
        profile.symbol,
        profile.call_slide,
        script_path.display()
    )
}

fn write_private_file(path: &Path, contents: &str) -> Result<(), CaptureError> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| CaptureError::DebuggerFailed)?;
    file.write_all(contents.as_bytes())
        .map_err(|_| CaptureError::DebuggerFailed)
}

struct PrivateCaptureDirectory {
    path: PathBuf,
}

impl PrivateCaptureDirectory {
    fn create(pid: libc::pid_t) -> Result<Self, CaptureError> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| CaptureError::DebuggerFailed)?
            .as_nanos();
        // macOS limits Unix-domain socket paths to 104 bytes. `/var/folders/...` temporary roots
        // can exceed that limit once a private directory and socket name are appended.
        let path =
            PathBuf::from("/tmp").join(format!("pca-wx-{pid}-{}-{nonce:x}", std::process::id()));
        DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|_| CaptureError::DebuggerFailed)?;
        Ok(Self { path })
    }
}

impl Drop for PrivateCaptureDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CaptureError {
    BuildUnavailable,
    DebuggerUnavailable,
    DebuggerFailed,
    TimedOut,
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{capture_commands, capture_script, pids_under_app, CaptureProfile};

    #[test]
    fn generated_script_is_bound_to_the_reviewed_call_site() {
        let script = capture_script(Path::new("/tmp/pca-test/key.sock"));
        assert!(script.contains("length != 32"));
        assert!(!script.contains("print(data"));
    }

    #[test]
    fn generated_commands_use_a_pending_reviewed_symbol_breakpoint() {
        let commands = capture_commands(
            Path::new("/tmp/pca-test/pca_capture.py"),
            Path::new("/tmp/pca-test/WeChat.app/Contents/MacOS/WeChat"),
            CaptureProfile {
                version: "4.1.12",
                dylib_sha256: "fixture",
                symbol: "___lldb_unnamed_symbol_4f242e0",
                call_slide: 60,
            },
        );
        assert!(commands.contains("-s wechat.dylib -n ___lldb_unnamed_symbol_4f242e0 -R 60"));
        assert!(commands.contains("-F pca_capture.capture 1"));
        assert!(commands.contains("run\nprocess kill\nquit\n"));
    }

    #[test]
    fn temporary_process_cleanup_is_scoped_to_the_exact_copy() {
        let processes = "  41 /Applications/WeChat.app/Contents/MacOS/WeChat\n\
                         42 /tmp/pca-copy/WeChat.app/Contents/MacOS/WeChat\n\
                         43 /tmp/pca-copy/WeChat.app/Contents/Frameworks/helper --flag\n\
                         44 /tmp/pca-copy-other/WeChat.app/Contents/MacOS/WeChat\n";
        assert_eq!(
            pids_under_app(processes, Path::new("/tmp/pca-copy/WeChat.app")),
            vec![42, 43]
        );
    }
}
