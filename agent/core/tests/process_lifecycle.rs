use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use pca_agent_runtime::{RuntimePaths, SingleInstanceGuard};
use pca_domain::{AgentStatus, BridgeStatus, RuntimeStatusEnvelope};
use rusqlite::Connection;

const PROCESS_TIMEOUT: Duration = Duration::from_secs(5);

struct ChildGuard(Child);

impl ChildGuard {
    fn wait_bounded(&mut self) -> ExitStatus {
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        loop {
            if let Some(status) = self.0.try_wait().expect("poll child") {
                return status;
            }
            if Instant::now() >= deadline {
                let _ = self.0.kill();
                let _ = self.0.wait();
                panic!("child process exceeded five-second bound");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_pca-agentd")
}

fn spawn_run(root: &Path) -> ChildGuard {
    ChildGuard(
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

fn signal(child: &ChildGuard, name: &str) {
    let result = Command::new("/bin/kill")
        .arg(format!("-{name}"))
        .arg(child.0.id().to_string())
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
    let output = Command::new(binary())
        .args(["run", "--runtime-root"])
        .arg(root.path())
        .arg("--process-test-event-barrier-ready")
        .arg(root.path().join("ready"))
        .arg("--process-test-event-barrier-release")
        .arg(root.path().join("release"))
        .output()
        .expect("run default binary with hidden flags");

    assert_eq!(output.status.code(), Some(2));
}
