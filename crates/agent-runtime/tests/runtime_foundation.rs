use std::{
    collections::BTreeSet, env, fs, os::unix::fs::PermissionsExt, path::Path, process::Command,
};

use pca_agent_runtime::{
    CrashMarkerGuard, LocalHeartbeatWriter, RuntimeError, RuntimePaths, RuntimeStateMachine,
    SingleInstanceGuard,
};
use pca_domain::{AgentStatus, BridgeStatus, RuntimeStatusEnvelope};

fn mode(path: &Path) -> u32 {
    fs::metadata(path)
        .expect("path metadata")
        .permissions()
        .mode()
}

fn status(heartbeat_at: &str) -> RuntimeStatusEnvelope {
    RuntimeStatusEnvelope {
        agent_status: AgentStatus::Unpaired,
        bridge_status: BridgeStatus::Ready,
        local_healthy: true,
        heartbeat_at: heartbeat_at.to_owned(),
        process_id: 4242,
        app_version: "0.0.0-s1a".to_owned(),
        schema_version: 1,
    }
}

#[test]
fn runtime_paths_create_the_fixed_secure_layout() {
    let temporary_root = tempfile::tempdir().expect("temporary root");
    let paths = RuntimePaths::under(temporary_root.path());

    paths.create_securely().expect("secure layout");

    assert_eq!(paths.root, temporary_root.path());
    assert_eq!(paths.app_dir, temporary_root.path().join("App"));
    assert_eq!(paths.data_dir, temporary_root.path().join("Data"));
    assert_eq!(paths.run_dir, temporary_root.path().join("Run"));
    assert_eq!(paths.database_file, paths.data_dir.join("agent.sqlite3"));
    assert_eq!(
        paths.crash_marker_file,
        paths.data_dir.join("crash-marker.json")
    );
    assert_eq!(paths.lock_file, paths.run_dir.join("agent.lock"));
    assert_eq!(paths.socket_file, paths.run_dir.join("bridge.sock"));
    assert_eq!(paths.status_file, paths.run_dir.join("runtime-status.json"));
    assert_eq!(mode(&paths.root) & 0o777, 0o700);
    assert_eq!(mode(&paths.data_dir) & 0o777, 0o700);
    assert_eq!(mode(&paths.run_dir) & 0o777, 0o700);
}

#[test]
fn current_user_paths_use_only_the_fixed_application_support_root() {
    let paths = RuntimePaths::for_current_user().expect("current-user root");
    let expected = env::var_os("HOME")
        .expect("HOME is available")
        .into_string()
        .expect("HOME is UTF-8");

    assert_eq!(
        paths.root,
        Path::new(&expected).join("Library/Application Support/PersonalComputerAgent")
    );
}

#[test]
fn instance_lock_rejects_a_second_owner_until_the_first_is_dropped() {
    let temporary_root = tempfile::tempdir().expect("temporary root");
    let paths = RuntimePaths::under(temporary_root.path());
    paths.create_securely().expect("secure layout");

    let first = SingleInstanceGuard::acquire(&paths.lock_file).expect("first lock");
    assert!(matches!(
        SingleInstanceGuard::acquire(&paths.lock_file),
        Err(RuntimeError::AlreadyRunning)
    ));
    drop(first);
    SingleInstanceGuard::acquire(&paths.lock_file).expect("lock released with guard");
    assert_eq!(mode(&paths.lock_file) & 0o777, 0o600);
}

#[test]
fn instance_lock_rejects_a_symlink_instead_of_following_it() {
    let temporary_root = tempfile::tempdir().expect("temporary root");
    let paths = RuntimePaths::under(temporary_root.path());
    paths.create_securely().expect("secure layout");
    let target = temporary_root.path().join("lock-target");
    fs::write(&target, b"not a lock").expect("target file");
    std::os::unix::fs::symlink(&target, &paths.lock_file).expect("lock symlink");

    assert!(matches!(
        SingleInstanceGuard::acquire(&paths.lock_file),
        Err(RuntimeError::UnsafePath { .. })
    ));
    assert_eq!(
        fs::read(&target).expect("target remains readable"),
        b"not a lock"
    );
}

