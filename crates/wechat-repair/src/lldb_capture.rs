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

const AUTHORIZATION_TIMEOUT: Duration = Duration::from_mins(5);
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(20);
const AUTOMATIC_CAPTURE_TIMEOUT_SECS: u64 = 7 * 24 * 60 * 60;
const CHILD_EXIT_TIMEOUT: Duration = Duration::from_secs(25);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const OFFICIAL_WECHAT_EXECUTABLE: &str = "/Applications/WeChat.app/Contents/MacOS/WeChat";

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
    preflight()?;
    let lldb_path = find_lldb()?;
    let directory = PrivateCaptureDirectory::create("manual")?;
    let socket_path = directory.path.join("key.sock");
    let socket = UnixDatagram::bind(&socket_path).map_err(|_| CaptureError::DebuggerFailed)?;
    socket
        .set_read_timeout(Some(POLL_INTERVAL))
        .map_err(|_| CaptureError::DebuggerFailed)?;

    let script_path = directory.path.join("pca_capture.py");
    write_private_file(&script_path, &capture_script(&socket_path))?;
    let command_path = directory.path.join("capture.lldb");
    write_private_file(&command_path, &capture_commands(&script_path, pid, profile))?;
    let ready_path = directory.path.join("debugger.ready");
    let finished_path = directory.path.join("debugger.finished");
    let runner_path = directory.path.join("run-debugger.sh");
    write_private_executable(
        &runner_path,
        &elevated_runner(&lldb_path, &command_path, &ready_path, &finished_path, pid),
    )?;

    let apple_script = administrator_script(&runner_path);
    let diagnostic_output = std::env::var_os("PCA_WECHAT_REPAIR_DEBUG").is_some();
    let mut command = Command::new("/usr/bin/osascript");
    command.args(["-e", &apple_script]).stdin(Stdio::null());
    if diagnostic_output {
        command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    } else {
        command.stdout(Stdio::null()).stderr(Stdio::null());
    }
    let mut child = command
        .spawn()
        .map_err(|_| CaptureError::DebuggerUnavailable)?;

    let result = receive_key(
        &socket,
        &ready_path,
        &finished_path,
        &mut child,
        Some(CAPTURE_TIMEOUT),
        || {},
    );
    finish_child(&mut child);
    result
}

pub(super) fn capture_key_on_next_launch(
    profile: CaptureProfile,
    on_authorized: impl FnOnce(),
) -> Result<[u8; 32], CaptureError> {
    preflight()?;
    let lldb_path = find_lldb()?;
    let directory = PrivateCaptureDirectory::create("automatic")?;
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
        &next_launch_capture_commands(&script_path, profile),
    )?;
    let ready_path = directory.path.join("debugger.ready");
    let finished_path = directory.path.join("debugger.finished");
    let runner_path = directory.path.join("run-debugger.sh");
    write_private_executable(
        &runner_path,
        &waiting_elevated_runner(&lldb_path, &command_path, &ready_path, &finished_path),
    )?;

    let apple_script = administrator_script(&runner_path);
    let mut child = Command::new("/usr/bin/osascript")
        .args(["-e", &apple_script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| CaptureError::DebuggerUnavailable)?;
    let result = receive_key(
        &socket,
        &ready_path,
        &finished_path,
        &mut child,
        Some(Duration::from_secs(AUTOMATIC_CAPTURE_TIMEOUT_SECS)),
        on_authorized,
    );
    finish_child(&mut child);
    result
}

pub(super) fn preflight() -> Result<(), CaptureError> {
    ensure_sip_disabled()?;
    find_lldb().map(|_| ())
}

fn ensure_sip_disabled() -> Result<(), CaptureError> {
    let output = Command::new("/usr/bin/csrutil")
        .arg("status")
        .output()
        .map_err(|_| CaptureError::SipStatusUnavailable)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if output.status.success() && stdout.contains("status: disabled") {
        Ok(())
    } else if output.status.success() {
        Err(CaptureError::SipEnabled)
    } else {
        Err(CaptureError::SipStatusUnavailable)
    }
}

