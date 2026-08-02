use std::{
    collections::VecDeque,
    fs,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use pca_agentd::communication::{
    CommunicationControl, CommunicationIdentity, CommunicationRuntime, CommunicationRuntimeError,
    UnavailableCommunicationProviderFactory, OUTBOX_HIGH_WATER, OUTBOX_LOW_WATER,
    SPOOL_HARD_LIMIT_BYTES, SPOOL_RESUME_BELOW_BYTES,
};
use pca_db_local::DbActorHandle;
use pca_domain::{
    CommunicationAttachment, CommunicationMessageRecorded, CommunicationMessageRecordedInput,
    ConversationScope, Direction, DomainError, MessageKind,
};
use pca_provider_contracts::{
    CommunicationPollFuture, CommunicationProvider, CommunicationProviderFactory,
    CommunicationProviderFuture, CompletedMediaSource, NormalizedCommunicationRecord,
    ProviderStatus,
};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::sync::Notify;

type ProviderResult = Result<Vec<NormalizedCommunicationRecord>, DomainError>;

#[derive(Default)]
struct RecordingState {
    factory_calls: AtomicUsize,
    discover_calls: AtomicUsize,
    poll_calls: AtomicUsize,
    stop_calls: AtomicUsize,
    poll_dropped: AtomicBool,
    block_poll: AtomicBool,
    poll_entered: Notify,
    discover_results: Mutex<VecDeque<Result<(), DomainError>>>,
    poll_results: Mutex<VecDeque<ProviderResult>>,
}

struct RecordingFactory {
    state: Arc<RecordingState>,
}

impl CommunicationProviderFactory for RecordingFactory {
    fn create(&self) -> Result<Box<dyn CommunicationProvider>, DomainError> {
        self.state.factory_calls.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(RecordingProvider {
            state: Arc::clone(&self.state),
            status: ProviderStatus::WaitingSource,
        }))
    }
}

struct RecordingProvider {
    state: Arc<RecordingState>,
    status: ProviderStatus,
}

struct PollDropGuard(Arc<RecordingState>);

impl Drop for PollDropGuard {
    fn drop(&mut self) {
        self.0.poll_dropped.store(true, Ordering::SeqCst);
    }
}

impl CommunicationProvider for RecordingProvider {
    fn key(&self) -> &'static str {
        "communication.wechat"
    }

    fn status(&self) -> ProviderStatus {
        self.status
    }

    fn discover(&mut self) -> CommunicationProviderFuture<'_> {
        Box::pin(async move {
            self.state.discover_calls.fetch_add(1, Ordering::SeqCst);
            let result = self
                .state
                .discover_results
                .lock()
                .expect("discover plan")
                .pop_front()
                .unwrap_or(Ok(()));
            self.status = if result.is_ok() {
                ProviderStatus::Active
            } else {
                ProviderStatus::Degraded
            };
            result
        })
    }

    fn poll_once(&mut self) -> CommunicationPollFuture<'_> {
        Box::pin(async move {
            self.state.poll_calls.fetch_add(1, Ordering::SeqCst);
            if self.state.block_poll.load(Ordering::SeqCst) {
                let _guard = PollDropGuard(Arc::clone(&self.state));
                self.state.poll_entered.notify_waiters();
                std::future::pending::<()>().await;
            }
            self.state
                .poll_results
                .lock()
                .expect("poll plan")
                .pop_front()
                .unwrap_or(Ok(Vec::new()))
        })
    }

    fn stop(&mut self) -> Result<(), DomainError> {
        self.state.stop_calls.fetch_add(1, Ordering::SeqCst);
        self.status = ProviderStatus::Disabled;
        Ok(())
    }
}

struct Harness {
    directory: TempDir,
    database_path: PathBuf,
    database: Arc<DbActorHandle>,
    state: Arc<RecordingState>,
}

impl Harness {
    async fn new() -> Self {
        let directory = TempDir::new().expect("temporary runtime directory");
        let database_path = directory.path().join("agent.sqlite3");
        let database = Arc::new(
            DbActorHandle::open(&database_path, "test")
                .await
                .expect("open database"),
        );
        Self {
            directory,
            database_path,
            database,
            state: Arc::new(RecordingState::default()),
        }
    }

    fn factory(&self) -> Arc<dyn CommunicationProviderFactory> {
        Arc::new(RecordingFactory {
            state: Arc::clone(&self.state),
        })
    }

