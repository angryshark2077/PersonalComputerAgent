use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use pca_agentd::cloud_control::{
    AgentControlSnapshot, CloudControlRuntime, ControlClient, ControlError, ControlFuture,
    LoadedDeviceCredentials, SyncEventsResponse,
};
use pca_agentd::communication::{
    CommunicationAuthorization, CommunicationControl, CommunicationIdentity, CommunicationRuntime,
    CommunicationRuntimeError, UnavailableCommunicationProviderFactory, OUTBOX_HIGH_WATER,
    OUTBOX_LOW_WATER, SPOOL_HARD_LIMIT_BYTES, SPOOL_RESUME_BELOW_BYTES,
};
use pca_db_local::{DbActorHandle, PairingState};
use pca_domain::{
    CollectorStatus, CommunicationAttachment, CommunicationMessageRecorded,
    CommunicationMessageRecordedInput, ConversationScope, Direction, DomainError, EventEnvelope,
    MessageKind, Sensitivity,
};
use pca_keychain::{CredentialError, CredentialStore, DeviceCredential};
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
    poll_release: Notify,
    discover_results: Mutex<VecDeque<Result<(), DomainError>>>,
    poll_results: Mutex<VecDeque<ProviderResult>>,
    stop_results: Mutex<VecDeque<Result<(), DomainError>>>,
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
                self.state.poll_release.notified().await;
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
        self.state
            .stop_results
            .lock()
            .expect("stop plan")
            .pop_front()
            .unwrap_or(Ok(()))
    }
}

struct Harness {
    directory: TempDir,
    database_path: PathBuf,
    database: Arc<DbActorHandle>,
    state: Arc<RecordingState>,
}

#[derive(Default)]
struct ControlStore {
    values: Mutex<BTreeMap<(String, String), Vec<u8>>>,
    fail_delete: AtomicBool,
}

impl CredentialStore for ControlStore {
    fn load(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, CredentialError> {
        Ok(self
            .values
            .lock()
            .unwrap()
            .get(&(service.to_owned(), account.to_owned()))
            .cloned())
    }

    fn store(&self, service: &str, account: &str, value: &[u8]) -> Result<(), CredentialError> {
        self.values
            .lock()
            .unwrap()
            .insert((service.to_owned(), account.to_owned()), value.to_vec());
        Ok(())
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), CredentialError> {
        if self.fail_delete.load(Ordering::SeqCst) {
            return Err(CredentialError::OperationFailed);
        }
        self.values
            .lock()
            .unwrap()
            .remove(&(service.to_owned(), account.to_owned()));
        Ok(())
    }
}

struct IntegratedControlClient {
    calls: AtomicUsize,
    snapshots: Mutex<VecDeque<Result<AgentControlSnapshot, ControlError>>>,
    block_sync: AtomicBool,
    sync_entered: Notify,
    sync_release: Notify,
}

impl ControlClient for IntegratedControlClient {
    fn refresh<'a>(&'a self, _: &'a DeviceCredential) -> ControlFuture<'a, DeviceCredential> {
        Box::pin(async { Err(ControlError::Contract) })
    }

    fn heartbeat_and_control<'a>(
        &'a self,
        _: &'a DeviceCredential,
        _: u64,
    ) -> ControlFuture<'a, AgentControlSnapshot> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let result = self
            .snapshots
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Err(ControlError::Transient));
        Box::pin(async move { result })
    }

    fn sync_system_events<'a>(
        &'a self,
        _: &'a DeviceCredential,
        _: &'a [EventEnvelope],
    ) -> ControlFuture<'a, SyncEventsResponse> {
        Box::pin(async move {
            if self.block_sync.load(Ordering::SeqCst) {
                self.sync_entered.notify_waiters();
                self.sync_release.notified().await;
            }
            Err(ControlError::Transient)
        })
    }
}

