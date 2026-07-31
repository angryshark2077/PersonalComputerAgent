#![cfg(feature = "process-test-hooks")]

use std::{
    fs,
    io::Read,
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use pca_agent_runtime::RuntimePaths;
use pca_domain::{AgentStatus, RuntimeStatusEnvelope};
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

fn signal(child: &ChildGuard, name: &str) {
    let status = Command::new("/bin/kill")
        .arg(format!("-{name}"))
        .arg(child.0.id().to_string())
        .status()
        .expect("signal child");
    assert!(status.success());
}

fn spawn(root: &Path, paired: bool) -> ChildGuard {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pca-agentd"));
    command.args(["run", "--runtime-root"]).arg(root);
    if paired {
        command
            .args(["--process-test-workspace-id", WORKSPACE_ID])
            .args(["--process-test-device-id", DEVICE_ID]);
    }
    ChildGuard(
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn agentd"),
    )
}

fn wait_for_samples(path: &Path) -> (u64, u64, u64, Vec<String>) {
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    loop {
        if let Ok(connection) = Connection::open(path) {
            let result = connection.query_row(
                "SELECT
                    (SELECT COUNT(*) FROM collector_states
                     WHERE collector_key = 'system' AND status = 'running'),
                    (SELECT COUNT(*) FROM events_local
                     WHERE event_type = 'system.metric_sampled'
                       AND json_extract(payload_json, '$.metric_group') = 'cpu_memory'),
                    (SELECT COUNT(*) FROM events_local
                     WHERE event_type = 'system.metric_sampled'
                       AND json_extract(payload_json, '$.metric_group') = 'disk')",
                [],
                |row| {
                    Ok((
                        row.get::<_, u64>(0)?,
                        row.get::<_, u64>(1)?,
                        row.get::<_, u64>(2)?,
                    ))
                },
            );
            if let Ok((running, cpu, disk)) = result {
                if running == 1 && cpu >= 1 && disk >= 1 {
                    let mismatches = connection
                        .query_row(
                            "SELECT COUNT(*)
                             FROM events_local e
                             LEFT JOIN sync_outbox o ON o.event_id = e.event_id
                             WHERE e.event_type IN (
                                 'collector.status_changed',
                                 'system.metric_sampled',
                                 'system.health_changed'
                             )
                               AND o.event_id IS NULL",
                            [],
                            |row| row.get::<_, u64>(0),
                        )
                        .expect("count missing Outbox rows");
                    let payloads = connection
                        .prepare(
                            "SELECT payload_json
                             FROM events_local
                             WHERE event_type = 'system.metric_sampled'
                             ORDER BY created_at_ms",
                        )
                        .expect("prepare payload query")
                        .query_map([], |row| row.get::<_, String>(0))
                        .expect("query payloads")
                        .collect::<rusqlite::Result<Vec<_>>>()
                        .expect("read payloads");
                    return (cpu, disk, mismatches, payloads);
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "System samples did not become durable in five seconds"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn real_process_samples_system_metrics_then_disables_when_restarted_unpaired() {
    let root = tempfile::tempdir().expect("temporary runtime root");
    let paths = RuntimePaths::under(root.path());
    let mut paired = spawn(root.path(), true);
    let (cpu_before, disk_before, mismatches, payloads) = wait_for_samples(&paths.database_file);

    assert_eq!(mismatches, 0);
    let root_text = root.path().to_string_lossy();
    let pid_text = paired.0.id().to_string();
    for payload in payloads {
        assert!(!payload.contains(root_text.as_ref()));
        assert!(!payload.contains(&pid_text));
    }

    signal(&paired, "TERM");
    assert!(paired.wait_bounded().success());

    let mut unpaired = spawn(root.path(), false);
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    loop {
        if let Ok(connection) = Connection::open(&paths.database_file) {
            let state = connection.query_row(
                "SELECT status FROM collector_states WHERE collector_key = 'system'",
                [],
                |row| row.get::<_, String>(0),
            );
            if state.as_deref() == Ok("disabled") {
                let counts = connection
                    .query_row(
                        "SELECT
                            SUM(json_extract(payload_json, '$.metric_group') = 'cpu_memory'),
                            SUM(json_extract(payload_json, '$.metric_group') = 'disk')
                         FROM events_local
                         WHERE event_type = 'system.metric_sampled'",
                        [],
                        |row| Ok((row.get::<_, u64>(0)?, row.get::<_, u64>(1)?)),
                    )
                    .expect("count metrics after unpaired restart");
                assert_eq!(counts, (cpu_before, disk_before));
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "unpaired restart did not persist disabled state"
        );
        thread::sleep(Duration::from_millis(10));
    }

    let deadline = Instant::now() + PROCESS_TIMEOUT;
    loop {
        if let Ok(bytes) = fs::read(&paths.status_file) {
            if serde_json::from_slice::<RuntimeStatusEnvelope>(&bytes)
                .is_ok_and(|status| status.agent_status == AgentStatus::Unpaired)
            {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "unpaired restart did not finish runtime startup"
        );
        thread::sleep(Duration::from_millis(10));
    }

    signal(&unpaired, "TERM");
    let status = unpaired.wait_bounded();
    let mut stderr = String::new();
    unpaired
        .0
        .stderr
        .take()
        .expect("unpaired stderr")
        .read_to_string(&mut stderr)
        .expect("read unpaired stderr");
    assert!(status.success(), "unpaired exit {status}: {stderr}");
    assert!(fs::read_dir(&paths.data_dir)
        .expect("read data dir")
        .filter_map(Result::ok)
        .all(|entry| !entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with("crash-marker.json."))));
}
