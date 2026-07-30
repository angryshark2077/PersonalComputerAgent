use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

#[cfg(feature = "process-test-hooks")]
use std::{io::Read, os::unix::fs::PermissionsExt};

use pca_agent_runtime::{RuntimePaths, SingleInstanceGuard};
#[cfg(feature = "process-test-hooks")]
use pca_bridge_client::supervisor::BridgeSupervisorConfig;
use pca_domain::{AgentStatus, BridgeStatus, RuntimeStatusEnvelope};
use rusqlite::Connection;

const PROCESS_TIMEOUT: Duration = Duration::from_secs(5);

struct ChildGuard {
    child: Child,
    fatal_release: Option<PathBuf>,
    bridge_pid_file: Option<PathBuf>,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self {
            child,
            fatal_release: None,
            bridge_pid_file: None,
        }
    }

    #[cfg(feature = "process-test-hooks")]
    fn with_fatal_cleanup(mut self, release: PathBuf, bridge_pid_file: PathBuf) -> Self {
        self.fatal_release = Some(release);
        self.bridge_pid_file = Some(bridge_pid_file);
        self
    }

    fn wait_bounded(&mut self) -> ExitStatus {
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().expect("poll child") {
                return status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!("child process exceeded five-second bound");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let mut forced_agent_kill = false;
        if self.child.try_wait().ok().flatten().is_none() {
            if let Some(release) = &self.fatal_release {
                let _ = fs::write(release, b"release-on-drop\n");
                let deadline = Instant::now() + PROCESS_TIMEOUT;
                while Instant::now() < deadline && self.child.try_wait().ok().flatten().is_none() {
                    thread::sleep(Duration::from_millis(10));
                }
            }
            if self.child.try_wait().ok().flatten().is_none() {
                let _ = self.child.kill();
                let _ = self.child.wait();
                forced_agent_kill = true;
            }
        }
        if forced_agent_kill {
            let Some(pid_file) = &self.bridge_pid_file else {
                return;
            };
            if let Ok(pid) = fs::read_to_string(pid_file) {
                let _ = Command::new("/bin/kill")
                    .args(["-KILL", pid.trim()])
                    .status();
            }
        }
    }
}

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_pca-agentd")
}