    async fn start(&self, control: CommunicationControl) -> CommunicationRuntime {
        CommunicationRuntime::start(
            Arc::clone(&self.database),
            self.database_path.clone(),
            self.factory(),
            control,
        )
        .await
        .expect("start communication runtime")
    }

    async fn assert_no_communication_commit(&self) {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        let connection = Connection::open(&self.database_path).expect("inspect database");
        let counts = [
            "events_local",
            "sync_outbox",
            "communication_messages",
            "communication_cursors",
            "attachment_spool",
        ]
        .map(|table| {
            connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get::<_, u64>(0)
                })
                .expect("count rows")
        });
        assert_eq!(counts, [0, 0, 0, 0, 0]);
    }
}

fn identity() -> CommunicationIdentity {
    CommunicationIdentity::try_new(
        "22222222-2222-4222-8222-222222222222",
        "11111111-1111-4111-8111-111111111111",
    )
    .expect("valid identity")
}

fn enabled(revision: u64) -> CommunicationControl {
    CommunicationControl::paired(identity(), revision, true).expect("valid enabled control")
}

fn disabled(revision: u64) -> CommunicationControl {
    CommunicationControl::paired(identity(), revision, false).expect("valid disabled control")
}

fn text_record(sequence: u64) -> NormalizedCommunicationRecord {
    NormalizedCommunicationRecord::try_new(
        "wechat-account-1".to_owned(),
        sequence,
        CommunicationMessageRecorded::try_new(CommunicationMessageRecordedInput {
            message_id: format!("message-{sequence}"),
            conversation_id: "conversation-1".to_owned(),
            source_key: format!("opaque-source-key-{sequence}"),
            occurred_at: "2026-08-02T00:00:00Z".to_owned(),
            direction: Direction::Outgoing,
            kind: MessageKind::Text,
            conversation: ConversationScope::Direct,
            text: Some("private body".to_owned()),
            attachments: Vec::new(),
        })
        .expect("valid text message"),
        Vec::new(),
    )
    .expect("valid normalized text record")
}

fn media_record(path: &Path, declared: &[u8], sequence: u64) -> NormalizedCommunicationRecord {
    let sha256 = format!("{:x}", Sha256::digest(declared));
    let attachment = CommunicationAttachment::try_new(
        format!("attachment-{sequence}"),
        MessageKind::Video,
        sha256,
        u64::try_from(declared.len()).expect("fixture size"),
        "video/mp4".to_owned(),
    )
    .expect("valid attachment");
    let source =
        CompletedMediaSource::try_new(attachment.attachment_id().to_owned(), path.to_owned())
            .expect("valid completed source descriptor");
    NormalizedCommunicationRecord::try_new(
        "wechat-account-1".to_owned(),
        sequence,
        CommunicationMessageRecorded::try_new(CommunicationMessageRecordedInput {
            message_id: format!("message-{sequence}"),
            conversation_id: "conversation-1".to_owned(),
            source_key: format!("opaque-source-key-{sequence}"),
            occurred_at: "2026-08-02T00:00:00Z".to_owned(),
            direction: Direction::Incoming,
            kind: MessageKind::Video,
            conversation: ConversationScope::Direct,
            text: None,
            attachments: vec![attachment],
        })
        .expect("valid media message"),
        vec![source],
    )
    .expect("valid normalized media record")
}