#[test]
fn state_machine_allows_only_explicit_agent_lifecycle_edges() {
    let mut state = RuntimeStateMachine::starting();

    state
        .transition_agent(AgentStatus::Unpaired)
        .expect("initializing -> unpaired");
    state
        .transition_agent(AgentStatus::WaitingPermission)
        .expect("unpaired -> waiting permission");
    state
        .transition_agent(AgentStatus::Running)
        .expect("waiting permission -> running");
    state
        .transition_agent(AgentStatus::Sleeping)
        .expect("running -> sleeping");
    state
        .transition_agent(AgentStatus::Running)
        .expect("sleeping -> running");
    state
        .transition_agent(AgentStatus::Updating)
        .expect("running -> updating");
    state
        .transition_agent(AgentStatus::Initializing)
        .expect("updating -> initializing");
    state
        .transition_agent(AgentStatus::Repair)
        .expect("initializing -> repair");
    state
        .transition_agent(AgentStatus::Stopped)
        .expect("repair -> stopped");

    assert_eq!(state.agent_status(), AgentStatus::Stopped);
    assert!(matches!(
        state.transition_agent(AgentStatus::Running),
        Err(RuntimeError::IllegalAgentTransition {
            from: AgentStatus::Stopped,
            to: AgentStatus::Running,
        })
    ));
}

#[test]
fn state_machine_rejects_skipping_agent_initialization() {
    let mut state = RuntimeStateMachine::starting();

    assert!(matches!(
        state.transition_agent(AgentStatus::Running),
        Err(RuntimeError::IllegalAgentTransition {
            from: AgentStatus::Initializing,
            to: AgentStatus::Running,
        })
    ));
}

#[test]
fn state_machine_allows_only_explicit_bridge_lifecycle_edges() {
    let mut state = RuntimeStateMachine::starting();

    state
        .transition_bridge(BridgeStatus::Handshaking)
        .expect("disconnected -> handshaking");
    state
        .transition_bridge(BridgeStatus::Ready)
        .expect("handshaking -> ready");
    state
        .transition_bridge(BridgeStatus::Degraded)
        .expect("ready -> degraded");
    state
        .transition_bridge(BridgeStatus::Handshaking)
        .expect("degraded -> handshaking");
    state
        .transition_bridge(BridgeStatus::Incompatible)
        .expect("handshaking -> incompatible");
    state
        .transition_bridge(BridgeStatus::Stopped)
        .expect("incompatible -> stopped");

    assert_eq!(state.bridge_status(), BridgeStatus::Stopped);
    assert!(matches!(
        state.transition_bridge(BridgeStatus::Ready),
        Err(RuntimeError::IllegalBridgeTransition {
            from: BridgeStatus::Stopped,
            to: BridgeStatus::Ready,
        })
    ));
}

#[test]
fn crash_marker_child_process() {
    let Some(marker_path) = env::var_os("PCA_CRASH_MARKER_CHILD") else {
        return;
    };
    let _guard = CrashMarkerGuard::activate(Path::new(&marker_path)).expect("child marker");

    match env::var("PCA_CRASH_MARKER_CHILD_MODE").as_deref() {
        Ok("exit") => std::process::exit(0),
        Ok("panic") => panic!("intentional child panic"),
        _ => panic!("unexpected crash-marker child mode"),
    }
}

#[test]
fn crash_marker_reports_an_unclean_previous_process_exit() {
    let temporary_root = tempfile::tempdir().expect("temporary root");
    let paths = RuntimePaths::under(temporary_root.path());
    paths.create_securely().expect("secure layout");

    let result = Command::new(env::current_exe().expect("test executable"))
        .arg("crash_marker_child_process")
        .arg("--exact")
        .env("PCA_CRASH_MARKER_CHILD", &paths.crash_marker_file)
        .env("PCA_CRASH_MARKER_CHILD_MODE", "exit")
        .status()
        .expect("child test process");
    assert!(result.success(), "child process should exit cleanly");
    assert!(paths.crash_marker_file.exists(), "child marker remains");

    let guard = CrashMarkerGuard::activate(&paths.crash_marker_file).expect("recovery marker");
    assert!(guard.previous_exit_was_unclean());
    guard.complete_cleanly().expect("clean completion");
    assert!(
        !paths.crash_marker_file.exists(),
        "clean completion removes marker"
    );
}