fn find_lldb() -> Result<PathBuf, CaptureError> {
    let output = Command::new("/usr/bin/xcrun")
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

fn receive_key(
    socket: &UnixDatagram,
    ready_path: &Path,
    finished_path: &Path,
    child: &mut Child,
    capture_timeout: Option<Duration>,
    on_authorized: impl FnOnce(),
) -> Result<[u8; 32], CaptureError> {
    let authorization_deadline = Instant::now()
        .checked_add(AUTHORIZATION_TIMEOUT)
        .ok_or(CaptureError::DebuggerFailed)?;
    let mut capture_deadline = None;
    let mut on_authorized = Some(on_authorized);
    let mut authorized = false;
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
        if !authorized && ready_path.is_file() {
            authorized = true;
            capture_deadline =
                capture_timeout.and_then(|timeout| Instant::now().checked_add(timeout));
            if let Some(callback) = on_authorized.take() {
                callback();
            }
        }
        let child_exited = child
            .try_wait()
            .map_err(|_| CaptureError::DebuggerFailed)?
            .is_some();
        if capture_has_failed(authorized, child_exited, finished_path.is_file()) {
            return Err(CaptureError::DebuggerFailed);
        }
        if capture_deadline.is_some_and(|deadline| Instant::now() >= deadline)
            || (!authorized && Instant::now() >= authorization_deadline)
        {
            return Err(CaptureError::TimedOut);
        }
    }
}

const fn capture_has_failed(
    authorized: bool,
    authorization_process_exited: bool,
    debugger_runner_finished: bool,
) -> bool {
    debugger_runner_finished || (!authorized && authorization_process_exited)
}