fn spawn_run(root: &Path) -> ChildGuard {
    ChildGuard::new(
        Command::new(binary())
            .arg("run")
            .arg("--runtime-root")
            .arg(root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn agentd"),
    )
}

fn wait_for_status(path: &Path) -> RuntimeStatusEnvelope {
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    loop {
        if let Ok(bytes) = fs::read(path) {
            if let Ok(status) = serde_json::from_slice(&bytes) {
                return status;
            }
        }
        assert!(
            Instant::now() < deadline,
            "status was not written in five seconds"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(feature = "process-test-hooks")]
fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "expected process rendezvous file within five seconds"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(feature = "process-test-hooks")]
fn wait_for_file_while_running(child: &mut ChildGuard, path: &Path) {
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    while !path.exists() {
        if let Some(status) = child.child.try_wait().expect("poll rendezvous child") {
            let mut stderr = String::new();
            if let Some(mut pipe) = child.child.stderr.take() {
                pipe.read_to_string(&mut stderr).expect("read child stderr");
            }
            panic!("child exited before rendezvous: {status}; stderr={stderr}");
        }
        assert!(
            Instant::now() < deadline,
            "expected process rendezvous file within five seconds"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(feature = "process-test-hooks")]
fn install_fake_bridge(paths: &RuntimePaths) -> String {
    let executable = paths
        .app_dir
        .join("PersonalComputerAgent.app/Contents/Resources/bin/PCAPlatformBridge");
    fs::create_dir_all(executable.parent().expect("Bridge executable parent"))
        .expect("create fake Bridge bundle path");
    fs::copy(env!("CARGO_BIN_EXE_pca-test-bridge"), &executable)
        .expect("install native fake Bridge");
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
        .expect("make fake Bridge executable");

    let pid_file = paths.run_dir.join("fake-bridge.pid");
    let mut probe = Command::new(&executable)
        .arg("--socket")
        .arg(&paths.socket_file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("execute fake Bridge with production argv");
    wait_for_file(&pid_file);
    let probe_pid = fs::read_to_string(&pid_file).expect("read fake Bridge executable probe pid");
    probe.kill().expect("stop fake Bridge executable probe");
    probe.wait().expect("reap fake Bridge executable probe");
    fs::remove_file(pid_file).expect("remove fake Bridge executable probe pid");
    probe_pid
}

fn signal(child: &ChildGuard, name: &str) {
    let result = Command::new("/bin/kill")
        .arg(format!("-{name}"))
        .arg(child.child.id().to_string())
        .status()
        .expect("send signal");
    assert!(result.success(), "signal command failed");
}

fn marker_members(paths: &RuntimePaths) -> Vec<PathBuf> {
    let prefix = "crash-marker.json.";
    fs::read_dir(&paths.data_dir)
        .expect("read data directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(prefix))
        })
        .collect()
}

#[test]
fn run_writes_fresh_local_health_when_bridge_is_missing_and_stops_cleanly() {
    let root = tempfile::tempdir().expect("temporary runtime root");
    let paths = RuntimePaths::under(root.path());
    let mut child = spawn_run(root.path());

    let initial = wait_for_status(&paths.status_file);
    assert_eq!(initial.agent_status, AgentStatus::Unpaired);
    assert_eq!(initial.bridge_status, BridgeStatus::Degraded);
    assert!(initial.local_healthy);

    let health = Command::new(binary())
        .arg("health")
        .arg("--runtime-root")
        .arg(root.path())
        .output()
        .expect("run health command");
    assert!(
        health.status.success(),
        "health stderr: {}",
        String::from_utf8_lossy(&health.stderr)
    );

    signal(&child, "TERM");
    assert!(child.wait_bounded().success());

    let final_status = wait_for_status(&paths.status_file);
    assert_eq!(final_status.agent_status, AgentStatus::Stopped);
    assert_eq!(final_status.bridge_status, BridgeStatus::Stopped);
    assert!(
        marker_members(&paths).is_empty(),
        "clean exit must remove marker"
    );

    let connection = Connection::open(&paths.database_file).expect("open runtime database");
    let lifecycle = connection
        .prepare(
            "SELECT event_type, COUNT(*) FROM events_local GROUP BY event_type ORDER BY event_type",
        )
        .expect("prepare lifecycle query")
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
        })
        .expect("query lifecycle events")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("read lifecycle events");
    assert_eq!(
        lifecycle,
        vec![
            ("AGENT_STARTED".to_owned(), 1),
            ("AGENT_STOPPED".to_owned(), 1)
        ]
    );
    let mismatched_pairs: u64 = connection
        .query_row(
            "SELECT COUNT(*) FROM events_local e LEFT JOIN sync_outbox o ON o.event_id = e.event_id WHERE o.event_id IS NULL",
            [],
            |row| row.get(0),
        )
        .expect("count missing outbox rows");
    assert_eq!(mismatched_pairs, 0);
}

#[test]
fn final_heartbeat_cannot_be_overwritten_by_the_periodic_timer() {
    let root = tempfile::tempdir().expect("temporary runtime root");
    let paths = RuntimePaths::under(root.path());
    let mut child = spawn_run(root.path());
    wait_for_status(&paths.status_file);

    signal(&child, "TERM");
    assert!(child.wait_bounded().success());
    let final_bytes = fs::read(&paths.status_file).expect("read final heartbeat");
    let final_status: RuntimeStatusEnvelope =
        serde_json::from_slice(&final_bytes).expect("decode final heartbeat");
    assert_eq!(final_status.agent_status, AgentStatus::Stopped);

    thread::sleep(Duration::from_millis(2_100));
    assert_eq!(
        fs::read(&paths.status_file).expect("reread final heartbeat"),
        final_bytes,
        "no periodic writer may survive final heartbeat"
    );
}

#[test]
fn second_instance_is_rejected_before_an_otherwise_fatal_database_open() {
    let root = tempfile::tempdir().expect("temporary runtime root");
    let paths = RuntimePaths::under(root.path());
    paths.create_securely().expect("secure runtime paths");
    let _lock = SingleInstanceGuard::acquire(&paths.lock_file).expect("own instance lock");
    fs::create_dir(&paths.database_file).expect("make database path fatal to SQLite open");

    let output = Command::new(binary())
        .arg("run")
        .arg("--runtime-root")
        .arg(root.path())
        .output()
        .expect("run duplicate agentd");

    assert_eq!(output.status.code(), Some(4));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "pca-agentd: already running\n"
    );
}

#[test]
fn fatal_startup_retains_crash_marker_for_next_recovery() {
    let root = tempfile::tempdir().expect("temporary runtime root");
    let paths = RuntimePaths::under(root.path());
    paths.create_securely().expect("secure runtime paths");
    fs::create_dir(&paths.database_file).expect("make database path fatal to SQLite open");

    let output = Command::new(binary())
        .arg("run")
        .arg("--runtime-root")
        .arg(root.path())
        .output()
        .expect("run agentd with fatal database path");

    assert_eq!(output.status.code(), Some(5));
    assert_eq!(
        marker_members(&paths).len(),
        1,
        "abnormal startup must remain visible to crash recovery"
    );
}

#[cfg(feature = "process-test-hooks")]
#[test]
fn fatal_failure_after_bridge_start_reaps_child_and_shuts_down_database_owner() {
    let root = tempfile::tempdir().expect("temporary runtime root");
    let paths = RuntimePaths::under(root.path());
    paths.create_securely().expect("secure runtime paths");
    let executable_probe_pid = install_fake_bridge(&paths);
    let canonical_paths = RuntimePaths::under(root.path().canonicalize().expect("canonical root"));
    let bridge_executable = canonical_paths
        .app_dir
        .join("PersonalComputerAgent.app/Contents/Resources/bin/PCAPlatformBridge");
    BridgeSupervisorConfig::new(
        &bridge_executable,
        &canonical_paths.socket_file,
        env!("CARGO_PKG_VERSION"),
    )
    .expect("fake Bridge satisfies production supervisor validation");
    let bridge_pid_file = paths.run_dir.join("fake-bridge.pid");
    let fatal_armed = paths.run_dir.join("fatal-cleanup.armed");
    let fatal_release = paths.run_dir.join("fatal-cleanup.release");
    let cleanup_complete = paths.run_dir.join("fatal-cleanup.complete");
    let mut child = ChildGuard::new(
        Command::new(binary())
            .args(["run", "--runtime-root"])
            .arg(root.path())
            .arg("--process-test-fail-heartbeat-after-bridge-pid")
            .arg(&bridge_pid_file)
            .arg("--process-test-fatal-armed")
            .arg(&fatal_armed)
            .arg("--process-test-fatal-release")
            .arg(&fatal_release)
            .arg("--process-test-cleanup-complete")
            .arg(&cleanup_complete)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn failure-injected agentd"),
    )
    .with_fatal_cleanup(fatal_release.clone(), bridge_pid_file.clone());

    wait_for_file_while_running(&mut child, &fatal_armed);
    let bridge_pid = fs::read_to_string(&bridge_pid_file).expect("read fake Bridge pid");
    assert_ne!(
        bridge_pid.trim(),
        executable_probe_pid.trim(),
        "supervisor must spawn a new fake Bridge child"
    );
    let live_probe = Command::new("/bin/kill")
        .args(["-0", bridge_pid.trim()])
        .output()
        .expect("probe live fake Bridge pid");
    assert!(
        live_probe.status.success(),
        "fake Bridge must be live immediately before fatal cleanup"
    );
    fs::write(&fatal_release, b"release\n").expect("release fatal cleanup injection");
    assert_eq!(child.wait_bounded().code(), Some(5));
    let mut stderr = String::new();
    child
        .child
        .stderr
        .take()
        .expect("fatal agent stderr")
        .read_to_string(&mut stderr)
        .expect("read fatal agent stderr");
    assert!(
        stderr.contains("stage=injected_heartbeat cleanup=none"),
        "fatal report must preserve static primary and cleanup context: {stderr}"
    );
    wait_for_file(&cleanup_complete);

    let bridge_probe = Command::new("/bin/kill")
        .args(["-0", bridge_pid.trim()])
        .output()
        .expect("probe fake Bridge pid");
    assert!(!bridge_probe.status.success(), "fake Bridge child leaked");
    assert!(!paths.socket_file.exists(), "Bridge socket leaked");
    assert_eq!(
        marker_members(&paths).len(),
        1,
        "fatal cleanup must retain crash marker"
    );
    let _lock = SingleInstanceGuard::acquire(&paths.lock_file).expect("agent lock was released");
    let connection = Connection::open(&paths.database_file).expect("reopen database after cleanup");
    connection
        .execute_batch("BEGIN EXCLUSIVE; ROLLBACK;")
        .expect("database owner and transaction were shut down");
}

#[test]
fn forced_kill_leaves_marker_and_next_start_records_crash_recovery() {
    let root = tempfile::tempdir().expect("temporary runtime root");
    let paths = RuntimePaths::under(root.path());
    let mut first = spawn_run(root.path());
    wait_for_status(&paths.status_file);

    signal(&first, "KILL");
    assert!(!first.wait_bounded().success());
    assert_eq!(
        marker_members(&paths).len(),
        1,
        "forced kill must retain marker"
    );
    fs::remove_file(&paths.status_file).expect("remove stale killed heartbeat");

    let mut recovered = spawn_run(root.path());
    let recovered_status = wait_for_status(&paths.status_file);
    assert!(recovered_status.local_healthy);
    signal(&recovered, "TERM");
    assert!(recovered.wait_bounded().success());

    let connection = Connection::open(&paths.database_file).expect("open runtime database");
    let crash_events: u64 = connection
        .query_row(
            "SELECT COUNT(*) FROM events_local WHERE event_type = 'AGENT_CRASH_RECOVERED'",
            [],
            |row| row.get(0),
        )
        .expect("count crash recovery events");
    assert_eq!(crash_events, 1);
}

#[test]
fn stale_health_and_unavailable_prepare_sleep_have_meaningful_exit_codes() {
    let root = tempfile::tempdir().expect("temporary runtime root");
    let paths = RuntimePaths::under(root.path());
    paths.create_securely().expect("secure runtime paths");
    let stale = RuntimeStatusEnvelope {
        agent_status: AgentStatus::Unpaired,
        bridge_status: BridgeStatus::Degraded,
        local_healthy: true,
        heartbeat_at: "2020-01-01T00:00:00Z".to_owned(),
        process_id: 1,
        app_version: "0.0.0".to_owned(),
        schema_version: 1,
    };
    fs::write(
        &paths.status_file,
        serde_json::to_vec(&stale).expect("serialize stale status"),
    )
    .expect("write stale status");

    let health = Command::new(binary())
        .args(["health", "--runtime-root"])
        .arg(root.path())
        .output()
        .expect("run stale health");
    assert_eq!(health.status.code(), Some(1));

    let prepare = Command::new(binary())
        .args(["prepare-sleep", "--runtime-root"])
        .arg(root.path())
        .output()
        .expect("run prepare-sleep");
    assert_eq!(prepare.status.code(), Some(3));
    assert_eq!(
        String::from_utf8_lossy(&prepare.stderr),
        "pca-agentd: live prepare-sleep control is unsupported by Bridge protocol v1\n"
    );
}

#[cfg(not(feature = "process-test-hooks"))]
#[test]
fn default_binary_does_not_recognize_process_test_hook_flags() {
    let root = tempfile::tempdir().expect("temporary runtime root");
    for flag in [
        "--process-test-event-barrier-ready",
        "--process-test-event-barrier-release",
        "--process-test-fail-heartbeat-after-bridge-pid",
        "--process-test-fatal-armed",
        "--process-test-fatal-release",
        "--process-test-cleanup-complete",
    ] {
        let output = Command::new(binary())
            .args(["run", "--runtime-root"])
            .arg(root.path())
            .arg(flag)
            .arg(root.path().join("hidden-hook-value"))
            .output()
            .expect("run default binary with hidden flag");

        assert_eq!(output.status.code(), Some(2), "default accepted {flag}");
    }
}