async fn wait_for(counter: &AtomicUsize, expected: usize) {
    for _ in 0..10_000 {
        if counter.load(Ordering::SeqCst) >= expected {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("counter did not reach {expected}");
}

#[tokio::test]
async fn disabled_unpaired_and_stale_controls_never_call_source_probe() {
    let harness = Harness::new().await;
    let runtime = harness.start(CommunicationControl::unpaired()).await;
    runtime
        .apply_control(disabled(1))
        .await
        .expect("new disabled revision applies");
    assert!(matches!(
        runtime.apply_control(enabled(1)).await,
        Err(CommunicationRuntimeError::StaleControl)
    ));
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert_eq!(harness.state.factory_calls.load(Ordering::SeqCst), 0);
    assert_eq!(harness.state.discover_calls.load(Ordering::SeqCst), 0);
    runtime.shutdown().await.expect("joined shutdown");
    harness.assert_no_communication_commit().await;
}

#[tokio::test]
async fn unavailable_production_factory_fails_closed_without_spin_or_commit() {
    let harness = Harness::new().await;
    let runtime = CommunicationRuntime::start(
        Arc::clone(&harness.database),
        harness.database_path.clone(),
        Arc::new(UnavailableCommunicationProviderFactory),
        enabled(1),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let state = harness
                .database
                .load_collector_states()
                .await
                .unwrap()
                .into_iter()
                .find(|state| state.collector_key == "communication.wechat");
            if state.is_some_and(|state| {
                state.status == pca_domain::CollectorStatus::Unsupported
                    && state.last_error_code.as_deref() == Some("WECHAT_CAPABILITY_UNAVAILABLE")
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("unavailable factory reaches terminal unsupported state");
    harness.assert_no_communication_commit().await;
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancellation_joins_the_poll_and_prevents_a_post_cancel_commit() {
    let harness = Harness::new().await;
    harness.state.block_poll.store(true, Ordering::SeqCst);
    harness
        .state
        .poll_results
        .lock()
        .unwrap()
        .push_back(Ok(vec![text_record(1)]));
    let runtime = harness.start(enabled(1)).await;
    harness.state.poll_entered.notified().await;

    tokio::time::timeout(Duration::from_secs(1), runtime.shutdown())
        .await
        .expect("shutdown joins without hanging")
        .expect("clean shutdown");
    assert!(harness.state.poll_dropped.load(Ordering::SeqCst));
    assert_eq!(harness.state.stop_calls.load(Ordering::SeqCst), 1);
    harness.assert_no_communication_commit().await;
}

#[tokio::test(start_paused = true)]
async fn retryable_provider_failure_uses_bounded_backoff_but_terminal_failure_does_not_restart() {
    let retryable = Harness::new().await;
    retryable
        .state
        .discover_results
        .lock()
        .unwrap()
        .push_back(Err(DomainError::new(
            "WECHAT_WAITING_SOURCE",
            "redacted",
            true,
        )));
    let retry_runtime = retryable.start(enabled(1)).await;
    wait_for(&retryable.state.discover_calls, 1).await;
    wait_for(&retryable.state.stop_calls, 1).await;
    settle().await;
    tokio::time::advance(Duration::from_secs(29)).await;
    assert_eq!(retryable.state.factory_calls.load(Ordering::SeqCst), 1);
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    wait_for(&retryable.state.factory_calls, 2).await;
    retry_runtime.shutdown().await.unwrap();

    let terminal = Harness::new().await;
    terminal
        .state
        .discover_results
        .lock()
        .unwrap()
        .push_back(Err(DomainError::new(
            "WECHAT_CAPABILITY_UNAVAILABLE",
            "redacted",
            false,
        )));
    let terminal_runtime = terminal.start(enabled(1)).await;
    wait_for(&terminal.state.discover_calls, 1).await;
    tokio::time::advance(Duration::from_hours(1)).await;
    assert_eq!(terminal.state.factory_calls.load(Ordering::SeqCst), 1);
    assert_eq!(terminal.state.discover_calls.load(Ordering::SeqCst), 1);
    terminal_runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn completed_media_is_streamed_to_its_hash_name_before_atomic_commit() {
    let harness = Harness::new().await;
    let bytes = b"completed-media";
    let source = harness.directory.path().join("source.mp4");
    fs::write(&source, bytes).expect("write source fixture");
    harness
        .state
        .poll_results
        .lock()
        .unwrap()
        .push_back(Ok(vec![media_record(
            &source.canonicalize().expect("canonical source fixture"),
            bytes,
            1,
        )]));
    let runtime = harness.start(enabled(1)).await;
    wait_for(&harness.state.poll_calls, 1).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if harness
                .database
                .load_pending_communication_events(1)
                .await
                .expect("load communication event")
                .len()
                == 1
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("completed media reaches atomic commit");
    runtime.shutdown().await.unwrap();

    let name = format!("{:x}", Sha256::digest(bytes));
    let copied = DbActorHandle::communication_spool_root(&harness.database_path).join(name);
    assert_eq!(fs::read(copied).expect("read committed spool file"), bytes);
    let connection = Connection::open(&harness.database_path).unwrap();
    assert_eq!(row_count(&connection, "events_local"), 1);
    assert_eq!(row_count(&connection, "sync_outbox"), 1);
    assert_eq!(row_count(&connection, "communication_messages"), 1);
    assert_eq!(row_count(&connection, "communication_cursors"), 1);
    assert_eq!(row_count(&connection, "attachment_spool"), 1);
}

#[tokio::test]
async fn incomplete_media_envelope_is_rejected_before_runtime_and_cannot_advance_state() {
    let attachment = CommunicationAttachment::try_new(
        "attachment-1".to_owned(),
        MessageKind::Image,
        "a".repeat(64),
        1,
        "image/png".to_owned(),
    )
    .unwrap();
    let message = CommunicationMessageRecorded::try_new(CommunicationMessageRecordedInput {
        message_id: "message-1".to_owned(),
        conversation_id: "conversation-1".to_owned(),
        source_key: "opaque-source-key".to_owned(),
        occurred_at: "2026-08-02T00:00:00Z".to_owned(),
        direction: Direction::Incoming,
        kind: MessageKind::Image,
        conversation: ConversationScope::Direct,
        text: None,
        attachments: vec![attachment],
    })
    .unwrap();
    assert!(NormalizedCommunicationRecord::try_new(
        "wechat-account-1".to_owned(),
        1,
        message,
        Vec::new(),
    )
    .is_err());
}

#[tokio::test]
async fn symlink_hash_size_open_and_copy_failures_leave_no_partial_spool_or_database_rows() {
    for failure in ["symlink", "hash", "size", "missing", "unsafe-root"] {
        let harness = Harness::new().await;
        let bytes = b"verified-source";
        let real_source = harness.directory.path().join(format!("{failure}-real.mp4"));
        fs::write(&real_source, bytes).unwrap();
        let source = match failure {
            "symlink" => {
                let path = harness.directory.path().join("source-link.mp4");
                symlink(&real_source, &path).unwrap();
                path
            }
            "missing" => harness.directory.path().join("missing.mp4"),
            _ => real_source.clone(),
        };
        let declared = if failure == "hash" {
            b"other".as_slice()
        } else {
            bytes
        };
        let safe_source = if failure == "symlink" || failure == "missing" {
            source.clone()
        } else {
            source.canonicalize().unwrap()
        };
        let mut record = media_record(&safe_source, declared, 1);
        if failure == "size" {
            record = media_record(&safe_source, b"verified-source-extra", 1);
        }
        if failure == "unsafe-root" {
            let root = DbActorHandle::communication_spool_root(&harness.database_path);
            fs::remove_dir(&root).unwrap();
            symlink(harness.directory.path(), root).unwrap();
        }
        harness
            .state
            .poll_results
            .lock()
            .unwrap()
            .push_back(Ok(vec![record]));
        let runtime = harness.start(enabled(1)).await;
        wait_for(&harness.state.poll_calls, 1).await;
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        runtime.shutdown().await.unwrap();
        harness.assert_no_communication_commit().await;
        let root = DbActorHandle::communication_spool_root(&harness.database_path);
        if root.is_dir() {
            assert!(fs::read_dir(root)
                .unwrap()
                .filter_map(Result::ok)
                .all(|entry| !entry.file_name().to_string_lossy().contains(".partial-")));
        }
    }
}

#[tokio::test]
async fn spool_hard_limit_pauses_copy_until_usage_is_strictly_below_resume_water() {
    assert_eq!(SPOOL_HARD_LIMIT_BYTES, 6 * 1024 * 1024 * 1024);
    assert_eq!(SPOOL_RESUME_BELOW_BYTES, 5 * 1024 * 1024 * 1024);
    let harness = Harness::new().await;
    let bytes = b"xy";
    let source = harness.directory.path().join("source.mp4");
    fs::write(&source, bytes).unwrap();
    let record = media_record(&source.canonicalize().unwrap(), bytes, 1);
    for _ in 0..4 {
        harness
            .state
            .poll_results
            .lock()
            .unwrap()
            .push_back(Ok(vec![record.clone()]));
    }
    let root = DbActorHandle::communication_spool_root(&harness.database_path);
    let filler = root.join("filler");
    let file = fs::File::create(&filler).unwrap();
    file.set_len(SPOOL_HARD_LIMIT_BYTES - 1).unwrap();
    let runtime = harness.start(enabled(1)).await;
    wait_for(&harness.state.poll_calls, 1).await;
    wait_for_collector_error(&harness.database, 1).await;
    harness.assert_no_communication_commit().await;

    file.set_len(SPOOL_RESUME_BELOW_BYTES).unwrap();
    runtime.apply_control(enabled(2)).await.unwrap();
    wait_for(&harness.state.poll_calls, 2).await;
    wait_for_collector_error(&harness.database, 2).await;
    harness.assert_no_communication_commit().await;

    file.set_len(SPOOL_RESUME_BELOW_BYTES - 1).unwrap();
    runtime.apply_control(enabled(3)).await.unwrap();
    wait_for(&harness.state.poll_calls, 3).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if harness.database.active_outbox_depth().await.unwrap() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("spool resumes below five GiB");
    runtime.shutdown().await.unwrap();
    let depth = harness.database.active_outbox_depth().await.unwrap();
    let inspection = Connection::open(&harness.database_path).unwrap();
    let error_code: Option<String> = inspection
        .query_row(
            "SELECT last_error_code FROM collector_states WHERE collector_key = 'communication.wechat'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(depth, 1, "collector error after resume: {error_code:?}");
    assert!(fs::read_dir(root)
        .unwrap()
        .filter_map(Result::ok)
        .all(|entry| !entry.file_name().to_string_lossy().contains(".partial-")));
}

#[tokio::test(start_paused = true)]
async fn outbox_high_water_blocks_probe_and_resumes_only_below_low_water() {
    assert_eq!(OUTBOX_HIGH_WATER, 10_000);
    assert_eq!(OUTBOX_LOW_WATER, 8_000);
    let harness = Harness::new().await;
    seed_outbox(&harness.database_path, OUTBOX_HIGH_WATER + 1);
    let runtime = harness.start(enabled(1)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert_eq!(harness.state.discover_calls.load(Ordering::SeqCst), 0);

    trim_outbox(&harness.database_path, OUTBOX_LOW_WATER);
    tokio::time::advance(Duration::from_secs(30)).await;
    assert_eq!(harness.state.discover_calls.load(Ordering::SeqCst), 0);

    trim_outbox(&harness.database_path, OUTBOX_LOW_WATER - 1);
    tokio::time::advance(Duration::from_secs(30)).await;
    wait_for(&harness.state.discover_calls, 1).await;
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn newer_disabled_or_unpaired_control_cancels_in_flight_provider_without_commit() {
    for next in [disabled(2), CommunicationControl::unpaired()] {
        let harness = Harness::new().await;
        harness.state.block_poll.store(true, Ordering::SeqCst);
        let runtime = harness.start(enabled(1)).await;
        harness.state.poll_entered.notified().await;
        runtime.apply_control(next).await.expect("disable applies");
        wait_for(&harness.state.stop_calls, 1).await;
        harness.assert_no_communication_commit().await;
        runtime.shutdown().await.unwrap();
    }
}

fn row_count(connection: &Connection, table: &str) -> u64 {
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

async fn settle() {
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
}

async fn wait_for_collector_error(database: &DbActorHandle, revision: u64) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let state = database
                .load_collector_states()
                .await
                .unwrap()
                .into_iter()
                .find(|state| state.collector_key == "communication.wechat");
            if state.is_some_and(|state| {
                state.desired_config_revision == revision
                    && state.last_error_code.as_deref() == Some("WECHAT_LOCAL_SPOOL_UNAVAILABLE")
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("collector records the spool failure");
}

fn seed_outbox(path: &Path, count: u64) {
    let mut connection = Connection::open(path).unwrap();
    let transaction = connection.transaction().unwrap();
    for index in 0..count {
        transaction
            .execute(
                "INSERT INTO events_local (event_id, workspace_id, device_id, event_type, source, schema_version, occurred_at_ms, created_at_ms, sensitivity, payload_json, attachment_refs_json, idempotency_key) VALUES (?1, 'w', 'd', 'test', 'test', 1, 0, 0, 'normal', '{}', '[]', NULL)",
                [format!("event-{index}")],
            )
            .unwrap();
        transaction
            .execute(
                "INSERT INTO sync_outbox (outbox_id, event_id, state, created_at_ms) VALUES (?1, ?2, 'pending', 0)",
                [format!("outbox-{index}"), format!("event-{index}")],
            )
            .unwrap();
    }
    transaction.commit().unwrap();
}

fn trim_outbox(path: &Path, count: u64) {
    let connection = Connection::open(path).unwrap();
    connection
        .execute(
            "DELETE FROM sync_outbox WHERE outbox_id NOT IN (SELECT outbox_id FROM sync_outbox ORDER BY outbox_id LIMIT ?1)",
            [count],
        )
        .unwrap();
}
