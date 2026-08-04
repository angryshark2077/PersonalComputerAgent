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

const PROCESS_TIMEOUT: Duration = Duration::from_secs(5);
const WORKSPACE_ID: &str = "11111111-1111-4111-8111-111111111111";
const DEVICE_ID: &str = "22222222-2222-4222-8222-222222222222";

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

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "barrier was not reached in five seconds"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn process_kill_between_event_and_outbox_statements_never_commits_a_partial_pair() {
    let root = tempfile::tempdir().expect("temporary runtime root");
    let paths = RuntimePaths::under(root.path());
    let ready = root.path().join("Run/event-inserted.ready");
    let release = root.path().join("Run/event-inserted.release");
    let mut child = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_pca-agentd"))
            .args(["run", "--runtime-root"])
            .arg(root.path())
            .args(["--process-test-workspace-id", WORKSPACE_ID])
            .args(["--process-test-device-id", DEVICE_ID])
            .arg("--process-test-event-barrier-ready")
            .arg(&ready)
            .arg("--process-test-event-barrier-release")
            .arg(&release)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn barrier agentd"),
    );

    wait_for_file(&ready);
    let killed = Command::new("/bin/kill")
        .arg("-KILL")
        .arg(child.0.id().to_string())
        .status()
        .expect("kill barrier child");
    assert!(killed.success());
    assert!(!child.wait_bounded().success());

    let connection = Connection::open(&paths.database_file).expect("reopen killed database");
    let counts = connection
        .query_row(
            "SELECT (SELECT COUNT(*) FROM events_local), (SELECT COUNT(*) FROM sync_outbox)",
            [],
            |row| Ok((row.get::<_, u64>(0)?, row.get::<_, u64>(1)?)),
        )
        .expect("count durable rows after process kill");
    eprintln!("durability counts after kill: {counts:?}");
    assert!(
        matches!(counts, (0, 0) | (1, 1)),
        "partial durable pair: {counts:?}"
    );
    assert_ne!(counts, (1, 0));

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
