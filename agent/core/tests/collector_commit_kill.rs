#![cfg(feature = "process-test-hooks")]

use std::{
    fs,
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use pca_agent_runtime::RuntimePaths;
use rusqlite::Connection;

const WORKSPACE_ID: &str = "018f3f4a-2d9b-7d21-a310-2c49d9b43c13";
const DEVICE_ID: &str = "018f3f4a-2d9b-7d21-a310-2c49d9b43c14";
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

fn wait_for_file_while_running(child: &mut ChildGuard, path: &Path) {
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    while !path.exists() {
        if let Some(status) = child.0.try_wait().expect("poll child") {
            panic!("child exited before Collector barrier: {status}");
        }
        assert!(
            Instant::now() < deadline,
            "Collector barrier was not reached in five seconds"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn process_kill_before_collector_state_upsert_never_commits_a_partial_registry_transition() {
    let root = tempfile::tempdir().expect("temporary runtime root");
    let paths = RuntimePaths::under(root.path());
    let ready = root.path().join("Run/collector-commit.ready");
    let release = root.path().join("Run/collector-commit.release");
    let mut child = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_pca-agentd"))
            .args(["run", "--runtime-root"])
            .arg(root.path())
            .args(["--process-test-workspace-id", WORKSPACE_ID])
            .args(["--process-test-device-id", DEVICE_ID])
            .arg("--process-test-collector-barrier-ready")
            .arg(&ready)
            .arg("--process-test-collector-barrier-release")
            .arg(&release)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn Collector barrier agentd"),
    );

    wait_for_file_while_running(&mut child, &ready);
    let killed = Command::new("/bin/kill")
        .arg("-KILL")
        .arg(child.0.id().to_string())
        .status()
        .expect("kill Collector barrier child");
    assert!(killed.success());
    assert!(!child.wait_bounded().success());

    let connection = Connection::open(&paths.database_file).expect("reopen killed database");
    let counts = connection
        .query_row(
            "SELECT
                (SELECT COUNT(*) FROM events_local
                 WHERE event_type = 'collector.status_changed'
                   AND source = 'collector.registry'),
                (SELECT COUNT(*) FROM sync_outbox o
                 JOIN events_local e ON e.event_id = o.event_id
                 WHERE e.event_type = 'collector.status_changed'
                   AND e.source = 'collector.registry'),
                (SELECT COUNT(*) FROM collector_states WHERE collector_key = 'system')",
            [],
            |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, u64>(2)?,
                ))
            },
        )
        .expect("count Collector transaction rows");
    assert!(
        matches!(counts, (0, 0, 0) | (1, 1, 1)),
        "partial collector transaction: {counts:?}"
    );

    let markers = fs::read_dir(&paths.data_dir)
        .expect("read data directory")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("crash-marker.json."))
        })
        .count();
    assert_eq!(markers, 1, "forced kill must retain crash marker");
}