fn finish_child(child: &mut Child) {
    let deadline = Instant::now()
        .checked_add(CHILD_EXIT_TIMEOUT)
        .unwrap_or_else(Instant::now);
    while Instant::now() < deadline {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        thread::sleep(POLL_INTERVAL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn capture_script(socket_path: &Path) -> String {
    let socket_path = socket_path.to_string_lossy();
    format!(
        r#"import lldb
import os
import socket

SOCKET_PATH = {socket_path:?}
OFFICIAL_WECHAT_EXECUTABLE = {OFFICIAL_WECHAT_EXECUTABLE:?}

def capture(frame, _bp_loc, _internal_dict):
    process = frame.GetThread().GetProcess()
    executable = process.GetProcessInfo().GetExecutableFile()
    actual_path = os.path.realpath(os.path.join(executable.GetDirectory(), executable.GetFilename()))
    if actual_path != OFFICIAL_WECHAT_EXECUTABLE:
        return True
    length = frame.FindRegister("x2").GetValueAsUnsigned()
    if length != 32:
        return False
    address = frame.FindRegister("x1").GetValueAsUnsigned()
    error = lldb.SBError()
    data = process.ReadMemory(address, length, error)
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

fn capture_commands(script_path: &Path, pid: libc::pid_t, profile: CaptureProfile) -> String {
    format!(
        "process attach --pid {pid}\n\
breakpoint set -s wechat.dylib -n {} -R {} -c '$x2 == 32' -o true\n\
command script import {}\n\
breakpoint command add -F pca_capture.capture 1\n\
continue\n\
process detach\n\
quit\n",
        profile.symbol,
        profile.call_slide,
        script_path.display()
    )
}

fn next_launch_capture_commands(script_path: &Path, profile: CaptureProfile) -> String {
    format!(
        "target create {OFFICIAL_WECHAT_EXECUTABLE}\n\
breakpoint set -s wechat.dylib -n {} -R {} -c '$x2 == 32' -o true\n\
command script import {}\n\
breakpoint command add -F pca_capture.capture 1\n\
process attach --name WeChat --waitfor --include-existing\n\
continue\n\
process detach\n\
quit\n",
        profile.symbol,
        profile.call_slide,
        script_path.display()
    )
}

fn elevated_runner(
    lldb_path: &Path,
    command_path: &Path,
    ready_path: &Path,
    finished_path: &Path,
    wechat_pid: libc::pid_t,
) -> String {
    format!(
        "#!/bin/sh\n\
set -u\n\
finish() {{ : > {}; }}\n\
trap finish EXIT\n\
: > {}\n\
{} --no-lldbinit -s {} </dev/null >/dev/null 2>&1 &\n\
debugger_pid=$!\n\
(sleep {}; /bin/kill -INT \"$debugger_pid\" 2>/dev/null || true; sleep 2; /bin/kill -TERM \"$debugger_pid\" 2>/dev/null || true; sleep 1; /bin/kill -KILL \"$debugger_pid\" 2>/dev/null || true) &\n\
watchdog_pid=$!\n\
wait \"$debugger_pid\"\n\
status=$?\n\
/usr/bin/pkill -TERM -P \"$watchdog_pid\" 2>/dev/null || true\n\
/bin/kill \"$watchdog_pid\" 2>/dev/null || true\n\
wait \"$watchdog_pid\" 2>/dev/null || true\n\
/bin/kill -CONT {wechat_pid} 2>/dev/null || true\n\
exit \"$status\"\n",
        shell_quote(finished_path),
        shell_quote(ready_path),
        shell_quote(lldb_path),
        shell_quote(command_path),
        CAPTURE_TIMEOUT.as_secs(),
    )
}

fn waiting_elevated_runner(
    lldb_path: &Path,
    command_path: &Path,
    ready_path: &Path,
    finished_path: &Path,
) -> String {
    format!(
        "#!/bin/sh\n\
set -u\n\
finish() {{ : > {}; }}\n\
trap finish EXIT\n\
{} --no-lldbinit -s {} </dev/null >/dev/null 2>&1 &\n\
debugger_pid=$!\n\
/bin/sleep 1\n\
if ! /bin/kill -0 \"$debugger_pid\" 2>/dev/null; then wait \"$debugger_pid\"; exit $?; fi\n\
: > {}\n\
(sleep {}; /bin/kill -INT \"$debugger_pid\" 2>/dev/null || true; sleep 2; /bin/kill -TERM \"$debugger_pid\" 2>/dev/null || true; sleep 1; /bin/kill -KILL \"$debugger_pid\" 2>/dev/null || true) &\n\
watchdog_pid=$!\n\
wait \"$debugger_pid\"\n\
status=$?\n\
/usr/bin/pkill -TERM -P \"$watchdog_pid\" 2>/dev/null || true\n\
/bin/kill \"$watchdog_pid\" 2>/dev/null || true\n\
wait \"$watchdog_pid\" 2>/dev/null || true\n\
exit \"$status\"\n",
        shell_quote(finished_path),
        shell_quote(lldb_path),
        shell_quote(command_path),
        shell_quote(ready_path),
        AUTOMATIC_CAPTURE_TIMEOUT_SECS,
    )
}

fn administrator_script(runner_path: &Path) -> String {
    let command = format!("/bin/sh {}", shell_quote(runner_path));
    format!(
        "do shell script \"{}\" with administrator privileges",
        apple_script_escape(&command)
    )
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn apple_script_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn write_private_file(path: &Path, contents: &str) -> Result<(), CaptureError> {
    write_private_file_with_mode(path, contents, 0o600)
}

fn write_private_executable(path: &Path, contents: &str) -> Result<(), CaptureError> {
    write_private_file_with_mode(path, contents, 0o700)
}

fn write_private_file_with_mode(
    path: &Path,
    contents: &str,
    mode: u32,
) -> Result<(), CaptureError> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(mode)
        .open(path)
        .map_err(|_| CaptureError::DebuggerFailed)?;
    file.write_all(contents.as_bytes())
        .map_err(|_| CaptureError::DebuggerFailed)
}

struct PrivateCaptureDirectory {
    path: PathBuf,
}

impl PrivateCaptureDirectory {
    fn create(label: &str) -> Result<Self, CaptureError> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| CaptureError::DebuggerFailed)?
            .as_nanos();
        let path =
            PathBuf::from("/tmp").join(format!("pca-wx-{label}-{}-{nonce:x}", std::process::id()));
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
    SipEnabled,
    SipStatusUnavailable,
    DebuggerUnavailable,
    DebuggerFailed,
    TimedOut,
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        administrator_script, capture_commands, capture_has_failed, capture_script,
        elevated_runner, next_launch_capture_commands, waiting_elevated_runner, CaptureProfile,
    };

    fn profile() -> CaptureProfile {
        CaptureProfile {
            version: "4.1.12",
            dylib_sha256: "fixture",
            symbol: "___lldb_unnamed_symbol_4f242e0",
            call_slide: 60,
        }
    }

    #[test]
    fn generated_script_is_bound_to_the_reviewed_call_site() {
        let script = capture_script(Path::new("/tmp/pca-test/key.sock"));
        assert!(script.contains("length != 32"));
        assert!(script.contains("actual_path != OFFICIAL_WECHAT_EXECUTABLE"));
        assert!(script.contains("/Applications/WeChat.app/Contents/MacOS/WeChat"));
        assert!(!script.contains("print(data"));
    }

    #[test]
    fn generated_commands_attach_and_detach_without_starting_or_killing_wechat() {
        let commands = capture_commands(Path::new("/tmp/pca-test/pca_capture.py"), 412, profile());
        assert!(commands.contains("process attach --pid 412"));
        assert!(commands.contains("-s wechat.dylib -n ___lldb_unnamed_symbol_4f242e0 -R 60"));
        assert!(commands.contains("continue\nprocess detach\nquit\n"));
        assert!(!commands.contains("target create"));
        assert!(!commands.contains("process kill"));
        assert!(!commands.contains("run\n"));
        assert!(!commands.contains("detach-on-error"));
    }

    #[test]
    fn elevated_runner_has_bounded_debugger_and_resumes_official_wechat() {
        let runner = elevated_runner(
            Path::new("/Applications/Xcode.app/Contents/Developer/usr/bin/lldb"),
            Path::new("/tmp/pca-test/capture.lldb"),
            Path::new("/tmp/pca-test/debugger.ready"),
            Path::new("/tmp/pca-test/debugger.finished"),
            412,
        );
        assert!(runner.contains("trap finish EXIT"));
        assert!(runner.contains("debugger.finished"));
        assert!(runner.contains("kill -INT \"$debugger_pid\""));
        assert!(runner.contains("pkill -TERM -P \"$watchdog_pid\""));
        assert!(runner.contains("kill -CONT 412"));
        assert!(!runner.contains("WeChat.app"));
        assert!(!runner.contains("codesign"));
    }

    #[test]
    fn automatic_commands_wait_for_wechat_without_launching_it() {
        let commands =
            next_launch_capture_commands(Path::new("/tmp/pca-test/pca_capture.py"), profile());
        assert!(commands.contains("target create /Applications/WeChat.app/Contents/MacOS/WeChat"));
        assert!(commands.contains("process attach --name WeChat --waitfor --include-existing"));
        assert!(commands.contains("continue\nprocess detach\nquit\n"));
        assert!(!commands.contains("run\n"));
        assert!(!commands.contains("process kill"));
        assert!(!commands.contains("detach-on-error"));
    }

    #[test]
    fn automatic_runner_authorizes_before_waiting_and_is_time_bounded() {
        let runner = waiting_elevated_runner(
            Path::new("/Applications/Xcode.app/Contents/Developer/usr/bin/lldb"),
            Path::new("/tmp/pca-test/capture.lldb"),
            Path::new("/tmp/pca-test/debugger.ready"),
            Path::new("/tmp/pca-test/debugger.finished"),
        );
        assert!(runner.contains("debugger.ready"));
        assert!(runner.contains("kill -0 \"$debugger_pid\""));
        assert!(runner.contains("kill -INT \"$debugger_pid\""));
        assert!(runner.contains("pkill -TERM -P \"$watchdog_pid\""));
        assert!(!runner.contains("open "));
        assert!(!runner.contains("codesign"));
    }

    #[test]
    fn authorized_capture_survives_early_osascript_exit_until_runner_finishes() {
        assert!(!capture_has_failed(true, true, false));
        assert!(capture_has_failed(true, true, true));
        assert!(capture_has_failed(false, true, false));
    }

    #[test]
    fn administrator_prompt_runs_only_the_private_debugger_runner() {
        let script = administrator_script(Path::new("/tmp/pca-test/run-debugger.sh"));
        assert!(script.contains("with administrator privileges"));
        assert!(script.contains("/bin/sh '/tmp/pca-test/run-debugger.sh'"));
        assert!(!script.contains("WeChat.app"));
    }
}