impl Harness {
    async fn new() -> Self {
        let directory = TempDir::new().expect("temporary runtime directory");
        let database_path = directory
            .path()
            .canonicalize()
            .expect("canonical temporary runtime directory")
            .join("agent.sqlite3");
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

    async fn new_with_symlinked_database_ancestor() -> Self {
        let directory = TempDir::new().expect("temporary runtime directory");
        let base = directory
            .path()
            .canonicalize()
            .expect("canonical temporary runtime directory");
        let real = base.join("real-database-parent");
        fs::create_dir(&real).expect("create real database parent");
        let alias = base.join("database-parent-alias");
        symlink(&real, &alias).expect("create database parent symlink");
        let database_path = alias.join("agent.sqlite3");
        let database = Arc::new(
            DbActorHandle::open(&database_path, "test")
                .await
                .expect("open database through ancestor symlink"),
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

    async fn assert_no_communication_rows(&self) {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        let connection = Connection::open(&self.database_path).expect("inspect database");
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM events_local WHERE source = 'communication.wechat'",
                    [],
                    |row| row.get::<_, u64>(0),
                )
                .unwrap(),
            0
        );
        for table in [
            "communication_messages",
            "communication_cursors",
            "attachment_spool",
        ] {
            assert_eq!(row_count(&connection, table), 0);
        }
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
        "Conversation One".to_owned(),
        CommunicationMessageRecorded::try_new(CommunicationMessageRecordedInput {
            message_id: format!("message-{sequence}"),
            conversation_id: "conversation-1".to_owned(),
            sender_id: "wxid_self".to_owned(),
            sender_display_name: "You".to_owned(),
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
        "Conversation One".to_owned(),
        CommunicationMessageRecorded::try_new(CommunicationMessageRecordedInput {
            message_id: format!("message-{sequence}"),
            conversation_id: "conversation-1".to_owned(),
            sender_id: "wxid_sender".to_owned(),
            sender_display_name: "Sender".to_owned(),
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

fn multi_media_record(
    sequence: u64,
    kind: MessageKind,
    media: &[(&str, &Path, &[u8], &str)],
) -> NormalizedCommunicationRecord {
    let attachments = media
        .iter()
        .map(|(attachment_id, _, declared, mime_type)| {
            CommunicationAttachment::try_new(
                (*attachment_id).to_owned(),
                kind,
                format!("{:x}", Sha256::digest(declared)),
                u64::try_from(declared.len()).expect("fixture size"),
                (*mime_type).to_owned(),
            )
            .expect("valid attachment fixture")
        })
        .collect::<Vec<_>>();
    let completed_media = media
        .iter()
        .map(|(attachment_id, path, _, _)| {
            CompletedMediaSource::try_new((*attachment_id).to_owned(), (*path).to_owned())
                .expect("valid completed source fixture")
        })
        .collect();
    NormalizedCommunicationRecord::try_new(
        "wechat-account-1".to_owned(),
        sequence,
        "Conversation One".to_owned(),
        CommunicationMessageRecorded::try_new(CommunicationMessageRecordedInput {
            message_id: format!("message-{sequence}"),
            conversation_id: "conversation-1".to_owned(),
            sender_id: "wxid_sender".to_owned(),
            sender_display_name: "Sender".to_owned(),
            source_key: format!("opaque-source-key-{sequence}"),
            occurred_at: "2026-08-02T00:00:00Z".to_owned(),
            direction: Direction::Incoming,
            kind,
            conversation: ConversationScope::Direct,
            text: None,
            attachments,
        })
        .expect("valid media message fixture"),
        completed_media,
    )
    .expect("valid normalized multi-media record")
}

async fn wait_for(counter: &AtomicUsize, expected: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if counter.load(Ordering::SeqCst) >= expected {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("counter did not reach {expected}");
}

fn prevent_virtual_time_auto_advance() -> tokio::task::JoinHandle<()> {
    tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    })
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
    wait_for(&harness.state.poll_calls, 1).await;

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

#[tokio::test(start_paused = true)]
async fn authoritative_disable_interrupts_retry_wait_without_an_app_command() {
    let harness = Harness::new().await;
    harness
        .state
        .discover_results
        .lock()
        .unwrap()
        .push_back(Err(DomainError::new(
            "WECHAT_WAITING_SOURCE",
            "redacted",
            true,
        )));
    let authorization = CommunicationAuthorization::new();
    authorization.apply_persisted(enabled(1)).await.unwrap();
    let time_guard = prevent_virtual_time_auto_advance();
    let runtime = CommunicationRuntime::start_authorized(
        Arc::clone(&harness.database),
        harness.database_path.clone(),
        harness.factory(),
        authorization.clone(),
    )
    .await
    .unwrap();
    wait_for(&harness.state.discover_calls, 1).await;
    wait_for(&harness.state.stop_calls, 1).await;

    authorization.disable().await;
    wait_for_collector_status(&harness.database, CollectorStatus::Disabled).await;
    tokio::time::advance(Duration::from_hours(1)).await;
    settle().await;
    assert_eq!(harness.state.factory_calls.load(Ordering::SeqCst), 1);

    runtime.shutdown().await.unwrap();
    time_guard.abort();
}

#[tokio::test(start_paused = true)]
async fn authoritative_disable_interrupts_outbox_hysteresis_without_an_app_command() {
    let harness = Harness::new().await;
    seed_outbox(&harness.database_path, OUTBOX_HIGH_WATER + 1);
    let authorization = CommunicationAuthorization::new();
    authorization.apply_persisted(enabled(1)).await.unwrap();
    let time_guard = prevent_virtual_time_auto_advance();
    let runtime = CommunicationRuntime::start_authorized(
        Arc::clone(&harness.database),
        harness.database_path.clone(),
        harness.factory(),
        authorization.clone(),
    )
    .await
    .unwrap();
    settle().await;
    assert_eq!(harness.state.factory_calls.load(Ordering::SeqCst), 0);

    authorization.disable().await;
    wait_for_collector_status(&harness.database, CollectorStatus::Disabled).await;
    trim_outbox(&harness.database_path, OUTBOX_LOW_WATER - 1);
    tokio::time::advance(Duration::from_secs(30)).await;
    settle().await;
    assert_eq!(harness.state.factory_calls.load(Ordering::SeqCst), 0);

    runtime.shutdown().await.unwrap();
    time_guard.abort();
}

#[tokio::test(start_paused = true)]
async fn authoritative_disable_interrupts_terminal_wait_without_an_app_command() {
    let harness = Harness::new().await;
    harness
        .state
        .discover_results
        .lock()
        .unwrap()
        .push_back(Err(DomainError::new(
            "WECHAT_CAPABILITY_UNAVAILABLE",
            "redacted",
            false,
        )));
    let authorization = CommunicationAuthorization::new();
    authorization.apply_persisted(enabled(1)).await.unwrap();
    let runtime = CommunicationRuntime::start_authorized(
        Arc::clone(&harness.database),
        harness.database_path.clone(),
        harness.factory(),
        authorization.clone(),
    )
    .await
    .unwrap();
    wait_for(&harness.state.discover_calls, 1).await;

    authorization.disable().await;
    wait_for_collector_status(&harness.database, CollectorStatus::Disabled).await;
    assert_eq!(harness.state.factory_calls.load(Ordering::SeqCst), 1);

    runtime.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn b2_retry_schedule_is_exact_and_caps_at_five_minutes() {
    let harness = Harness::new().await;
    let time_guard = prevent_virtual_time_auto_advance();
    let codes = [
        "WECHAT_DATABASE_UNAVAILABLE",
        "WECHAT_PROBE_TIMEOUT",
        "WECHAT_WAITING_SOURCE",
        "WECHAT_DATABASE_UNAVAILABLE",
        "WECHAT_PROBE_TIMEOUT",
        "WECHAT_WAITING_SOURCE",
        "WECHAT_DATABASE_UNAVAILABLE",
    ];
    harness.state.discover_results.lock().unwrap().extend(
        codes
            .iter()
            .map(|code| Err(DomainError::new(code, "redacted", true))),
    );
    let runtime = harness.start(enabled(1)).await;
    wait_for(&harness.state.discover_calls, 1).await;
    wait_for_collector_code(&harness.database, codes[0]).await;

    for (index, delay) in [30_u64, 60, 120, 240, 300, 300].into_iter().enumerate() {
        settle().await;
        tokio::time::advance(Duration::from_secs(delay - 1)).await;
        settle().await;
        assert_eq!(
            harness.state.factory_calls.load(Ordering::SeqCst),
            index + 1,
            "retry must not start before delay {delay}"
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        wait_for(&harness.state.factory_calls, index + 2).await;
        wait_for_collector_code(&harness.database, codes[index + 1]).await;
    }

    runtime.shutdown().await.unwrap();
    time_guard.abort();
}

#[tokio::test(start_paused = true)]
async fn b2_success_and_new_control_reset_retry_delay_to_thirty_seconds() {
    let time_guard = prevent_virtual_time_auto_advance();
    let after_success = Harness::new().await;
    after_success
        .state
        .discover_results
        .lock()
        .unwrap()
        .extend([
            Err(DomainError::new(
                "WECHAT_DATABASE_UNAVAILABLE",
                "redacted",
                true,
            )),
            Ok(()),
            Ok(()),
        ]);
    after_success
        .state
        .poll_results
        .lock()
        .unwrap()
        .push_back(Err(DomainError::new(
            "WECHAT_PROBE_TIMEOUT",
            "redacted",
            true,
        )));
    let after_success_runtime = after_success.start(enabled(1)).await;
    wait_for(&after_success.state.discover_calls, 1).await;
    wait_for_collector_code(&after_success.database, "WECHAT_DATABASE_UNAVAILABLE").await;
    settle().await;
    tokio::time::advance(Duration::from_secs(30)).await;
    wait_for(&after_success.state.poll_calls, 1).await;
    wait_for_collector_code(&after_success.database, "WECHAT_PROBE_TIMEOUT").await;
    settle().await;
    tokio::time::advance(Duration::from_secs(29)).await;
    assert_eq!(after_success.state.factory_calls.load(Ordering::SeqCst), 2);
    tokio::time::advance(Duration::from_secs(1)).await;
    wait_for(&after_success.state.factory_calls, 3).await;
    after_success_runtime.shutdown().await.unwrap();

    let after_control = Harness::new().await;
    after_control
        .state
        .discover_results
        .lock()
        .unwrap()
        .extend([
            Err(DomainError::new(
                "WECHAT_DATABASE_UNAVAILABLE",
                "redacted",
                true,
            )),
            Err(DomainError::new("WECHAT_PROBE_TIMEOUT", "redacted", true)),
            Err(DomainError::new("WECHAT_WAITING_SOURCE", "redacted", true)),
            Err(DomainError::new(
                "WECHAT_DATABASE_UNAVAILABLE",
                "redacted",
                true,
            )),
        ]);
    let after_control_runtime = after_control.start(enabled(1)).await;
    wait_for(&after_control.state.discover_calls, 1).await;
    wait_for_collector_code(&after_control.database, "WECHAT_DATABASE_UNAVAILABLE").await;
    settle().await;
    tokio::time::advance(Duration::from_secs(30)).await;
    wait_for(&after_control.state.discover_calls, 2).await;
    wait_for_collector_code(&after_control.database, "WECHAT_PROBE_TIMEOUT").await;
    after_control_runtime
        .apply_control(enabled(2))
        .await
        .unwrap();
    wait_for(&after_control.state.discover_calls, 3).await;
    wait_for_collector_code(&after_control.database, "WECHAT_WAITING_SOURCE").await;
    settle().await;
    let post_control_attempts = after_control.state.factory_calls.load(Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(29)).await;
    assert_eq!(
        after_control.state.factory_calls.load(Ordering::SeqCst),
        post_control_attempts
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    wait_for(
        &after_control.state.factory_calls,
        post_control_attempts + 1,
    )
    .await;
    after_control_runtime.shutdown().await.unwrap();
    time_guard.abort();
}

#[tokio::test(start_paused = true)]
async fn b2_terminal_or_malformed_errors_never_retry_even_when_flagged_retryable() {
    let time_guard = prevent_virtual_time_auto_advance();
    for code in [
        "WECHAT_CAPABILITY_UNAVAILABLE",
        "WECHAT_UNSUPPORTED_SOURCE_VERSION",
        "WECHAT_INVALID_CONFIG",
        "WECHAT_STOP_FAILED",
        "DATABASE_UNAVAILABLE",
        "WECHAT_bad_code",
        "WECHAT_UNKNOWN_RETRY",
    ] {
        let harness = Harness::new().await;
        harness
            .state
            .discover_results
            .lock()
            .unwrap()
            .push_back(Err(DomainError::new(code, "redacted", true)));
        let runtime = harness.start(enabled(1)).await;
        wait_for(&harness.state.discover_calls, 1).await;
        wait_for_collector_code(&harness.database, code).await;
        settle().await;
        tokio::time::advance(Duration::from_hours(1)).await;
        settle().await;
        assert_eq!(
            harness.state.factory_calls.load(Ordering::SeqCst),
            1,
            "terminal or malformed code retried: {code}"
        );
        runtime.shutdown().await.unwrap();
    }

    let harness = Harness::new().await;
    harness
        .state
        .discover_results
        .lock()
        .unwrap()
        .push_back(Err(DomainError::new(
            "WECHAT_DATABASE_UNAVAILABLE",
            "redacted",
            false,
        )));
    let runtime = harness.start(enabled(1)).await;
    wait_for(&harness.state.discover_calls, 1).await;
    wait_for_collector_code(&harness.database, "WECHAT_DATABASE_UNAVAILABLE").await;
    settle().await;
    tokio::time::advance(Duration::from_hours(1)).await;
    settle().await;
    assert_eq!(harness.state.factory_calls.load(Ordering::SeqCst), 1);
    runtime.shutdown().await.unwrap();
    time_guard.abort();
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

    let pending = harness
        .database
        .load_pending_communication_events(3)
        .await
        .expect("load message and conversation metadata events");
    assert_eq!(pending.len(), 3);
    assert!(pending
        .iter()
        .any(|event| event.event_type == "communication.message_recorded"));
    assert!(pending
        .iter()
        .any(|event| event.event_type == "communication.conversation_observed"));
    assert!(pending
        .iter()
        .any(|event| event.event_type == "communication.message_sender_observed"));

    let name = format!("{:x}", Sha256::digest(bytes));
    let copied = DbActorHandle::communication_spool_root(&harness.database_path).join(name);
    assert_eq!(fs::read(copied).expect("read committed spool file"), bytes);
    let connection = Connection::open(&harness.database_path).unwrap();
    assert_eq!(row_count(&connection, "events_local"), 3);
    assert_eq!(row_count(&connection, "sync_outbox"), 3);
    assert_eq!(row_count(&connection, "communication_messages"), 1);
    assert_eq!(row_count(&connection, "communication_cursors"), 1);
    assert_eq!(row_count(&connection, "attachment_spool"), 1);
}

#[tokio::test]
async fn replayed_media_with_a_changed_manifest_does_not_block_a_new_message() {
    let harness = Harness::new().await;
    let source = harness.directory.path().join("replayed.mp4");
    let original = b"original-media";
    fs::write(&source, original).expect("write original media");
    let source = source.canonicalize().expect("canonical media source");
    harness
        .state
        .poll_results
        .lock()
        .unwrap()
        .push_back(Ok(vec![media_record(&source, original, 1)]));

    let runtime = harness.start(enabled(1)).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let connection = Connection::open(&harness.database_path).unwrap();
            if row_count(&connection, "communication_messages") == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("original media commits");
    runtime.shutdown().await.unwrap();

    let changed = b"changed-media";
    fs::write(&source, changed).expect("replace replayed media");
    harness
        .state
        .poll_results
        .lock()
        .unwrap()
        .push_back(Ok(vec![media_record(&source, changed, 1), text_record(2)]));

    let runtime = harness.start(enabled(1)).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let connection = Connection::open(&harness.database_path).unwrap();
            if row_count(&connection, "communication_messages") == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("new message is not blocked by changed replay");
    runtime.shutdown().await.unwrap();

    let connection = Connection::open(&harness.database_path).unwrap();
    assert_eq!(row_count(&connection, "communication_messages"), 2);
    assert_eq!(row_count(&connection, "attachment_spool"), 1);
}

#[tokio::test]
async fn b1_cancellation_after_first_finalized_attachment_removes_attempt_files() {
    let harness = Harness::new().await;
    let first = b"first-finalized";
    let second = vec![0x5a; 64 * 1024 * 1024];
    let first_source = harness.directory.path().join("first.mp4");
    let second_source = harness.directory.path().join("second.mp4");
    fs::write(&first_source, first).unwrap();
    fs::write(&second_source, &second).unwrap();
    let first_source = first_source.canonicalize().unwrap();
    let second_source = second_source.canonicalize().unwrap();
    harness
        .state
        .poll_results
        .lock()
        .unwrap()
        .push_back(Ok(vec![multi_media_record(
            1,
            MessageKind::Video,
            &[
                (
                    "attachment-first",
                    first_source.as_path(),
                    first,
                    "video/mp4",
                ),
                (
                    "attachment-second",
                    second_source.as_path(),
                    second.as_slice(),
                    "video/mp4",
                ),
            ],
        )]));
    let authorization = CommunicationAuthorization::new();
    authorization.apply_persisted(enabled(1)).await.unwrap();
    let runtime = CommunicationRuntime::start_authorized(
        Arc::clone(&harness.database),
        harness.database_path.clone(),
        harness.factory(),
        authorization.clone(),
    )
    .await
    .unwrap();
    let root = DbActorHandle::communication_spool_root(&harness.database_path);
    let first_name = format!("{:x}", Sha256::digest(first));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let has_partial = fs::read_dir(&root)
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| entry.file_name().to_string_lossy().starts_with(".partial-"));
            if root.join(&first_name).is_file() && has_partial {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first attachment finalizes while the second remains partial");

    authorization.disable().await;
    wait_for(&harness.state.stop_calls, 1).await;
    runtime.shutdown().await.unwrap();
    harness.assert_no_communication_commit().await;
    assert!(fs::read_dir(root).unwrap().next().is_none());
}

#[tokio::test]
async fn b1_database_commit_failure_removes_newly_finalized_file() {
    let harness = Harness::new().await;
    let bytes = b"database-failure-media";
    let source = harness.directory.path().join("database-failure.mp4");
    fs::write(&source, bytes).unwrap();
    let source = source.canonicalize().unwrap();
    harness
        .state
        .poll_results
        .lock()
        .unwrap()
        .push_back(Ok(vec![media_record(&source, bytes, u64::MAX)]));
    let runtime = harness.start(enabled(1)).await;
    wait_for(&harness.state.poll_calls, 1).await;
    wait_for_collector_error(&harness.database, 1).await;
    runtime.shutdown().await.unwrap();

    harness.assert_no_communication_commit().await;
    let root = DbActorHandle::communication_spool_root(&harness.database_path);
    assert!(fs::read_dir(root).unwrap().next().is_none());
}

#[tokio::test]
async fn b1_second_attachment_failure_removes_first_new_final() {
    let harness = Harness::new().await;
    let first = b"first-valid-media";
    let second = b"missing-media";
    let first_source = harness.directory.path().join("first-valid.mp4");
    fs::write(&first_source, first).unwrap();
    let first_source = first_source.canonicalize().unwrap();
    let missing_source = harness
        .directory
        .path()
        .canonicalize()
        .unwrap()
        .join("missing-second.mp4");
    harness
        .state
        .poll_results
        .lock()
        .unwrap()
        .push_back(Ok(vec![multi_media_record(
            1,
            MessageKind::Video,
            &[
                (
                    "attachment-first",
                    first_source.as_path(),
                    first,
                    "video/mp4",
                ),
                (
                    "attachment-second",
                    missing_source.as_path(),
                    second,
                    "video/mp4",
                ),
            ],
        )]));
    let runtime = harness.start(enabled(1)).await;
    wait_for(&harness.state.poll_calls, 1).await;
    wait_for_collector_error(&harness.database, 1).await;
    runtime.shutdown().await.unwrap();

    harness.assert_no_communication_commit().await;
    let root = DbActorHandle::communication_spool_root(&harness.database_path);
    assert!(fs::read_dir(root).unwrap().next().is_none());
}

#[tokio::test]
async fn b1_preexisting_deduplicated_file_survives_later_attachment_failure() {
    let harness = Harness::new().await;
    let existing = b"shared-deduplicated-media";
    let missing = b"missing-media";
    let existing_source = harness.directory.path().join("existing-source.mp4");
    fs::write(&existing_source, existing).unwrap();
    let existing_source = existing_source.canonicalize().unwrap();
    let missing_source = harness
        .directory
        .path()
        .canonicalize()
        .unwrap()
        .join("missing-source.mp4");
    let root = DbActorHandle::communication_spool_root(&harness.database_path);
    let existing_name = format!("{:x}", Sha256::digest(existing));
    fs::write(root.join(&existing_name), existing).unwrap();
    harness
        .state
        .poll_results
        .lock()
        .unwrap()
        .push_back(Ok(vec![multi_media_record(
            1,
            MessageKind::Video,
            &[
                (
                    "attachment-existing",
                    existing_source.as_path(),
                    existing,
                    "video/mp4",
                ),
                (
                    "attachment-missing",
                    missing_source.as_path(),
                    missing,
                    "video/mp4",
                ),
            ],
        )]));
    let runtime = harness.start(enabled(1)).await;
    wait_for(&harness.state.poll_calls, 1).await;
    wait_for_collector_error(&harness.database, 1).await;
    runtime.shutdown().await.unwrap();

    harness.assert_no_communication_commit().await;
    assert_eq!(fs::read(root.join(existing_name)).unwrap(), existing);
    assert_eq!(fs::read_dir(root).unwrap().count(), 1);
}

#[tokio::test]
async fn b1_mime_family_mismatch_is_rejected_without_spool_or_database_rows() {
    let harness = Harness::new().await;
    let bytes = b"mime-mismatch";
    let source = harness.directory.path().join("mime-mismatch.mp4");
    fs::write(&source, bytes).unwrap();
    let source = source.canonicalize().unwrap();
    harness
        .state
        .poll_results
        .lock()
        .unwrap()
        .push_back(Ok(vec![multi_media_record(
            1,
            MessageKind::Video,
            &[("attachment-1", source.as_path(), bytes, "audio/mpeg")],
        )]));
    let runtime = harness.start(enabled(1)).await;
    wait_for(&harness.state.poll_calls, 1).await;
    wait_for_collector_error(&harness.database, 1).await;
    runtime.shutdown().await.unwrap();

    harness.assert_no_communication_commit().await;
    let root = DbActorHandle::communication_spool_root(&harness.database_path);
    assert!(fs::read_dir(root).unwrap().next().is_none());
}

#[tokio::test]
async fn b1_symlinked_source_or_spool_ancestor_is_rejected() {
    let source_harness = Harness::new().await;
    let bytes = b"ancestor-symlink";
    let source_base = source_harness.directory.path().canonicalize().unwrap();
    let real_source_parent = source_base.join("real-source-parent");
    fs::create_dir(&real_source_parent).unwrap();
    fs::write(real_source_parent.join("source.mp4"), bytes).unwrap();
    let source_alias = source_base.join("source-parent-alias");
    symlink(&real_source_parent, &source_alias).unwrap();
    source_harness
        .state
        .poll_results
        .lock()
        .unwrap()
        .push_back(Ok(vec![media_record(
            &source_alias.join("source.mp4"),
            bytes,
            1,
        )]));
    let source_runtime = source_harness.start(enabled(1)).await;
    wait_for(&source_harness.state.poll_calls, 1).await;
    wait_for_collector_error(&source_harness.database, 1).await;
    source_runtime.shutdown().await.unwrap();
    source_harness.assert_no_communication_commit().await;

    let spool_harness = Harness::new_with_symlinked_database_ancestor().await;
    let spool_source = spool_harness.directory.path().join("spool-source.mp4");
    fs::write(&spool_source, bytes).unwrap();
    let spool_source = spool_source.canonicalize().unwrap();
    spool_harness
        .state
        .poll_results
        .lock()
        .unwrap()
        .push_back(Ok(vec![media_record(&spool_source, bytes, 1)]));
    let spool_runtime = spool_harness.start(enabled(1)).await;
    wait_for(&spool_harness.state.poll_calls, 1).await;
    wait_for_collector_error(&spool_harness.database, 1).await;
    spool_runtime.shutdown().await.unwrap();
    spool_harness.assert_no_communication_commit().await;
}

#[tokio::test]
async fn b1_replaced_spool_path_cleans_only_through_pinned_directory() {
    let harness = Harness::new().await;
    let bytes = vec![0x6b; 8 * 1024 * 1024];
    let source = harness.directory.path().join("replacement-race.mp4");
    fs::write(&source, &bytes).unwrap();
    let source = source.canonicalize().unwrap();
    harness
        .state
        .poll_results
        .lock()
        .unwrap()
        .push_back(Ok(vec![media_record(&source, &bytes, 1)]));
    let root = DbActorHandle::communication_spool_root(&harness.database_path);
    let displaced = harness.directory.path().join("displaced-spool");
    let attacker = harness.directory.path().join("attacker-spool");
    fs::create_dir(&attacker).unwrap();
    let runtime = harness.start(enabled(1)).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if fs::read_dir(&root)
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| entry.file_name().to_string_lossy().starts_with(".partial-"))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("copy creates a partial before spool replacement");
    fs::rename(&root, &displaced).unwrap();
    symlink(&attacker, &root).unwrap();
    wait_for_collector_error(&harness.database, 1).await;
    runtime.shutdown().await.unwrap();

    harness.assert_no_communication_commit().await;
    assert!(fs::read_dir(displaced).unwrap().next().is_none());
    assert!(fs::read_dir(attacker).unwrap().next().is_none());
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
        sender_id: "wxid_sender".to_owned(),
        sender_display_name: "Sender".to_owned(),
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
        "Conversation One".to_owned(),
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
            if harness.database.active_outbox_depth().await.unwrap() == 3 {
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
    assert_eq!(depth, 3, "collector error after resume: {error_code:?}");
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
async fn b2_batch_rechecks_depth_and_skips_media_after_crossing_high_water() {
    let harness = Harness::new().await;
    seed_outbox(&harness.database_path, OUTBOX_HIGH_WATER);
    let skipped_bytes = b"must-not-be-copied";
    let skipped_source = harness.directory.path().join("skipped.mp4");
    fs::write(&skipped_source, skipped_bytes).unwrap();
    let skipped_source = skipped_source.canonicalize().unwrap();
    harness
        .state
        .poll_results
        .lock()
        .unwrap()
        .push_back(Ok(vec![
            text_record(1),
            media_record(&skipped_source, skipped_bytes, 2),
        ]));

    let runtime = harness.start(enabled(1)).await;
    wait_for(&harness.state.stop_calls, 1).await;
    runtime.shutdown().await.unwrap();

    assert_eq!(
        harness.database.active_outbox_depth().await.unwrap(),
        OUTBOX_HIGH_WATER + 3
    );
    let connection = Connection::open(&harness.database_path).unwrap();
    assert_eq!(row_count(&connection, "communication_messages"), 1);
    assert_eq!(row_count(&connection, "attachment_spool"), 0);
    assert_eq!(
        connection
            .query_row(
                "SELECT last_source_sequence FROM communication_cursors",
                [],
                |row| row.get::<_, u64>(0),
            )
            .unwrap(),
        1
    );
    let root = DbActorHandle::communication_spool_root(&harness.database_path);
    assert!(fs::read_dir(root).unwrap().next().is_none());
}

#[tokio::test]
async fn b2_system_outbox_movement_stops_later_batch_record_before_copy() {
    let harness = Harness::new().await;
    seed_outbox(&harness.database_path, OUTBOX_HIGH_WATER - 1);
    let first_bytes = vec![0x4d; 8 * 1024 * 1024];
    let second_bytes = b"later-media";
    let first_source = harness.directory.path().join("first-batch.mp4");
    let second_source = harness.directory.path().join("second-batch.mp4");
    fs::write(&first_source, &first_bytes).unwrap();
    fs::write(&second_source, second_bytes).unwrap();
    let first_source = first_source.canonicalize().unwrap();
    let second_source = second_source.canonicalize().unwrap();
    harness
        .state
        .poll_results
        .lock()
        .unwrap()
        .push_back(Ok(vec![
            media_record(&first_source, &first_bytes, 1),
            media_record(&second_source, second_bytes, 2),
        ]));
    let root = DbActorHandle::communication_spool_root(&harness.database_path);
    let runtime = harness.start(enabled(1)).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if fs::read_dir(&root)
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| entry.file_name().to_string_lossy().starts_with(".partial-"))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first batch record begins media copy");
    harness
        .database
        .append_event_with_outbox(&system_event("b2-system-depth-1"))
        .await
        .unwrap();
    harness
        .database
        .append_event_with_outbox(&system_event("b2-system-depth-2"))
        .await
        .unwrap();
    wait_for(&harness.state.stop_calls, 1).await;
    runtime.shutdown().await.unwrap();

    assert_eq!(
        harness.database.active_outbox_depth().await.unwrap(),
        OUTBOX_HIGH_WATER + 4
    );
    let connection = Connection::open(&harness.database_path).unwrap();
    assert_eq!(row_count(&connection, "communication_messages"), 1);
    assert_eq!(row_count(&connection, "attachment_spool"), 1);
    assert_eq!(
        connection
            .query_row(
                "SELECT last_source_sequence FROM communication_cursors",
                [],
                |row| row.get::<_, u64>(0),
            )
            .unwrap(),
        1
    );
    let first_name = format!("{:x}", Sha256::digest(&first_bytes));
    let second_name = format!("{:x}", Sha256::digest(second_bytes));
    assert!(root.join(first_name).is_file());
    assert!(!root.join(second_name).exists());
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

#[tokio::test]
async fn failed_provider_stop_blocks_enabled_replacement_and_prevents_overlap() {
    let harness = Harness::new().await;
    harness.state.block_poll.store(true, Ordering::SeqCst);
    harness.state.stop_results.lock().unwrap().extend([
        Err(DomainError::new(
            "WECHAT_PRIVATE_STOP_DETAIL",
            "redacted",
            false,
        )),
        Err(DomainError::new(
            "WECHAT_PRIVATE_STOP_DETAIL",
            "redacted",
            false,
        )),
        Err(DomainError::new(
            "WECHAT_PRIVATE_STOP_DETAIL",
            "redacted",
            false,
        )),
    ]);
    let runtime = harness.start(enabled(1)).await;
    wait_for(&harness.state.poll_calls, 1).await;

    runtime
        .apply_control(enabled(2))
        .await
        .expect("new revision is recorded even when the old provider cannot stop");
    wait_for_stop_quarantine(&harness.database, 2).await;
    assert_eq!(harness.state.stop_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        harness.state.factory_calls.load(Ordering::SeqCst),
        1,
        "a replacement provider must not be created after stop failure"
    );

    runtime
        .apply_control(disabled(3))
        .await
        .expect("the quarantined owner remains alive to process disable");
    wait_for_stop_quarantine(&harness.database, 3).await;
    wait_for(&harness.state.stop_calls, 2).await;
    assert_eq!(harness.state.factory_calls.load(Ordering::SeqCst), 1);

    let error = runtime
        .shutdown()
        .await
        .expect_err("repeated stop failure is surfaced by joined shutdown");
    assert_eq!(error.to_string(), "communication provider stop failed");
    assert_eq!(harness.state.stop_calls.load(Ordering::SeqCst), 3);
    harness.assert_no_communication_commit().await;
}

#[tokio::test]
async fn quarantined_provider_retries_stop_when_command_channel_closes() {
    let harness = Harness::new().await;
    harness.state.block_poll.store(true, Ordering::SeqCst);
    harness.state.stop_results.lock().unwrap().extend([
        Err(DomainError::new("WECHAT_STOP_PRIVATE", "redacted", false)),
        Err(DomainError::new("WECHAT_STOP_PRIVATE", "redacted", false)),
    ]);
    let runtime = harness.start(enabled(1)).await;
    wait_for(&harness.state.poll_calls, 1).await;
    runtime.apply_control(enabled(2)).await.unwrap();
    wait_for_stop_quarantine(&harness.database, 2).await;

    drop(runtime);
    wait_for(&harness.state.stop_calls, 2).await;
    assert_eq!(harness.state.factory_calls.load(Ordering::SeqCst), 1);
    harness.assert_no_communication_commit().await;
}

#[tokio::test]
async fn authoritative_disable_cancels_media_preparation_without_app_forwarding_or_commit() {
    let harness = Harness::new().await;
    let bytes = vec![0x5a; 64 * 1024 * 1024];
    let source = harness.directory.path().join("slow-source.mp4");
    fs::write(&source, &bytes).unwrap();
    harness
        .state
        .poll_results
        .lock()
        .unwrap()
        .push_back(Ok(vec![media_record(
            &source.canonicalize().unwrap(),
            &bytes,
            1,
        )]));
    let authorization = CommunicationAuthorization::new();
    authorization
        .apply_persisted(enabled(1))
        .await
        .expect("persisted exact-v2 enable is authoritative");
    let runtime = CommunicationRuntime::start_authorized(
        Arc::clone(&harness.database),
        harness.database_path.clone(),
        harness.factory(),
        authorization.clone(),
    )
    .await
    .unwrap();
    let root = DbActorHandle::communication_spool_root(&harness.database_path);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if fs::read_dir(&root)
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| entry.file_name().to_string_lossy().starts_with(".partial-"))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("media preparation creates its attempt file");

    authorization.disable().await;
    wait_for(&harness.state.stop_calls, 1).await;
    runtime.shutdown().await.unwrap();
    harness.assert_no_communication_commit().await;
    assert!(fs::read_dir(root).unwrap().next().is_none());
}

#[tokio::test(start_paused = true)]
async fn cloud_disable_invalidates_runtime_before_failing_system_sync() {
    let harness = Harness::new().await;
    harness.state.block_poll.store(true, Ordering::SeqCst);
    harness
        .state
        .poll_results
        .lock()
        .unwrap()
        .push_back(Ok(vec![text_record(1)]));
    let authorization = CommunicationAuthorization::new();
    let communication = CommunicationRuntime::start_authorized(
        Arc::clone(&harness.database),
        harness.database_path.clone(),
        harness.factory(),
        authorization.clone(),
    )
    .await
    .unwrap();
    let store = Arc::new(ControlStore::default());
    let loaded = control_credentials(Arc::clone(&store));
    save_pairing(&harness.database, loaded.credential()).await;
    let client = Arc::new(IntegratedControlClient {
        calls: AtomicUsize::new(0),
        snapshots: Mutex::new(VecDeque::from([
            Ok(control_snapshot(1, true)),
            Ok(control_snapshot(2, false)),
        ])),
        block_sync: AtomicBool::new(true),
        sync_entered: Notify::new(),
        sync_release: Notify::new(),
    });
    let (pairing_sender, _) = tokio::sync::watch::channel(false);
    let cloud = CloudControlRuntime::start_with_pairing_state_and_authorization(
        Arc::clone(&harness.database),
        loaded,
        Arc::clone(&client) as Arc<dyn ControlClient>,
        pairing_sender,
        authorization,
    )
    .await
    .unwrap();
    wait_for(&harness.state.poll_calls, 1).await;
    harness
        .database
        .append_event_with_outbox(&system_event("disable-sync-failure"))
        .await
        .unwrap();
    let sync_entered = client.sync_entered.notified();
    tokio::pin!(sync_entered);
    tokio::time::advance(Duration::from_secs(30)).await;
    sync_entered.as_mut().await;

    wait_for(&harness.state.stop_calls, 1).await;
    harness.assert_no_communication_rows().await;
    client.sync_release.notify_waiters();
    cloud.shutdown().await.unwrap();
    communication.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn cloud_revoke_invalidates_runtime_before_failing_credential_cleanup() {
    let harness = Harness::new().await;
    harness.state.block_poll.store(true, Ordering::SeqCst);
    harness
        .state
        .poll_results
        .lock()
        .unwrap()
        .push_back(Ok(vec![text_record(1)]));
    let authorization = CommunicationAuthorization::new();
    let communication = CommunicationRuntime::start_authorized(
        Arc::clone(&harness.database),
        harness.database_path.clone(),
        harness.factory(),
        authorization.clone(),
    )
    .await
    .unwrap();
    let store = Arc::new(ControlStore::default());
    store.fail_delete.store(true, Ordering::SeqCst);
    let loaded = control_credentials(Arc::clone(&store));
    save_pairing(&harness.database, loaded.credential()).await;
    let client = Arc::new(IntegratedControlClient {
        calls: AtomicUsize::new(0),
        snapshots: Mutex::new(VecDeque::from([
            Ok(control_snapshot(1, true)),
            Err(ControlError::Revoked),
        ])),
        block_sync: AtomicBool::new(false),
        sync_entered: Notify::new(),
        sync_release: Notify::new(),
    });
    let (pairing_sender, _) = tokio::sync::watch::channel(false);
    let cloud = CloudControlRuntime::start_with_pairing_state_and_authorization(
        Arc::clone(&harness.database),
        loaded,
        Arc::clone(&client) as Arc<dyn ControlClient>,
        pairing_sender,
        authorization,
    )
    .await
    .unwrap();
    wait_for(&harness.state.poll_calls, 1).await;
    tokio::time::advance(Duration::from_secs(30)).await;
    wait_for(&client.calls, 2).await;

    wait_for(&harness.state.stop_calls, 1).await;
    harness.assert_no_communication_commit().await;
    assert!(cloud.shutdown().await.is_err());
    communication.shutdown().await.unwrap();
}

fn control_credentials(store: Arc<ControlStore>) -> LoadedDeviceCredentials {
    let credential = DeviceCredential::new(
        "11111111-1111-4111-8111-111111111111".to_owned(),
        "22222222-2222-4222-8222-222222222222".to_owned(),
        "synthetic-access",
        "synthetic-refresh",
    )
    .unwrap()
    .with_metadata(1, 1_850_000_000_000, 1_900_000_000_000);
    LoadedDeviceCredentials::new(credential, store)
}

async fn save_pairing(database: &DbActorHandle, credential: &DeviceCredential) {
    database
        .save_pairing_state(&PairingState::paired(
            credential.device_id(),
            credential.workspace_id(),
            "keychain://pca/device/current",
            1,
            "https://pca-cloud-api-production.up.railway.app",
        ))
        .await
        .unwrap();
}

fn control_snapshot(revision: u64, enabled: bool) -> AgentControlSnapshot {
    serde_json::from_value(serde_json::json!({
        "device_id": "11111111-1111-4111-8111-111111111111",
        "workspace_id": "22222222-2222-4222-8222-222222222222",
        "revoked": false,
        "configuration_revision": revision,
        "collectors": {
            "network": { "enabled": false },
            "communication.wechat": {
                "enabled": enabled,
                "directions": ["incoming", "outgoing"],
                "message_types": ["text", "audio", "image", "video"],
                "conversation_scope": "direct_and_group_at_most_fifteen_members",
                "max_group_members": 15,
                "sync_mode": "full",
                "retention_days": 180
            }
        }
    }))
    .unwrap()
}

fn system_event(event_id: &str) -> EventEnvelope {
    EventEnvelope {
        event_id: event_id.to_owned(),
        workspace_id: "22222222-2222-4222-8222-222222222222".to_owned(),
        device_id: "11111111-1111-4111-8111-111111111111".to_owned(),
        event_type: "system.metric_sampled".to_owned(),
        source: "system".to_owned(),
        schema_version: 1,
        occurred_at: "2026-08-02T00:00:00Z".to_owned(),
        created_at: "2026-08-02T00:00:00Z".to_owned(),
        sensitivity: Sensitivity::Normal,
        payload: serde_json::Map::new(),
        attachment_refs: Vec::new(),
        idempotency_key: Some(format!("system:{event_id}")),
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

async fn wait_for_collector_code(database: &DbActorHandle, expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let state = database
            .load_collector_states()
            .await
            .unwrap()
            .into_iter()
            .find(|state| state.collector_key == "communication.wechat");
        if state.is_some_and(|state| state.last_error_code.as_deref() == Some(expected)) {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("collector did not record {expected}");
}

async fn wait_for_collector_status(database: &DbActorHandle, expected: CollectorStatus) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let state = database
            .load_collector_states()
            .await
            .unwrap()
            .into_iter()
            .find(|state| state.collector_key == "communication.wechat");
        if state.is_some_and(|state| state.status == expected) {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("collector did not reach {expected:?}");
}

async fn wait_for_stop_quarantine(database: &DbActorHandle, revision: u64) {
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
                    && state.status == pca_domain::CollectorStatus::Degraded
                    && state.last_error_code.as_deref() == Some("WECHAT_STOP_FAILED")
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("collector records the redacted stop quarantine state");
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