#[test]
fn crash_marker_is_retained_when_a_child_panics_while_unwinding() {
    let temporary_root = tempfile::tempdir().expect("temporary root");
    let paths = RuntimePaths::under(temporary_root.path());
    paths.create_securely().expect("secure layout");

    let result = Command::new(env::current_exe().expect("test executable"))
        .arg("crash_marker_child_process")
        .arg("--exact")
        .env("PCA_CRASH_MARKER_CHILD", &paths.crash_marker_file)
        .env("PCA_CRASH_MARKER_CHILD_MODE", "panic")
        .status()
        .expect("child test process");
    assert!(!result.success(), "panicking child must fail");
    assert!(paths.crash_marker_file.exists(), "panic marker remains");

    let guard = CrashMarkerGuard::activate(&paths.crash_marker_file).expect("recovery marker");
    assert!(guard.previous_exit_was_unclean());
    guard
        .complete_cleanly()
        .expect("recovered clean completion");
    assert!(!paths.crash_marker_file.exists(), "recovery removes marker");
}

#[test]
fn stale_crash_marker_guard_cannot_remove_a_newer_activation_marker() {
    let temporary_root = tempfile::tempdir().expect("temporary root");
    let paths = RuntimePaths::under(temporary_root.path());
    paths.create_securely().expect("secure layout");

    let stale = CrashMarkerGuard::activate(&paths.crash_marker_file).expect("first marker");
    let current = CrashMarkerGuard::activate(&paths.crash_marker_file).expect("newer marker");

    assert!(matches!(
        stale.complete_cleanly(),
        Err(RuntimeError::CrashMarkerOwnershipLost { .. })
    ));
    assert!(paths.crash_marker_file.exists(), "newer marker remains");
    current
        .complete_cleanly()
        .expect("current clean completion");
    assert!(!paths.crash_marker_file.exists(), "current marker removed");
}

#[test]
fn stale_crash_marker_drop_cannot_remove_a_newer_activation_marker() {
    let temporary_root = tempfile::tempdir().expect("temporary root");
    let paths = RuntimePaths::under(temporary_root.path());
    paths.create_securely().expect("secure layout");

    let stale = CrashMarkerGuard::activate(&paths.crash_marker_file).expect("first marker");
    let current = CrashMarkerGuard::activate(&paths.crash_marker_file).expect("newer marker");
    drop(stale);

    assert!(paths.crash_marker_file.exists(), "newer marker remains");
    current
        .complete_cleanly()
        .expect("current clean completion");
}

#[test]
fn crash_marker_drop_removes_its_marker_without_a_panic() {
    let temporary_root = tempfile::tempdir().expect("temporary root");
    let paths = RuntimePaths::under(temporary_root.path());
    paths.create_securely().expect("secure layout");

    let guard = CrashMarkerGuard::activate(&paths.crash_marker_file).expect("marker");
    drop(guard);
    assert!(
        !paths.crash_marker_file.exists(),
        "best-effort drop removed marker"
    );
}

#[test]
fn heartbeat_replaces_status_atomically_without_temp_files_or_secrets() {
    let temporary_root = tempfile::tempdir().expect("temporary root");
    let paths = RuntimePaths::under(temporary_root.path());
    paths.create_securely().expect("secure layout");
    fs::write(&paths.status_file, b"old status").expect("old status");

    LocalHeartbeatWriter::new(&paths.status_file)
        .write(&status("2026-07-31T00:00:02Z"))
        .expect("atomic heartbeat");

    let serialized = fs::read_to_string(&paths.status_file).expect("status file");
    let value: serde_json::Value = serde_json::from_str(&serialized).expect("status JSON");
    let keys = value
        .as_object()
        .expect("status object")
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    assert_eq!(
        keys,
        BTreeSet::from([
            "agent_status".to_owned(),
            "app_version".to_owned(),
            "bridge_status".to_owned(),
            "heartbeat_at".to_owned(),
            "local_healthy".to_owned(),
            "process_id".to_owned(),
            "schema_version".to_owned(),
        ])
    );
    assert_eq!(value["heartbeat_at"], "2026-07-31T00:00:02Z");
    assert_eq!(mode(&paths.status_file) & 0o777, 0o600);
    let temporary_files = fs::read_dir(&paths.run_dir)
        .expect("run directory")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .contains("runtime-status.tmp")
        })
        .count();
    assert_eq!(
        temporary_files, 0,
        "successful writes clean up temporary files"
    );
}
