use std::{
    collections::{BTreeMap, HashSet},
    error::Error,
    fmt,
    future::Future,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{mpsc as std_mpsc, Arc},
    time::Duration,
};

use ::time::{format_description::well_known::Rfc3339, OffsetDateTime};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use pca_bridge_client::{
    NetworkObservation, NetworkObservationState, ScreenCaptureCommandHandle, ScreenCaptureStatus,
};
use pca_db_local::{
    AppliedCollectorControl, CommunicationMediaStorageStats, DbActorHandle, DbError, PairingState,
    PendingCommunicationAttachment,
};
use pca_domain::{CollectorState, CollectorStatus, CommunicationScopeV2, EventEnvelope};
use pca_keychain::{
    delete_device_credential, load_device_credential, store_device_credential, CredentialError,
    CredentialStore, DeviceCredential,
};
use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    io::AsyncReadExt,
    sync::{mpsc, oneshot, watch, Mutex},
    task::JoinHandle,
    time::{self, Instant},
};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::communication::{
    CommunicationAuthorization, CommunicationControl, CommunicationIdentity,
};

const CONTROL_INTERVAL: Duration = Duration::from_secs(30);
const COLLECTOR_HEALTH_INTERVAL: Duration = Duration::from_mins(30);
const CONTROL_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const CREDENTIAL_PERSIST_TIMEOUT: Duration = Duration::from_mins(5);
const MEDIA_UPLOAD_TIMEOUT: Duration = Duration::from_mins(5);
const PHOTO_UPLOAD_MAX_TIMEOUT: Duration = Duration::from_mins(20);
const PHOTO_UPLOAD_GRACE: Duration = Duration::from_mins(2);
const PHOTO_UPLOAD_MIN_BYTES_PER_SECOND: u64 = 512 * 1024;
const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const CLOUD_WORKER_WATCHDOG_INTERVAL: Duration = Duration::from_secs(1);
const MEDIA_CYCLE_TIMEOUT: Duration = Duration::from_mins(45);
const SCREEN_UPLOAD_BATCH_TIMEOUT: Duration = Duration::from_mins(25);
const MEDIA_BATCH_SIZE: u16 = 4;
const MAX_BACKOFF: Duration = CONTROL_INTERVAL;
const CREDENTIAL_REF: &str = "keychain://pca/device/current";
const CONTROL_OWNER_COMMAND_CAPACITY: usize = 8;
const COMMUNICATION_MEDIA_UPLOAD_FAILED: &str = "COMMUNICATION_MEDIA_UPLOAD_FAILED";
const PHOTOS_UPLOAD_FAILED: &str = "PHOTOS_UPLOAD_FAILED";
const SCREEN_UPLOAD_FAILED: &str = "SCREEN_UPLOAD_FAILED";
const SCREEN_UPLOAD_TIMEOUT: &str = "SCREEN_UPLOAD_TIMEOUT";
const MEDIA_CYCLE_TIMEOUT_ERROR: &str = "MEDIA_CYCLE_TIMEOUT";
const SCREEN_LOCAL_MANIFEST_INVALID: &str = "SCREEN_LOCAL_MANIFEST_INVALID";
const MEDIA_SOURCE_UNSUPPORTED: &str = "MEDIA_SOURCE_UNSUPPORTED";
const SYNC_PAYLOAD_REJECTED: &str = "SYNC_PAYLOAD_REJECTED";
pub const PRODUCTION_CLOUD_API_ORIGIN: &str = "https://pca-cloud-api-production.up.railway.app";

pub(crate) fn is_media_upload_error_code(error_code: Option<&str>) -> bool {
    matches!(
        error_code,
        Some(
            COMMUNICATION_MEDIA_UPLOAD_FAILED
                | PHOTOS_UPLOAD_FAILED
                | SCREEN_UPLOAD_FAILED
                | SCREEN_UPLOAD_TIMEOUT
                | MEDIA_CYCLE_TIMEOUT_ERROR
        )
    )
}

pub(crate) fn is_terminal_media_failure_code(error_code: Option<&str>) -> bool {
    matches!(
        error_code,
        Some(
            "MEDIA_LOCAL_BODY_INVALID"
                | MEDIA_SOURCE_UNSUPPORTED
                | "PHOTOS_LOCAL_MANIFEST_INVALID"
                | SCREEN_LOCAL_MANIFEST_INVALID
        )
    )
}

enum CloudWorkerExit {
    Control(Result<(), CloudControlRuntimeError>),
    Media,
    CollectorHealth,
    AppleMessages,
    Photos,
}

#[derive(Default)]
struct MediaUploadOutcome {
    completed: usize,
    successful_collectors: HashSet<&'static str>,
    failed_collectors: HashSet<&'static str>,
}

impl MediaUploadOutcome {
    fn failed(collectors: impl IntoIterator<Item = &'static str>) -> Self {
        Self {
            failed_collectors: collectors.into_iter().collect(),
            ..Self::default()
        }
    }

    fn record_success(&mut self, collector_key: &'static str) {
        self.completed = self.completed.saturating_add(1);
        self.successful_collectors.insert(collector_key);
    }

    fn record_failure(&mut self, collector_key: &'static str) {
        self.failed_collectors.insert(collector_key);
    }
}

/// Future returned by the small Cloud-control port.
pub type ControlFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ControlError>> + Send + 'a>>;

/// Future returned by the media-transfer control port with a redacted failure stage.
pub type MediaControlFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), MediaUploadFailure>> + Send + 'a>>;

/// Bounded stage at which an attachment transfer failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaUploadFailureStage {
    /// Cloud API object preparation.
    Prepare,
    /// Upload through the currently configured proxy route.
    ProxyUpload,
    /// Upload after bypassing the configured proxy route.
    DirectUpload,
    /// Cloud API object completion verification.
    Complete,
    /// A non-HTTP control-client implementation.
    Client,
}

impl MediaUploadFailureStage {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Prepare => "prepare",
            Self::ProxyUpload => "proxy_upload",
            Self::DirectUpload => "direct_upload",
            Self::Complete => "complete",
            Self::Client => "client",
        }
    }
}

/// Redacted attachment-transfer failure safe for local diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MediaUploadFailure {
    stage: MediaUploadFailureStage,
    error: ControlError,
    fallback_from: Option<MediaUploadFailureStage>,
    superseded: bool,
}

impl MediaUploadFailure {
    /// Creates a failure without a prior fallback attempt.
    #[must_use]
    pub const fn new(stage: MediaUploadFailureStage, error: ControlError) -> Self {
        Self {
            stage,
            error,
            fallback_from: None,
            superseded: false,
        }
    }

    const fn superseded() -> Self {
        Self {
            stage: MediaUploadFailureStage::Prepare,
            error: ControlError::Contract,
            fallback_from: None,
            superseded: true,
        }
    }

    const fn after_fallback(
        stage: MediaUploadFailureStage,
        error: ControlError,
        fallback_from: MediaUploadFailureStage,
    ) -> Self {
        Self {
            stage,
            error,
            fallback_from: Some(fallback_from),
            superseded: false,
        }
    }
}

fn attachment_was_superseded(failure: MediaUploadFailure) -> bool {
    failure.superseded
}

/// Failures a Cloud-control adapter can return without exposing response bodies or credentials.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlError {
    Transient,
    Revoked,
    InvalidCredential,
    Contract,
}

impl ControlError {
    const fn diagnostic_category(self) -> &'static str {
        match self {
            Self::Transient => "transient",
            Self::Revoked => "revoked",
            Self::InvalidCredential => "invalid_credential",
            Self::Contract => "contract",
        }
    }
}

/// Authenticated, bounded Cloud-control operations owned by Agent Core.
pub trait ControlClient: Send + Sync {
    fn set_network_enabled(&self, _: bool) {}

    fn network_observation_available(&self) -> Option<bool> {
        None
    }

    fn refresh<'a>(
        &'a self,
        credentials: &'a DeviceCredential,
    ) -> ControlFuture<'a, DeviceCredential>;

    fn heartbeat_and_control<'a>(
        &'a self,
        credentials: &'a DeviceCredential,
        outbox_depth: u64,
    ) -> ControlFuture<'a, AgentControlSnapshot>;

    fn heartbeat_and_control_with_media<'a>(
        &'a self,
        credentials: &'a DeviceCredential,
        outbox_depth: u64,
        _: CommunicationMediaStorageStats,
        _: Option<LocalMediaCleanupResult>,
    ) -> ControlFuture<'a, AgentControlSnapshot> {
        self.heartbeat_and_control(credentials, outbox_depth)
    }

    fn report_collector_health<'a>(
        &'a self,
        _: &'a DeviceCredential,
        _: &'a [CollectorState],
    ) -> ControlFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn sync_system_events<'a>(
        &'a self,
        _: &'a DeviceCredential,
        _: &'a [EventEnvelope],
    ) -> ControlFuture<'a, SyncEventsResponse> {
        Box::pin(async { Err(ControlError::Contract) })
    }

    fn sync_communication_events<'a>(
        &'a self,
        _: &'a DeviceCredential,
        _: &'a [EventEnvelope],
    ) -> ControlFuture<'a, SyncEventsResponse> {
        Box::pin(async { Err(ControlError::Contract) })
    }

    fn sync_communication_attachment<'a>(
        &'a self,
        _: &'a DeviceCredential,
        _: &'a PendingCommunicationAttachment,
    ) -> MediaControlFuture<'a> {
        Box::pin(async {
            Err(MediaUploadFailure::new(
                MediaUploadFailureStage::Client,
                ControlError::Contract,
            ))
        })
    }

    fn sync_screenshot<'a>(
        &'a self,
        _: &'a DeviceCredential,
        _: &'a PendingScreenshot,
    ) -> ControlFuture<'a, ()> {
        Box::pin(async { Err(ControlError::Contract) })
    }

    fn sync_photo<'a>(
        &'a self,
        _: &'a DeviceCredential,
        _: &'a PendingPhoto,
    ) -> ControlFuture<'a, ()> {
        Box::pin(async { Err(ControlError::Contract) })
    }

    fn fail_screenshot_request<'a>(
        &'a self,
        _: &'a DeviceCredential,
        _: &'a str,
        _: &'static str,
    ) -> ControlFuture<'a, ()> {
        Box::pin(async { Err(ControlError::Contract) })
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct SyncEventsResponse {
    batch_id: String,
    accepted: Vec<String>,
    duplicates: Vec<String>,
    rejected: Vec<SyncEventRejection>,
}

#[derive(Clone, Debug, Deserialize)]
struct SyncEventRejection {
    #[serde(rename = "event_id")]
    event_id: String,
    error_code: String,
    retryable: bool,
}

#[derive(Deserialize)]
struct PreparedCommunicationObject {
    object_id: String,
    state: String,
    upload: Option<PreparedCommunicationUpload>,
}

#[derive(Deserialize)]
struct PreparedCommunicationUpload {
    url: String,
    headers: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct CompletedCommunicationObject {
    object_id: String,
    state: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PendingScreenshot {
    screenshot_id: String,
    request_id: Option<String>,
    trigger: ScreenshotTrigger,
    captured_at: String,
    app_bundle_id: Option<String>,
    pixel_width: u32,
    pixel_height: u32,
    sha256: String,
    size_bytes: u64,
    mime_type: String,
    image_file_name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PendingPhoto {
    pub(crate) photo_id: String,
    pub(crate) event_id: String,
    pub(crate) asset_id: String,
    pub(crate) captured_at: String,
    pub(crate) media_type: String,
    pub(crate) original_filename: String,
    pub(crate) mime_type: String,
    pub(crate) pixel_width: u32,
    pub(crate) pixel_height: u32,
    pub(crate) duration_seconds: f64,
    pub(crate) album_names: Vec<String>,
    pub(crate) sha256: String,
    pub(crate) size_bytes: u64,
    pub(crate) media_file_name: Option<String>,
    pub(crate) completed: bool,
}

#[derive(Clone, Copy)]
pub(crate) enum PhotoMarker {
    Completed,
    Oversized,
}

impl PhotoMarker {
    fn extension(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Oversized => "oversized",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum ScreenshotTrigger {
    Manual,
    Scheduled,
    Activity,
}

#[derive(Deserialize)]
struct PreparedScreenshot {
    screenshot_id: String,
    state: String,
    upload: Option<PreparedCommunicationUpload>,
}

#[derive(Deserialize)]
struct CompletedScreenshot {
    screenshot_id: String,
    state: String,
}

#[derive(Deserialize)]
struct PreparedPhoto {
    photo_id: String,
    state: String,
    upload: Option<PreparedCommunicationUpload>,
}

#[derive(Deserialize)]
struct CompletedPhoto {
    photo_id: String,
    state: String,
}

/// Cloud pairing operations owned by Agent Core. The local Setup transport is deliberately
/// outside this port: it may only forward the typed callback result.
pub trait PairingClient: Send + Sync {
    fn create_pairing_session<'a>(
        &'a self,
        request: &'a PairingSessionRequest,
    ) -> ControlFuture<'a, PairingSessionResponse>;

    fn exchange_pairing_callback<'a>(
        &'a self,
        request: &'a PairingExchangeRequest,
    ) -> ControlFuture<'a, DeviceCredential>;
}

/// The only non-secret input Agent Core accepts from Setup before browser launch.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PairingStartHandoff {
    pub callback_uri: String,
}

/// The only values Setup needs to launch the browser and validate its local callback.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PairingSessionHandoff {
    pub session_id: String,
    pub authorization_url: String,
    pub callback_state: String,
}

/// The only one-time value Setup may return after accepting the loopback callback.
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct PairingCallbackHandoff {
    pub session_id: String,
    pub authorization_code: String,
}

impl std::fmt::Debug for PairingCallbackHandoff {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PairingCallbackHandoff")
            .field("session_id", &self.session_id)
            .field("authorization_code", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct PairingSessionRequest {
    pub device_public_key: String,
    pub existing_device_id: Option<String>,
    pub code_challenge: String,
    pub callback_uri: String,
    pub callback_state: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PairingSessionResponse {
    pub session_id: String,
    pub authorization_url: String,
}

#[derive(Clone, Serialize)]
pub struct PairingExchangeRequest {
    pub session_id: String,
    pub authorization_code: String,
    pub code_verifier: String,
}

impl std::fmt::Debug for PairingExchangeRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PairingExchangeRequest")
            .field("session_id", &self.session_id)
            .field("authorization_code", &"[redacted]")
            .field("code_verifier", &"[redacted]")
            .finish()
    }
}

#[derive(Clone)]
struct PendingPairing {
    session_id: String,
    code_verifier: String,
    credential: Option<DeviceCredential>,
}

/// In-memory owner of a single Setup pairing transaction.
///
/// A future 0600 Unix-domain-socket adapter may call these methods, but it must not receive the
/// verifier, generated device material, or resulting Keychain record.
pub struct AgentPairingService {
    database: Arc<DbActorHandle>,
    store: Arc<dyn CredentialStore>,
    client: Arc<dyn PairingClient>,
    pending: Mutex<Option<PendingPairing>>,
}

impl AgentPairingService {
    #[must_use]
    pub fn new(
        database: Arc<DbActorHandle>,
        store: Arc<dyn CredentialStore>,
        client: Arc<dyn PairingClient>,
    ) -> Self {
        Self {
            database,
            store,
            client,
            pending: Mutex::new(None),
        }
    }

    /// Creates Agent-owned PKCE/state/device material and returns only browser-safe values.
    ///
    /// # Errors
    ///
    /// Returns [`ControlError::Contract`] when the callback or Cloud response violates the
    /// pairing contract, or a Cloud-control error when session creation fails.
    pub async fn begin(
        &self,
        handoff: PairingStartHandoff,
    ) -> Result<PairingSessionHandoff, ControlError> {
        let callback_uri = Url::parse(&handoff.callback_uri).map_err(|_| ControlError::Contract)?;
        if callback_uri.scheme() != "http"
            || callback_uri.host_str() != Some("127.0.0.1")
            || callback_uri.path() != "/pca/pair/callback"
            || callback_uri.port().is_none()
        {
            return Err(ControlError::Contract);
        }
        let mut pending = self.pending.lock().await;
        if pending.is_some() {
            return Err(ControlError::Contract);
        }
        let code_verifier = random_url_safe_value();
        let callback_state = random_url_safe_value();
        let existing_device_id = self
            .database
            .load_pairing_state()
            .await
            .map_err(|_| ControlError::Transient)?
            .map(|state| state.device_id);
        let request = PairingSessionRequest {
            device_public_key: random_url_safe_value(),
            existing_device_id,
            code_challenge: pkce_challenge(&code_verifier),
            callback_uri: handoff.callback_uri,
            callback_state: callback_state.clone(),
        };
        let response = self.client.create_pairing_session(&request).await?;
        let authorization_is_https = matches!(
            Url::parse(&response.authorization_url),
            Ok(ref url) if url.scheme() == "https"
        );
        if Uuid::parse_str(&response.session_id).is_err() || !authorization_is_https {
            return Err(ControlError::Contract);
        }
        *pending = Some(PendingPairing {
            session_id: response.session_id.clone(),
            code_verifier,
            credential: None,
        });
        Ok(PairingSessionHandoff {
            session_id: response.session_id,
            authorization_url: response.authorization_url,
            callback_state,
        })
    }

    /// Consumes one callback and persists only its resulting Keychain-backed non-secret pointer.
    ///
    /// # Errors
    ///
    /// Returns an error when the callback is invalid, the Cloud exchange or Keychain operation
    /// fails, or the durable pairing state cannot be saved.
    pub async fn complete(
        &self,
        handoff: PairingCallbackHandoff,
    ) -> Result<PairingCompletion, CloudControlRuntimeError> {
        if handoff.authorization_code.is_empty() {
            return Err(CloudControlRuntimeError::Pairing(ControlError::Contract));
        }
        let pending = self
            .pending
            .lock()
            .await
            .as_ref()
            .filter(|pending| pending.session_id == handoff.session_id)
            .cloned()
            .ok_or(CloudControlRuntimeError::Pairing(ControlError::Contract))?;
        let credential = if let Some(credential) = pending.credential {
            credential
        } else {
            let credential = self
                .client
                .exchange_pairing_callback(&PairingExchangeRequest {
                    session_id: handoff.session_id.clone(),
                    authorization_code: handoff.authorization_code,
                    code_verifier: pending.code_verifier,
                })
                .await
                .map_err(CloudControlRuntimeError::Pairing)?;
            let mut current = self.pending.lock().await;
            let current = current
                .as_mut()
                .filter(|pending| pending.session_id == handoff.session_id)
                .ok_or(CloudControlRuntimeError::Pairing(ControlError::Contract))?;
            current.credential = Some(credential.clone());
            credential
        };
        store_device_credential(self.store.as_ref(), &credential)?;
        ensure_pairing_state(&self.database, &credential).await?;
        let mut current = self.pending.lock().await;
        if current
            .as_ref()
            .is_some_and(|pending| pending.session_id == handoff.session_id)
        {
            *current = None;
        }
        Ok(PairingCompletion {
            device_id: credential.device_id().to_owned(),
            workspace_id: credential.workspace_id().to_owned(),
        })
    }

    pub async fn cancel(&self, session_id: &str) {
        let mut pending = self.pending.lock().await;
        if pending
            .as_ref()
            .is_some_and(|pending| pending.session_id == session_id)
        {
            *pending = None;
        }
    }
}

/// Non-secret completion result returned to Setup after Agent-owned Keychain persistence.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PairingCompletion {
    pub device_id: String,
    pub workspace_id: String,
}

/// Keychain-validated credentials together with the only store that may mutate them.
#[derive(Clone)]
pub struct LoadedDeviceCredentials {
    credential: DeviceCredential,
    store: Arc<dyn CredentialStore>,
}

impl LoadedDeviceCredentials {
    #[must_use]
    pub fn new(credential: DeviceCredential, store: Arc<dyn CredentialStore>) -> Self {
        Self { credential, store }
    }

    #[must_use]
    pub fn credential(&self) -> &DeviceCredential {
        &self.credential
    }
}

/// The strict, revisioned remote configuration admitted by Agent Core.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentControlSnapshot {
    pub device_id: String,
    pub workspace_id: String,
    pub revoked: bool,
    pub configuration_revision: u64,
    #[serde(default)]
    pub local_media_cleanup: Option<LocalMediaCleanupRequest>,
    #[serde(default)]
    pub screenshot_request: Option<ScreenshotRequest>,
    pub collectors: CollectorControls,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalMediaCleanupRequest {
    pub request_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScreenshotRequest {
    pub request_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalMediaCleanupResult {
    request_id: String,
    status: &'static str,
    deleted_file_count: u64,
    freed_bytes: u64,
    error_code: Option<&'static str>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CollectorControls {
    pub network: EnabledControl,
    #[serde(rename = "screen.capture", default)]
    pub screen_capture: ScreenCaptureControl,
    #[serde(rename = "communication.wechat")]
    pub communication_wechat: CommunicationScopeV2,
    #[serde(rename = "communication.messages", default)]
    pub communication_messages: MessagesControl,
    #[serde(rename = "photos.library", default)]
    pub photos_library: PhotosControl,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MessagesControl {
    pub enabled: bool,
    pub directions: [String; 2],
    pub message_types: [String; 1],
    pub conversation_scope: String,
    pub initial_lookback_days: u8,
    pub sync_mode: String,
    pub attachments_enabled: bool,
    pub attachment_retention_days: u16,
}

impl Default for MessagesControl {
    fn default() -> Self {
        Self {
            enabled: false,
            directions: ["incoming".to_owned(), "outgoing".to_owned()],
            message_types: ["text".to_owned()],
            conversation_scope: "all".to_owned(),
            initial_lookback_days: 7,
            sync_mode: "full".to_owned(),
            attachments_enabled: false,
            attachment_retention_days: 7,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PhotosControl {
    pub enabled: bool,
    pub media_types: [String; 2],
    pub include_originals: bool,
    pub include_album_names: bool,
    pub initial_lookback_days: u8,
    pub cloud_retention: String,
}

impl Default for PhotosControl {
    fn default() -> Self {
        Self {
            enabled: false,
            media_types: ["image".to_owned(), "video".to_owned()],
            include_originals: true,
            include_album_names: true,
            initial_lookback_days: 60,
            cloud_retention: "permanent".to_owned(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScreenCaptureControl {
    pub enabled: bool,
    pub scheduled_enabled: bool,
    pub interval_seconds: u64,
    pub activity_enabled: bool,
    pub activity_min_interval_seconds: u64,
    pub excluded_bundle_ids: Vec<String>,
}

impl Default for ScreenCaptureControl {
    fn default() -> Self {
        Self {
            enabled: false,
            scheduled_enabled: true,
            interval_seconds: 300,
            activity_enabled: true,
            activity_min_interval_seconds: 30,
            excluded_bundle_ids: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EnabledControl {
    pub enabled: bool,
}

impl AgentControlSnapshot {
    fn validate_exact_scopes(&self) -> Result<(), ControlError> {
        if Uuid::parse_str(&self.device_id).is_err() || Uuid::parse_str(&self.workspace_id).is_err()
        {
            return Err(ControlError::Contract);
        }
        if self
            .local_media_cleanup
            .as_ref()
            .is_some_and(|request| Uuid::parse_str(&request.request_id).is_err())
            || self.collectors.communication_messages.directions != ["incoming", "outgoing"]
            || self.collectors.communication_messages.message_types != ["text"]
            || self.collectors.communication_messages.conversation_scope != "all"
            || self.collectors.communication_messages.initial_lookback_days != 7
            || self.collectors.communication_messages.sync_mode != "full"
            || self.collectors.communication_messages.attachments_enabled
            || self
                .collectors
                .communication_messages
                .attachment_retention_days
                != 7
            || self.collectors.photos_library.media_types != ["image", "video"]
            || !self.collectors.photos_library.include_originals
            || !self.collectors.photos_library.include_album_names
            || !matches!(self.collectors.photos_library.initial_lookback_days, 7 | 60)
            || self.collectors.photos_library.cloud_retention != "permanent"
        {
            return Err(ControlError::Contract);
        }
        if self
            .screenshot_request
            .as_ref()
            .is_some_and(|request| Uuid::parse_str(&request.request_id).is_err())
            || !(60..=86_400).contains(&self.collectors.screen_capture.interval_seconds)
            || !(10..=3_600).contains(&self.collectors.screen_capture.activity_min_interval_seconds)
            || self.collectors.screen_capture.excluded_bundle_ids.len() > 100
            || self
                .collectors
                .screen_capture
                .excluded_bundle_ids
                .iter()
                .any(|value| !valid_bundle_id(value))
        {
            return Err(ControlError::Contract);
        }
        Ok(())
    }
}

/// Complete, durable desired configuration. S1B intentionally does not start either source.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "fixed independent collector enable flags"
)]
pub struct AppliedControl {
    pub configuration_revision: u64,
    pub network_enabled: bool,
    pub communication_wechat_enabled: bool,
    pub communication_messages_enabled: bool,
    pub photos_library_enabled: bool,
    pub screen_capture: ScreenCaptureControl,
    pub screenshot_request_id: Option<String>,
}

impl AppliedControl {
    fn from_persisted(control: AppliedCollectorControl, configuration_revision: u64) -> Self {
        Self {
            configuration_revision,
            network_enabled: false,
            communication_wechat_enabled: control.communication_wechat_enabled,
            communication_messages_enabled: false,
            photos_library_enabled: false,
            screen_capture: ScreenCaptureControl {
                enabled: control.screen_capture_enabled,
                scheduled_enabled: control.screen_capture_scheduled_enabled,
                interval_seconds: control.screen_capture_interval_seconds,
                activity_enabled: control.screen_capture_activity_enabled,
                activity_min_interval_seconds: control.screen_capture_activity_min_interval_seconds,
                excluded_bundle_ids: control.screen_capture_excluded_bundle_ids,
            },
            screenshot_request_id: None,
        }
    }

    fn persisted(&self, credentials: &DeviceCredential) -> AppliedCollectorControl {
        AppliedCollectorControl {
            device_id: credentials.device_id().to_owned(),
            workspace_id: credentials.workspace_id().to_owned(),
            configuration_revision: self.configuration_revision,
            communication_wechat_enabled: self.communication_wechat_enabled,
            screen_capture_enabled: self.screen_capture.enabled,
            screen_capture_scheduled_enabled: self.screen_capture.scheduled_enabled,
            screen_capture_interval_seconds: self.screen_capture.interval_seconds,
            screen_capture_activity_enabled: self.screen_capture.activity_enabled,
            screen_capture_activity_min_interval_seconds: self
                .screen_capture
                .activity_min_interval_seconds,
            screen_capture_excluded_bundle_ids: self.screen_capture.excluded_bundle_ids.clone(),
            updated_at_ms: i64::try_from(
                OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000,
            )
            .unwrap_or(i64::MAX),
        }
    }
}

/// Rejects malformed scopes and ignores snapshots that cannot advance the durable revision.
///
/// # Errors
///
/// Returns [`ControlError::Contract`] when the snapshot identifiers or collector scopes are
/// malformed or unsupported.
pub fn apply_snapshot(
    current: u64,
    snapshot: &AgentControlSnapshot,
) -> Result<Option<AppliedControl>, ControlError> {
    snapshot.validate_exact_scopes()?;
    if snapshot.configuration_revision <= current {
        return Ok(None);
    }
    Ok(Some(AppliedControl {
        configuration_revision: snapshot.configuration_revision,
        network_enabled: snapshot.collectors.network.enabled,
        communication_wechat_enabled: snapshot.collectors.communication_wechat.enabled(),
        communication_messages_enabled: snapshot.collectors.communication_messages.enabled,
        photos_library_enabled: snapshot.collectors.photos_library.enabled,
        screen_capture: snapshot.collectors.screen_capture.clone(),
        screenshot_request_id: snapshot
            .screenshot_request
            .as_ref()
            .map(|request| request.request_id.clone()),
    }))
}

fn valid_bundle_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ControlState {
    unpaired: bool,
    applied_revision: Option<u64>,
    communication_hydrated: bool,
    pending_media_cleanup_result: Option<LocalMediaCleanupResult>,
}

#[derive(Clone)]
struct ScreenshotCloudContext {
    credential: DeviceCredential,
    client: Arc<dyn ControlClient>,
}

/// Starts the authenticated Cloud-control worker.
pub struct CloudControlRuntime;

/// Handle for observing and stopping the bounded Cloud-control worker.
pub struct CloudControlHandle {
    state: Arc<Mutex<ControlState>>,
    communication_controls: watch::Sender<Option<AppliedControl>>,
    publication: ControlPublication,
    owner_epoch: u64,
    shutdown: Option<watch::Sender<bool>>,
    worker: Option<JoinHandle<Result<(), CloudControlRuntimeError>>>,
}

#[derive(Clone)]
struct ControlPublication {
    state: Arc<Mutex<ControlPublicationState>>,
}

struct ControlPublicationState {
    owner_epoch: u64,
    sender: watch::Sender<Option<AppliedControl>>,
}

impl ControlPublication {
    fn new(sender: watch::Sender<Option<AppliedControl>>, owner_epoch: u64) -> Self {
        Self {
            state: Arc::new(Mutex::new(ControlPublicationState {
                owner_epoch,
                sender,
            })),
        }
    }

    async fn replace_owner(&self, owner_epoch: u64) {
        let mut state = self.state.lock().await;
        state.owner_epoch = owner_epoch;
        state.sender.send_replace(None);
    }

    async fn publish(&self, owner_epoch: u64, control: Option<AppliedControl>) -> bool {
        let state = self.state.lock().await;
        if state.owner_epoch != owner_epoch {
            return false;
        }
        state.sender.send_replace(control);
        true
    }

    async fn restore(&self, control: Option<AppliedControl>) {
        self.state.lock().await.sender.send_replace(control);
    }
}

enum CloudControlOwnerCommand {
    ReplaceIdentity {
        credentials: LoadedDeviceCredentials,
        client: Arc<dyn ControlClient>,
        response: oneshot::Sender<Result<(), CloudControlRuntimeError>>,
    },
    ReplaceFromKeychain {
        store: Arc<dyn CredentialStore>,
        client: Arc<dyn ControlClient>,
        response: oneshot::Sender<Result<bool, CloudControlRuntimeError>>,
    },
}

/// Serialized command sender for the process-lifetime Cloud-control owner.
#[derive(Clone)]
pub struct CloudControlCommands {
    commands: mpsc::Sender<CloudControlOwnerCommand>,
}

/// Process-lifetime owner of at most one joined Cloud-control worker.
pub struct CloudControlOwner {
    communication_controls: watch::Sender<Option<AppliedControl>>,
    shutdown: Option<watch::Sender<bool>>,
    worker: Option<JoinHandle<Result<(), CloudControlRuntimeError>>>,
    screen_worker: Option<JoinHandle<Result<(), CloudControlRuntimeError>>>,
}

impl CloudControlRuntime {
    /// Loads the Keychain record at Agent startup without treating credential failure as an
    /// Owner unpair decision.
    ///
    /// # Errors
    ///
    /// Returns an error when the Keychain cannot be accessed or the local pairing state cannot
    /// be synchronized or started.
    pub async fn start_from_keychain(
        database: Arc<DbActorHandle>,
        store: Arc<dyn CredentialStore>,
        client: Arc<dyn ControlClient>,
    ) -> Result<Option<CloudControlHandle>, CloudControlRuntimeError> {
        let (pairing_state_sender, _) = watch::channel(false);
        Self::start_from_keychain_with_pairing_state(database, store, client, pairing_state_sender)
            .await
    }

    /// Loads a credential and reports its paired state to the Agent lifecycle owner.
    ///
    /// # Errors
    ///
    /// Returns an error when Keychain access, state synchronization, or worker startup fails.
    pub async fn start_from_keychain_with_pairing_state(
        database: Arc<DbActorHandle>,
        store: Arc<dyn CredentialStore>,
        client: Arc<dyn ControlClient>,
        pairing_state_sender: watch::Sender<bool>,
    ) -> Result<Option<CloudControlHandle>, CloudControlRuntimeError> {
        Self::start_from_keychain_with_pairing_state_and_authorization(
            database,
            store,
            client,
            pairing_state_sender,
            CommunicationAuthorization::new(),
        )
        .await
    }

    /// Loads Keychain state and starts control over the App-owned communication gate.
    ///
    /// # Errors
    ///
    /// Returns an error when Keychain access, pairing reconciliation, or worker startup fails.
    pub async fn start_from_keychain_with_pairing_state_and_authorization(
        database: Arc<DbActorHandle>,
        store: Arc<dyn CredentialStore>,
        client: Arc<dyn ControlClient>,
        pairing_state_sender: watch::Sender<bool>,
        authorization: CommunicationAuthorization,
    ) -> Result<Option<CloudControlHandle>, CloudControlRuntimeError> {
        if !synchronize_pairing_state_with_authorization(&database, store.as_ref(), &authorization)
            .await?
        {
            pairing_state_sender.send_replace(false);
            return Ok(None);
        }
        pairing_state_sender.send_replace(true);
        let credential = match load_device_credential(store.as_ref()) {
            Ok(Some(credential)) => credential,
            Ok(None) | Err(CredentialError::InvalidCredential) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Self::start_with_pairing_state_and_authorization(
            database,
            LoadedDeviceCredentials::new(credential, store),
            client,
            pairing_state_sender,
            authorization,
        )
        .await
        .map(Some)
    }

    /// Validates the local non-secret pointer and begins an immediate control request.
    ///
    /// # Errors
    ///
    /// Returns an error when the local pairing state cannot be validated or persisted.
    pub async fn start(
        database: Arc<DbActorHandle>,
        credentials: LoadedDeviceCredentials,
        client: Arc<dyn ControlClient>,
    ) -> Result<CloudControlHandle, CloudControlRuntimeError> {
        let (pairing_state_sender, _) = watch::channel(false);
        Self::start_with_pairing_state_and_authorization(
            database,
            credentials,
            client,
            pairing_state_sender,
            CommunicationAuthorization::new(),
        )
        .await
    }

    /// Validates local state, starts control, and reports paired/revoked transitions.
    ///
    /// # Errors
    ///
    /// Returns an error when local state cannot be persisted or the worker cannot start.
    pub async fn start_with_pairing_state(
        database: Arc<DbActorHandle>,
        credentials: LoadedDeviceCredentials,
        client: Arc<dyn ControlClient>,
        pairing_state_sender: watch::Sender<bool>,
    ) -> Result<CloudControlHandle, CloudControlRuntimeError> {
        Self::start_with_pairing_state_and_authorization(
            database,
            credentials,
            client,
            pairing_state_sender,
            CommunicationAuthorization::new(),
        )
        .await
    }

    /// Starts Cloud control over the App-owned communication authorization gate.
    ///
    /// # Errors
    ///
    /// Returns an error when local pairing state cannot be validated or persisted.
    pub async fn start_with_pairing_state_and_authorization(
        database: Arc<DbActorHandle>,
        credentials: LoadedDeviceCredentials,
        client: Arc<dyn ControlClient>,
        pairing_state_sender: watch::Sender<bool>,
        authorization: CommunicationAuthorization,
    ) -> Result<CloudControlHandle, CloudControlRuntimeError> {
        let owner_epoch = authorization.owner_epoch().await;
        let (communication_controls, _) = watch::channel(None);
        let publication = ControlPublication::new(communication_controls.clone(), owner_epoch);
        start_control_worker(
            database,
            credentials,
            client,
            pairing_state_sender,
            authorization,
            communication_controls,
            publication,
            owner_epoch,
            None,
            None,
        )
        .await
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the worker receives the process-owned authorization and publication epoch"
)]
async fn start_control_worker(
    database: Arc<DbActorHandle>,
    credentials: LoadedDeviceCredentials,
    client: Arc<dyn ControlClient>,
    pairing_state_sender: watch::Sender<bool>,
    authorization: CommunicationAuthorization,
    communication_controls: watch::Sender<Option<AppliedControl>>,
    publication: ControlPublication,
    owner_epoch: u64,
    screen_capture: Option<ScreenCaptureCommandHandle>,
    screenshot_cloud_context: Option<watch::Sender<Option<ScreenshotCloudContext>>>,
) -> Result<CloudControlHandle, CloudControlRuntimeError> {
    let pairing_revision = ensure_pairing_state(&database, credentials.credential()).await?;
    let applied_revision = complete_persisted_revision(
        credentials.credential(),
        pairing_revision,
        database.load_applied_collector_control().await?.as_ref(),
    );
    pairing_state_sender.send_replace(true);
    let state = Arc::new(Mutex::new(ControlState {
        unpaired: false,
        applied_revision: Some(applied_revision),
        communication_hydrated: false,
        pending_media_cleanup_result: None,
    }));
    let (shutdown_sender, shutdown_receiver) = watch::channel(false);
    let worker = tokio::spawn(run_cloud_workers(
        database,
        credentials,
        client,
        Arc::clone(&state),
        shutdown_receiver,
        pairing_state_sender,
        publication.clone(),
        authorization.clone(),
        owner_epoch,
        communication_controls.subscribe(),
        screen_capture,
        screenshot_cloud_context,
    ));
    Ok(CloudControlHandle {
        state,
        communication_controls,
        publication,
        owner_epoch,
        shutdown: Some(shutdown_sender),
        worker: Some(worker),
    })
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the joined Cloud runtime owns one control worker and one independent media worker"
)]
async fn run_cloud_workers(
    database: Arc<DbActorHandle>,
    credentials: LoadedDeviceCredentials,
    client: Arc<dyn ControlClient>,
    state: Arc<Mutex<ControlState>>,
    shutdown: watch::Receiver<bool>,
    pairing_state_sender: watch::Sender<bool>,
    publication: ControlPublication,
    authorization: CommunicationAuthorization,
    owner_epoch: u64,
    screen_controls: watch::Receiver<Option<AppliedControl>>,
    screen_capture: Option<ScreenCaptureCommandHandle>,
    screenshot_cloud_context: Option<watch::Sender<Option<ScreenshotCloudContext>>>,
) -> Result<(), CloudControlRuntimeError> {
    let (media_credentials, media_credential_receiver) =
        watch::channel(credentials.credential.clone());
    let (media_shutdown, media_shutdown_receiver) = watch::channel(false);
    let screenshot_media_enabled = screen_capture.is_some();
    let mut media_worker = tokio::spawn(run_data_plane_workers(
        Arc::clone(&database),
        media_credential_receiver,
        Arc::clone(&client),
        media_shutdown_receiver,
        screenshot_media_enabled,
    ));
    let mut collector_health_worker = tokio::spawn(run_collector_health_loop(
        Arc::clone(&database),
        media_credentials.subscribe(),
        screen_controls.clone(),
        Arc::clone(&client),
        media_shutdown.subscribe(),
    ));
    let mut apple_worker = screen_capture.as_ref().map(|bridge| {
        tokio::spawn(crate::apple_messages::run(
            Arc::clone(&database),
            bridge.clone(),
            credentials.credential().workspace_id().to_owned(),
            credentials.credential().device_id().to_owned(),
            screen_controls.clone(),
            media_shutdown.subscribe(),
        ))
    });
    let mut photo_worker = screen_capture.as_ref().map(|bridge| {
        tokio::spawn(crate::apple_photos::run(
            Arc::clone(&database),
            bridge.clone(),
            credentials.credential().workspace_id().to_owned(),
            credentials.credential().device_id().to_owned(),
            screen_controls.clone(),
            media_shutdown.subscribe(),
        ))
    });
    let control = run_control_loop(
        database,
        credentials,
        client,
        state,
        shutdown,
        pairing_state_sender,
        publication,
        authorization,
        owner_epoch,
        media_credentials,
        screenshot_cloud_context,
    );
    tokio::pin!(control);
    let worker_exit = tokio::select! {
        biased;
        result = &mut control => CloudWorkerExit::Control(result),
        _ = &mut media_worker => CloudWorkerExit::Media,
        _ = &mut collector_health_worker => CloudWorkerExit::CollectorHealth,
        () = wait_for_optional_worker(&mut apple_worker) => CloudWorkerExit::AppleMessages,
        () = wait_for_optional_worker(&mut photo_worker) => CloudWorkerExit::Photos,
    };
    let media_completed = matches!(worker_exit, CloudWorkerExit::Media);
    let collector_health_completed = matches!(worker_exit, CloudWorkerExit::CollectorHealth);
    let apple_completed = matches!(worker_exit, CloudWorkerExit::AppleMessages);
    let photo_completed = matches!(worker_exit, CloudWorkerExit::Photos);
    let control_result = match worker_exit {
        CloudWorkerExit::Control(result) => result,
        _ => Err(CloudControlRuntimeError::WorkerStopped),
    };
    media_shutdown.send_replace(true);
    collector_health_worker.abort();
    if let Some(worker) = &apple_worker {
        worker.abort();
    }
    if let Some(worker) = &photo_worker {
        worker.abort();
    }
    let media_result = if media_completed {
        Ok(())
    } else {
        match time::timeout(RUNTIME_SHUTDOWN_TIMEOUT, &mut media_worker).await {
            Ok(Ok(result)) => result,
            Ok(Err(error)) if error.is_cancelled() => Ok(()),
            Ok(Err(_)) => Err(CloudControlRuntimeError::WorkerStopped),
            Err(_) => {
                media_worker.abort();
                let _ = media_worker.await;
                Err(CloudControlRuntimeError::WorkerStopped)
            }
        }
    };
    let collector_health_result = if collector_health_completed {
        Ok(())
    } else {
        match collector_health_worker.await {
            Ok(result) => result,
            Err(error) if error.is_cancelled() => Ok(()),
            Err(_) => Err(CloudControlRuntimeError::WorkerStopped),
        }
    };
    control_result?;
    media_result?;
    collector_health_result?;
    if !apple_completed {
        if let Some(worker) = apple_worker {
            let _ = worker.await;
        }
    }
    if !photo_completed {
        if let Some(worker) = photo_worker {
            let _ = worker.await;
        }
    }
    Ok(())
}

async fn wait_for_optional_worker<T>(worker: &mut Option<JoinHandle<T>>) {
    if let Some(worker) = worker {
        let _ = worker.await;
    } else {
        std::future::pending::<()>().await;
    }
}

impl CloudControlOwner {
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.worker
            .as_ref()
            .is_none_or(tokio::task::JoinHandle::is_finished)
            || self
                .screen_worker
                .as_ref()
                .is_some_and(tokio::task::JoinHandle::is_finished)
    }

    #[must_use]
    pub fn start(
        database: Arc<DbActorHandle>,
        pairing_state_sender: watch::Sender<bool>,
        authorization: CommunicationAuthorization,
    ) -> (Self, CloudControlCommands) {
        Self::start_owner(database, pairing_state_sender, authorization, None)
    }

    #[must_use]
    pub fn start_with_screen_capture(
        database: Arc<DbActorHandle>,
        pairing_state_sender: watch::Sender<bool>,
        authorization: CommunicationAuthorization,
        screen_capture: ScreenCaptureCommandHandle,
    ) -> (Self, CloudControlCommands) {
        Self::start_owner(
            database,
            pairing_state_sender,
            authorization,
            Some(screen_capture),
        )
    }

    fn start_owner(
        database: Arc<DbActorHandle>,
        pairing_state_sender: watch::Sender<bool>,
        authorization: CommunicationAuthorization,
        screen_capture: Option<ScreenCaptureCommandHandle>,
    ) -> (Self, CloudControlCommands) {
        let (communication_controls, _) = watch::channel(None);
        let publication = ControlPublication::new(communication_controls.clone(), 0);
        let (commands, command_receiver) = mpsc::channel(CONTROL_OWNER_COMMAND_CAPACITY);
        let (shutdown, shutdown_receiver) = watch::channel(false);
        let (screenshot_cloud_context, screenshot_cloud_context_receiver) = watch::channel(None);
        let screen_worker = screen_capture.as_ref().map(|bridge| {
            tokio::spawn(run_screenshot_loop(
                Arc::clone(&database),
                communication_controls.subscribe(),
                screenshot_cloud_context_receiver,
                bridge.clone(),
                shutdown.subscribe(),
            ))
        });
        let worker = tokio::spawn(run_control_owner(
            database,
            pairing_state_sender,
            authorization,
            communication_controls.clone(),
            publication,
            screen_capture,
            screenshot_cloud_context,
            command_receiver,
            shutdown_receiver,
        ));
        (
            Self {
                communication_controls,
                shutdown: Some(shutdown),
                worker: Some(worker),
                screen_worker,
            },
            CloudControlCommands { commands },
        )
    }

    #[must_use]
    pub fn communication_controls(&self) -> watch::Receiver<Option<AppliedControl>> {
        self.communication_controls.subscribe()
    }

    /// Stops and joins the current Cloud worker and its process-lifetime owner.
    ///
    /// # Errors
    ///
    /// Returns a redacted owner or worker lifecycle failure.
    pub async fn shutdown(mut self) -> Result<(), CloudControlRuntimeError> {
        if let Some(shutdown) = self.shutdown.take() {
            shutdown.send_replace(true);
        }
        let owner_result = match self.worker.take() {
            Some(worker) => stop_cloud_worker(worker).await,
            None => Ok(()),
        };
        let screen_result = match self.screen_worker.take() {
            Some(worker) => stop_local_screen_worker(worker).await,
            None => Ok(()),
        };
        owner_result.and(screen_result)
    }
}

async fn stop_local_screen_worker(
    mut worker: JoinHandle<Result<(), CloudControlRuntimeError>>,
) -> Result<(), CloudControlRuntimeError> {
    if worker.is_finished() {
        return worker
            .await
            .map_err(|_| CloudControlRuntimeError::WorkerStopped)?;
    }
    let (cancel_deadline, deadline_cancelled) = std_mpsc::channel();
    let mut deadline = tokio::task::spawn_blocking(move || {
        matches!(
            deadline_cancelled.recv_timeout(RUNTIME_SHUTDOWN_TIMEOUT),
            Err(std_mpsc::RecvTimeoutError::Timeout)
        )
    });
    tokio::select! {
        result = &mut worker => {
            let _ = cancel_deadline.send(());
            let _ = deadline.await;
            result.map_err(|_| CloudControlRuntimeError::WorkerStopped)?
        }
        _ = &mut deadline => {
            worker.abort();
            let _ = worker.await;
            Ok(())
        }
    }
}

impl CloudControlCommands {
    /// Serially replaces the active identity after the prior worker has fully joined.
    ///
    /// # Errors
    ///
    /// Returns a redacted lifecycle failure without starting an overlapping worker.
    pub async fn replace_identity(
        &self,
        credentials: LoadedDeviceCredentials,
        client: Arc<dyn ControlClient>,
    ) -> Result<(), CloudControlRuntimeError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(CloudControlOwnerCommand::ReplaceIdentity {
                credentials,
                client,
                response,
            })
            .await
            .map_err(|_| CloudControlRuntimeError::WorkerStopped)?;
        receiver
            .await
            .map_err(|_| CloudControlRuntimeError::WorkerStopped)?
    }

    /// Reconciles Keychain state when no healthy worker is currently owned.
    ///
    /// # Errors
    ///
    /// Returns a redacted Keychain, database, or lifecycle failure.
    pub async fn replace_from_keychain(
        &self,
        store: Arc<dyn CredentialStore>,
        client: Arc<dyn ControlClient>,
    ) -> Result<bool, CloudControlRuntimeError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(CloudControlOwnerCommand::ReplaceFromKeychain {
                store,
                client,
                response,
            })
            .await
            .map_err(|_| CloudControlRuntimeError::WorkerStopped)?;
        receiver
            .await
            .map_err(|_| CloudControlRuntimeError::WorkerStopped)?
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the owner serializes one worker over its shared lifecycle dependencies"
)]
async fn run_control_owner(
    database: Arc<DbActorHandle>,
    pairing_state_sender: watch::Sender<bool>,
    authorization: CommunicationAuthorization,
    communication_controls: watch::Sender<Option<AppliedControl>>,
    publication: ControlPublication,
    screen_capture: Option<ScreenCaptureCommandHandle>,
    screenshot_cloud_context: watch::Sender<Option<ScreenshotCloudContext>>,
    mut commands: mpsc::Receiver<CloudControlOwnerCommand>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), CloudControlRuntimeError> {
    let mut current = None;
    restore_local_control_publication(&database, &publication).await?;
    let mut watchdog = time::interval(CLOUD_WORKER_WATCHDOG_INTERVAL);
    watchdog.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow_and_update() {
                    break;
                }
            }
            command = commands.recv() => {
                let Some(command) = command else {
                    break;
                };
                match command {
                    CloudControlOwnerCommand::ReplaceIdentity {
                        credentials,
                        client,
                        response,
                    } => {
                        let result = replace_owned_control(
                            &database,
                            credentials,
                            client,
                            &pairing_state_sender,
                            &authorization,
                            &communication_controls,
                            &publication,
                            screen_capture.clone(),
                            &screenshot_cloud_context,
                            &mut current,
                        )
                        .await;
                        let _ = response.send(result);
                    }
                    CloudControlOwnerCommand::ReplaceFromKeychain {
                        store,
                        client,
                        response,
                    } => {
                        let result = replace_owned_control_from_keychain(
                            &database,
                            store,
                            client,
                            &pairing_state_sender,
                            &authorization,
                            &communication_controls,
                            &publication,
                            screen_capture.clone(),
                            &screenshot_cloud_context,
                            &mut current,
                        )
                        .await;
                        let _ = response.send(result);
                    }
                }
            }
            _ = watchdog.tick() => {
                if current.as_ref().is_some_and(CloudControlHandle::is_finished) {
                    let worker = current.take().expect("finished Cloud worker exists");
                    worker.shutdown().await?;
                }
            }
        }
    }

    invalidate_and_stop_owned_control(&authorization, &publication, &mut current)
        .await
        .map(|_| ())
}

async fn restore_local_control_publication(
    database: &DbActorHandle,
    publication: &ControlPublication,
) -> Result<(), CloudControlRuntimeError> {
    let pairing = database.load_pairing_state().await?;
    let control = database.load_applied_collector_control().await?;
    let restored = match (pairing, control) {
        (Some(pairing), Some(control))
            if pairing.is_paired()
                && pairing.device_id == control.device_id
                && pairing.workspace_id == control.workspace_id
                && (pairing.applied_control_revision == control.configuration_revision
                    || control.is_legacy_bootstrap()) =>
        {
            Some(AppliedControl::from_persisted(
                control,
                pairing.applied_control_revision,
            ))
        }
        _ => None,
    };
    publication.restore(restored).await;
    Ok(())
}

fn complete_persisted_revision(
    credentials: &DeviceCredential,
    pairing_revision: u64,
    control: Option<&AppliedCollectorControl>,
) -> u64 {
    control
        .filter(|control| {
            !control.is_legacy_bootstrap()
                && control.device_id == credentials.device_id()
                && control.workspace_id == credentials.workspace_id()
                && control.configuration_revision == pairing_revision
        })
        .map_or(0, |control| control.configuration_revision)
}

async fn invalidate_and_stop_owned_control(
    authorization: &CommunicationAuthorization,
    publication: &ControlPublication,
    current: &mut Option<CloudControlHandle>,
) -> Result<u64, CloudControlRuntimeError> {
    let owner_epoch = authorization.replace_owner().await;
    publication.replace_owner(owner_epoch).await;
    if let Some(worker) = current.take() {
        worker.shutdown().await?;
    }
    Ok(owner_epoch)
}

#[allow(
    clippy::too_many_arguments,
    reason = "identity handoff uses the owner-shared lifecycle dependencies"
)]
async fn replace_owned_control(
    database: &Arc<DbActorHandle>,
    credentials: LoadedDeviceCredentials,
    client: Arc<dyn ControlClient>,
    pairing_state_sender: &watch::Sender<bool>,
    authorization: &CommunicationAuthorization,
    communication_controls: &watch::Sender<Option<AppliedControl>>,
    publication: &ControlPublication,
    screen_capture: Option<ScreenCaptureCommandHandle>,
    screenshot_cloud_context: &watch::Sender<Option<ScreenshotCloudContext>>,
    current: &mut Option<CloudControlHandle>,
) -> Result<(), CloudControlRuntimeError> {
    let owner_epoch =
        invalidate_and_stop_owned_control(authorization, publication, current).await?;
    screenshot_cloud_context.send_replace(Some(ScreenshotCloudContext {
        credential: credentials.credential.clone(),
        client: Arc::clone(&client),
    }));
    *current = Some(
        start_control_worker(
            Arc::clone(database),
            credentials,
            client,
            pairing_state_sender.clone(),
            authorization.clone(),
            communication_controls.clone(),
            publication.clone(),
            owner_epoch,
            screen_capture,
            Some(screenshot_cloud_context.clone()),
        )
        .await?,
    );
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "startup reconciliation uses the owner-shared lifecycle dependencies"
)]
async fn replace_owned_control_from_keychain(
    database: &Arc<DbActorHandle>,
    store: Arc<dyn CredentialStore>,
    client: Arc<dyn ControlClient>,
    pairing_state_sender: &watch::Sender<bool>,
    authorization: &CommunicationAuthorization,
    communication_controls: &watch::Sender<Option<AppliedControl>>,
    publication: &ControlPublication,
    screen_capture: Option<ScreenCaptureCommandHandle>,
    screenshot_cloud_context: &watch::Sender<Option<ScreenshotCloudContext>>,
    current: &mut Option<CloudControlHandle>,
) -> Result<bool, CloudControlRuntimeError> {
    if current.as_ref().is_some_and(|worker| !worker.is_finished()) {
        return Ok(true);
    }
    if let Some(worker) = current.take() {
        let _ = worker.shutdown().await;
    }
    if !synchronize_pairing_state_with_authorization(database, store.as_ref(), authorization)
        .await?
    {
        pairing_state_sender.send_replace(false);
        return Ok(false);
    }
    pairing_state_sender.send_replace(true);
    let credential = match load_device_credential(store.as_ref()) {
        Ok(Some(credential)) => credential,
        Ok(None) | Err(CredentialError::InvalidCredential) => {
            restore_local_control_publication(database, publication).await?;
            return Ok(false);
        }
        Err(error) => return Err(error.into()),
    };
    let owner_epoch = authorization.advance_owner_preserving_control().await;
    publication.replace_owner(owner_epoch).await;
    screenshot_cloud_context.send_replace(Some(ScreenshotCloudContext {
        credential: credential.clone(),
        client: Arc::clone(&client),
    }));
    *current = Some(
        start_control_worker(
            Arc::clone(database),
            LoadedDeviceCredentials::new(credential, store),
            client,
            pairing_state_sender.clone(),
            authorization.clone(),
            communication_controls.clone(),
            publication.clone(),
            owner_epoch,
            screen_capture,
            Some(screenshot_cloud_context.clone()),
        )
        .await?,
    );
    Ok(true)
}

/// Reconciles the non-secret `SQLite` pointer with the Keychain record at Agent startup.
///
/// Only the durable manual-unpair marker makes the Agent unpaired. Credential failures affect
/// Cloud connectivity without changing the Owner's pairing decision.
///
/// # Errors
///
/// Returns an error when the Keychain cannot be accessed or the local pairing state cannot be
/// synchronized.
pub async fn synchronize_pairing_state(
    database: &DbActorHandle,
    store: &dyn CredentialStore,
) -> Result<bool, CloudControlRuntimeError> {
    synchronize_pairing_state_with_authorization(
        database,
        store,
        &CommunicationAuthorization::new(),
    )
    .await
}

pub(crate) async fn synchronize_pairing_state_with_authorization(
    database: &DbActorHandle,
    store: &dyn CredentialStore,
    authorization: &CommunicationAuthorization,
) -> Result<bool, CloudControlRuntimeError> {
    let durable = database.load_pairing_state().await?;
    if durable
        .as_ref()
        .is_some_and(|state| state.manually_unpaired)
    {
        authorization.disable().await;
        return Ok(false);
    }
    match load_device_credential(store) {
        Ok(Some(credential)) => {
            ensure_pairing_state(database, &credential).await?;
            Ok(true)
        }
        Ok(None) | Err(CredentialError::InvalidCredential) => Ok(durable.is_some()),
        Err(error) => Err(error.into()),
    }
}

impl CloudControlHandle {
    /// Returns whether the joined Cloud worker has already stopped.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.worker
            .as_ref()
            .is_none_or(tokio::task::JoinHandle::is_finished)
    }

    #[must_use]
    pub async fn is_unpaired(&self) -> bool {
        self.state.lock().await.unpaired
    }

    #[must_use]
    pub async fn applied_revision(&self) -> Option<u64> {
        self.state.lock().await.applied_revision
    }

    /// Subscribes to validated, monotonic communication control revisions.
    #[must_use]
    pub fn communication_controls(&self) -> watch::Receiver<Option<AppliedControl>> {
        self.communication_controls.subscribe()
    }

    /// Stops the control worker and cancels any independently retryable media transfer.
    ///
    /// # Errors
    ///
    /// Returns an error if the worker ends unexpectedly or reports a runtime failure.
    pub async fn shutdown(mut self) -> Result<(), CloudControlRuntimeError> {
        self.publication.publish(self.owner_epoch, None).await;
        if let Some(shutdown) = self.shutdown.take() {
            shutdown.send_replace(true);
        }
        match self.worker.take() {
            Some(worker) => stop_cloud_worker(worker).await,
            None => Ok(()),
        }
    }
}

async fn stop_cloud_worker(
    mut worker: JoinHandle<Result<(), CloudControlRuntimeError>>,
) -> Result<(), CloudControlRuntimeError> {
    let (cancel_deadline, deadline_cancelled) = std_mpsc::channel();
    let mut deadline = tokio::task::spawn_blocking(move || {
        matches!(
            deadline_cancelled.recv_timeout(RUNTIME_SHUTDOWN_TIMEOUT),
            Err(std_mpsc::RecvTimeoutError::Timeout)
        )
    });
    tokio::select! {
        biased;
        result = &mut worker => {
            let _ = cancel_deadline.send(());
            let _ = deadline.await;
            result.map_err(|_| CloudControlRuntimeError::WorkerStopped)?
        }
        deadline_result = &mut deadline => {
            let timed_out = deadline_result.unwrap_or(true);
            if !timed_out {
                return worker.await.map_err(|_| CloudControlRuntimeError::WorkerStopped)?;
            }
            worker.abort();
            let _ = worker.await;
            Err(CloudControlRuntimeError::WorkerStopped)
        }
    }
}

#[derive(Debug)]
pub enum CloudControlRuntimeError {
    Database(DbError),
    Keychain(CredentialError),
    Pairing(ControlError),
    WorkerStopped,
}

impl fmt::Display for CloudControlRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "cloud control database operation: {error}"),
            Self::Keychain(error) => write!(formatter, "cloud control Keychain operation: {error}"),
            Self::Pairing(error) => write!(formatter, "cloud pairing operation: {error:?}"),
            Self::WorkerStopped => formatter.write_str("cloud control worker stopped"),
        }
    }
}

impl Error for CloudControlRuntimeError {}

impl From<DbError> for CloudControlRuntimeError {
    fn from(error: DbError) -> Self {
        Self::Database(error)
    }
}

impl From<CredentialError> for CloudControlRuntimeError {
    fn from(error: CredentialError) -> Self {
        Self::Keychain(error)
    }
}

async fn ensure_pairing_state(
    database: &DbActorHandle,
    credentials: &DeviceCredential,
) -> Result<u64, CloudControlRuntimeError> {
    let existing = database.load_pairing_state().await?;
    let revision = existing
        .as_ref()
        .filter(|state| {
            state.device_id == credentials.device_id()
                && state.workspace_id == credentials.workspace_id()
        })
        .map_or(0, |state| state.applied_control_revision);
    let mut state = PairingState::paired(
        credentials.device_id(),
        credentials.workspace_id(),
        CREDENTIAL_REF,
        credentials.credential_generation(),
        PRODUCTION_CLOUD_API_ORIGIN,
    );
    state.applied_control_revision = revision;
    database.save_pairing_state(&state).await?;
    Ok(revision)
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the control owner receives one shared authorization gate and its existing runtime dependencies"
)]
async fn run_control_loop(
    database: Arc<DbActorHandle>,
    mut credentials: LoadedDeviceCredentials,
    client: Arc<dyn ControlClient>,
    state: Arc<Mutex<ControlState>>,
    mut shutdown: watch::Receiver<bool>,
    pairing_state_sender: watch::Sender<bool>,
    publication: ControlPublication,
    authorization: CommunicationAuthorization,
    owner_epoch: u64,
    media_credentials: watch::Sender<DeviceCredential>,
    screenshot_cloud_context: Option<watch::Sender<Option<ScreenshotCloudContext>>>,
) -> Result<(), CloudControlRuntimeError> {
    let mut retry_attempt = 0_u8;
    let mut wait = Duration::ZERO;
    loop {
        if wait != Duration::ZERO && wait_or_shutdown(wait, &mut shutdown).await {
            return Ok(());
        }
        if *shutdown.borrow() {
            return Ok(());
        }

        match control_once(
            &database,
            &mut credentials,
            client.as_ref(),
            &state,
            &publication,
            &authorization,
            owner_epoch,
        )
        .await
        {
            Ok(()) => {
                retry_attempt = 0;
                wait = CONTROL_INTERVAL;
            }
            Err(ControlError::Transient | ControlError::Contract) => {
                retry_attempt = retry_attempt.saturating_add(1);
                wait = retry_delay(retry_attempt);
            }
            Err(ControlError::InvalidCredential) => {
                match client.refresh(&credentials.credential).await {
                    Ok(next) => {
                        if next.device_id() != credentials.credential.device_id()
                            || next.workspace_id() != credentials.credential.workspace_id()
                        {
                            retry_attempt = retry_attempt.saturating_add(1);
                            wait = retry_delay(retry_attempt);
                            continue;
                        }
                        match time::timeout(
                            CREDENTIAL_PERSIST_TIMEOUT,
                            persist_refreshed_credential(
                                &database,
                                &mut credentials,
                                next,
                                &media_credentials,
                                screenshot_cloud_context.as_ref(),
                                Arc::clone(&client),
                                &mut shutdown,
                            ),
                        )
                        .await
                        {
                            Ok(true) => {}
                            Ok(false) => return Ok(()),
                            Err(_) => return Err(CloudControlRuntimeError::WorkerStopped),
                        }
                        retry_attempt = 0;
                        wait = Duration::ZERO;
                    }
                    Err(ControlError::Revoked) => {
                        return mark_manually_unpaired(
                            &database,
                            &credentials,
                            &state,
                            &pairing_state_sender,
                            &publication,
                            &authorization,
                            owner_epoch,
                        )
                        .await;
                    }
                    Err(
                        ControlError::InvalidCredential
                        | ControlError::Transient
                        | ControlError::Contract,
                    ) => {
                        retry_attempt = retry_attempt.saturating_add(1);
                        wait = retry_delay(retry_attempt);
                    }
                }
            }
            Err(ControlError::Revoked) => {
                return mark_manually_unpaired(
                    &database,
                    &credentials,
                    &state,
                    &pairing_state_sender,
                    &publication,
                    &authorization,
                    owner_epoch,
                )
                .await;
            }
        }
    }
}

async fn persist_refreshed_credential(
    database: &DbActorHandle,
    credentials: &mut LoadedDeviceCredentials,
    next: DeviceCredential,
    media_credentials: &watch::Sender<DeviceCredential>,
    screenshot_cloud_context: Option<&watch::Sender<Option<ScreenshotCloudContext>>>,
    client: Arc<dyn ControlClient>,
    shutdown: &mut watch::Receiver<bool>,
) -> bool {
    let mut retry_attempt = 0_u8;
    let mut keychain_persisted = false;
    loop {
        if !keychain_persisted {
            keychain_persisted = store_device_credential(credentials.store.as_ref(), &next).is_ok();
        }
        if keychain_persisted && ensure_pairing_state(database, &next).await.is_ok() {
            media_credentials.send_replace(next.clone());
            if let Some(context) = screenshot_cloud_context {
                context.send_replace(Some(ScreenshotCloudContext {
                    credential: next.clone(),
                    client: Arc::clone(&client),
                }));
            }
            credentials.credential = next;
            return true;
        }
        retry_attempt = retry_attempt.saturating_add(1);
        if wait_or_shutdown(retry_delay(retry_attempt), shutdown).await {
            return false;
        }
    }
}

async fn control_once(
    database: &DbActorHandle,
    credentials: &mut LoadedDeviceCredentials,
    client: &dyn ControlClient,
    state: &Arc<Mutex<ControlState>>,
    publication: &ControlPublication,
    authorization: &CommunicationAuthorization,
    owner_epoch: u64,
) -> Result<(), ControlError> {
    let snapshot = send_control_heartbeat(database, credentials, client, state).await?;
    if snapshot.revoked {
        client.set_network_enabled(false);
        return Err(ControlError::Revoked);
    }
    if snapshot.device_id != credentials.credential.device_id()
        || snapshot.workspace_id != credentials.credential.workspace_id()
    {
        return Err(ControlError::Contract);
    }
    apply_control_snapshot(
        database,
        credentials,
        client,
        state,
        publication,
        authorization,
        owner_epoch,
        snapshot,
    )
    .await
}

async fn send_control_heartbeat(
    database: &DbActorHandle,
    credentials: &LoadedDeviceCredentials,
    client: &dyn ControlClient,
    state: &Arc<Mutex<ControlState>>,
) -> Result<AgentControlSnapshot, ControlError> {
    let outbox_depth = database
        .active_outbox_depth()
        .await
        .map_err(|_| ControlError::Transient)?;
    let media_stats = database
        .communication_media_storage_stats()
        .await
        .map_err(|_| ControlError::Transient)?;
    let pending_cleanup_result = state.lock().await.pending_media_cleanup_result.clone();
    let snapshot = client
        .heartbeat_and_control_with_media(
            &credentials.credential,
            outbox_depth,
            media_stats,
            pending_cleanup_result.clone(),
        )
        .await?;
    if let Some(acknowledged) = pending_cleanup_result {
        let mut state = state.lock().await;
        if state
            .pending_media_cleanup_result
            .as_ref()
            .is_some_and(|current| current.request_id == acknowledged.request_id)
        {
            state.pending_media_cleanup_result = None;
        }
    }
    Ok(snapshot)
}

#[allow(
    clippy::too_many_arguments,
    reason = "one validated control snapshot is applied against its existing runtime dependencies"
)]
async fn apply_control_snapshot(
    database: &DbActorHandle,
    credentials: &LoadedDeviceCredentials,
    client: &dyn ControlClient,
    state: &Arc<Mutex<ControlState>>,
    publication: &ControlPublication,
    authorization: &CommunicationAuthorization,
    owner_epoch: u64,
    snapshot: AgentControlSnapshot,
) -> Result<(), ControlError> {
    snapshot.validate_exact_scopes()?;
    let observed_control = AppliedControl {
        configuration_revision: snapshot.configuration_revision,
        network_enabled: snapshot.collectors.network.enabled,
        communication_wechat_enabled: snapshot.collectors.communication_wechat.enabled(),
        communication_messages_enabled: snapshot.collectors.communication_messages.enabled,
        photos_library_enabled: snapshot.collectors.photos_library.enabled,
        screen_capture: snapshot.collectors.screen_capture.clone(),
        screenshot_request_id: snapshot
            .screenshot_request
            .as_ref()
            .map(|request| request.request_id.clone()),
    };
    let (current, hydrated) = {
        let state = state.lock().await;
        (
            state.applied_revision.unwrap_or(0),
            state.communication_hydrated,
        )
    };
    if snapshot.configuration_revision == current {
        client.set_network_enabled(observed_control.network_enabled);
    }
    let applied = if snapshot.configuration_revision == current && !hydrated {
        snapshot.validate_exact_scopes()?;
        Some(AppliedControl {
            configuration_revision: snapshot.configuration_revision,
            network_enabled: snapshot.collectors.network.enabled,
            communication_wechat_enabled: snapshot.collectors.communication_wechat.enabled(),
            communication_messages_enabled: snapshot.collectors.communication_messages.enabled,
            photos_library_enabled: snapshot.collectors.photos_library.enabled,
            screen_capture: snapshot.collectors.screen_capture.clone(),
            screenshot_request_id: snapshot
                .screenshot_request
                .as_ref()
                .map(|request| request.request_id.clone()),
        })
    } else {
        apply_snapshot(current, &snapshot)?
    };
    if applied
        .as_ref()
        .is_some_and(|applied| !applied.communication_wechat_enabled)
    {
        if !apply_communication_authorization(
            authorization,
            &credentials.credential,
            applied.clone().unwrap(),
            owner_epoch,
        )
        .await?
        {
            return Err(ControlError::Transient);
        }
        publication.publish(owner_epoch, applied.clone()).await;
    }
    let control_changed = applied.is_some();
    if let Some(applied) = applied {
        persist_network_collector_state(database, &applied).await?;
        client.set_network_enabled(applied.network_enabled);
        database
            .save_applied_collector_control(&applied.persisted(&credentials.credential))
            .await
            .map_err(|_| ControlError::Transient)?;
        {
            let mut state = state.lock().await;
            state.applied_revision = Some(applied.configuration_revision);
            state.communication_hydrated = true;
        }
        if applied.communication_wechat_enabled {
            if !apply_communication_authorization(
                authorization,
                &credentials.credential,
                applied.clone(),
                owner_epoch,
            )
            .await?
            {
                return Err(ControlError::Transient);
            }
            publication.publish(owner_epoch, Some(applied)).await;
        }
    }
    if let Some(request) = snapshot.local_media_cleanup {
        handle_local_media_cleanup(database, state, request).await?;
    }
    if control_changed {
        publication
            .publish(owner_epoch, Some(observed_control))
            .await;
    }
    Ok(())
}

async fn persist_network_collector_state(
    database: &DbActorHandle,
    applied: &AppliedControl,
) -> Result<(), ControlError> {
    let now_ms = i64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
        .map_err(|_| ControlError::Transient)?;
    let prior = database
        .load_collector_states()
        .await
        .map_err(|_| ControlError::Transient)?
        .into_iter()
        .find(|state| state.collector_key == "network");
    database
        .upsert_collector_state(&CollectorState {
            collector_key: "network".to_owned(),
            collector_version: option_env!("PCA_APP_VERSION")
                .unwrap_or(env!("CARGO_PKG_VERSION"))
                .to_owned(),
            status: if applied.network_enabled {
                CollectorStatus::Running
            } else {
                CollectorStatus::Disabled
            },
            desired_config_revision: applied.configuration_revision,
            applied_config_revision: applied.configuration_revision,
            last_event_at_ms: prior.as_ref().and_then(|state| state.last_event_at_ms),
            last_health_at_ms: Some(now_ms),
            last_error_code: None,
            created_at_ms: prior.map_or(now_ms, |state| state.created_at_ms),
            updated_at_ms: now_ms,
        })
        .await
        .map_err(|_| ControlError::Transient)
}

pub(crate) async fn persist_aux_collector_state(
    database: &DbActorHandle,
    collector_key: &str,
    enabled: bool,
    configuration_revision: u64,
    event_observed: bool,
    error_code: Option<&str>,
) -> Result<(), ControlError> {
    let now_ms = i64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
        .map_err(|_| ControlError::Transient)?;
    let prior = database
        .load_collector_states()
        .await
        .map_err(|_| ControlError::Transient)?
        .into_iter()
        .find(|state| state.collector_key == collector_key);
    let collector_version = option_env!("PCA_APP_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"));
    if !enabled
        && prior.as_ref().is_some_and(|state| {
            disabled_collector_state_is_current(state, collector_version, configuration_revision)
        })
    {
        return Ok(());
    }
    let incoming_media_error = is_media_upload_error_code(error_code);
    let preserve_prior = enabled
        && prior.as_ref().is_some_and(|state| {
            let prior_media_error = is_media_upload_error_code(state.last_error_code.as_deref());
            let prior_terminal_error =
                is_terminal_media_failure_code(state.last_error_code.as_deref());
            (error_code.is_none() && (prior_media_error || prior_terminal_error))
                || (incoming_media_error
                    && !prior_media_error
                    && !prior_terminal_error
                    && matches!(
                        state.status,
                        CollectorStatus::PermissionRequired
                            | CollectorStatus::Degraded
                            | CollectorStatus::Unsupported
                            | CollectorStatus::Error
                    ))
        });
    let status = if !enabled {
        CollectorStatus::Disabled
    } else if preserve_prior {
        prior
            .as_ref()
            .expect("preserved collector state exists")
            .status
    } else if error_code.is_some() {
        CollectorStatus::Degraded
    } else {
        CollectorStatus::Running
    };
    let last_error_code = if preserve_prior {
        prior
            .as_ref()
            .expect("preserved collector state exists")
            .last_error_code
            .clone()
    } else {
        error_code.map(str::to_owned)
    };
    database
        .upsert_collector_state_preserving_media_failure(&CollectorState {
            collector_key: collector_key.to_owned(),
            collector_version: collector_version.to_owned(),
            status,
            desired_config_revision: configuration_revision,
            applied_config_revision: configuration_revision,
            last_event_at_ms: if event_observed {
                Some(now_ms)
            } else {
                prior.as_ref().and_then(|state| state.last_event_at_ms)
            },
            last_health_at_ms: if enabled && error_code.is_none() && !preserve_prior {
                Some(now_ms)
            } else {
                prior.as_ref().and_then(|state| state.last_health_at_ms)
            },
            last_error_code,
            created_at_ms: prior.as_ref().map_or(now_ms, |state| state.created_at_ms),
            updated_at_ms: now_ms,
        })
        .await
        .map_err(|_| ControlError::Transient)
}

pub(crate) fn disabled_collector_state_is_current(
    state: &CollectorState,
    collector_version: &str,
    configuration_revision: u64,
) -> bool {
    state.status == CollectorStatus::Disabled
        && state.collector_version == collector_version
        && state.desired_config_revision == configuration_revision
        && state.applied_config_revision == configuration_revision
        && state.last_error_code.is_none()
}

async fn persist_media_upload_outcome(
    database: &DbActorHandle,
    outcome: &MediaUploadOutcome,
    error_code: &str,
) -> Result<(), ControlError> {
    for collector_key in &outcome.failed_collectors {
        persist_media_upload_health(database, collector_key, error_code, true).await?;
    }
    for collector_key in outcome
        .successful_collectors
        .difference(&outcome.failed_collectors)
    {
        persist_media_upload_health(database, collector_key, error_code, false).await?;
    }
    Ok(())
}

async fn persist_media_upload_health(
    database: &DbActorHandle,
    collector_key: &str,
    media_error_code: &str,
    failed: bool,
) -> Result<(), ControlError> {
    let now_ms = i64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
        .map_err(|_| ControlError::Transient)?;
    let Some(mut state) = database
        .load_collector_states()
        .await
        .map_err(|_| ControlError::Transient)?
        .into_iter()
        .find(|state| state.collector_key == collector_key)
    else {
        return Ok(());
    };
    if state.status == CollectorStatus::Disabled {
        return Ok(());
    }
    let primary_failure = !is_media_upload_error_code(state.last_error_code.as_deref())
        && (state.last_error_code.is_some()
            || matches!(
                state.status,
                CollectorStatus::PermissionRequired
                    | CollectorStatus::Unsupported
                    | CollectorStatus::Error
            ));
    if failed {
        if primary_failure {
            return Ok(());
        }
        state.status = CollectorStatus::Degraded;
        state.last_error_code = Some(media_error_code.to_owned());
    } else {
        if state.status != CollectorStatus::Degraded
            || !is_media_upload_error_code(state.last_error_code.as_deref())
        {
            return Ok(());
        }
        state.status = CollectorStatus::Running;
        state.last_error_code = None;
        state.last_health_at_ms = Some(now_ms);
    }
    state.updated_at_ms = now_ms;
    database
        .upsert_collector_state(&state)
        .await
        .map_err(|_| ControlError::Transient)
}

async fn recover_media_upload_health(
    database: &DbActorHandle,
    collector_key: &str,
    media_error_codes: &[&str],
) -> Result<(), ControlError> {
    let now_ms = i64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
        .map_err(|_| ControlError::Transient)?;
    let Some(mut state) = database
        .load_collector_states()
        .await
        .map_err(|_| ControlError::Transient)?
        .into_iter()
        .find(|state| state.collector_key == collector_key)
    else {
        return Ok(());
    };
    if state.status != CollectorStatus::Degraded
        || !state
            .last_error_code
            .as_deref()
            .is_some_and(|error_code| media_error_codes.contains(&error_code))
    {
        return Ok(());
    }
    state.status = CollectorStatus::Running;
    state.last_error_code = None;
    state.last_health_at_ms = Some(now_ms);
    state.updated_at_ms = now_ms;
    database
        .upsert_collector_state(&state)
        .await
        .map_err(|_| ControlError::Transient)
}

async fn handle_local_media_cleanup(
    database: &DbActorHandle,
    state: &Arc<Mutex<ControlState>>,
    request: LocalMediaCleanupRequest,
) -> Result<(), ControlError> {
    let before = database
        .communication_media_storage_stats()
        .await
        .map_err(|_| ControlError::Transient)?;
    let cleanup = database
        .cleanup_completed_communication_attachments(i64::MAX)
        .await;
    let after = database
        .communication_media_storage_stats()
        .await
        .map_err(|_| ControlError::Transient)?;
    let (status, deleted_file_count, error_code) = match cleanup {
        Ok(deleted) => ("succeeded", deleted, None),
        Err(_) => (
            "failed",
            before
                .completed_file_count
                .saturating_sub(after.completed_file_count),
            Some("LOCAL_MEDIA_CLEANUP_FAILED"),
        ),
    };
    state.lock().await.pending_media_cleanup_result = Some(LocalMediaCleanupResult {
        request_id: request.request_id,
        status,
        deleted_file_count,
        freed_bytes: before.completed_bytes.saturating_sub(after.completed_bytes),
        error_code,
    });
    Ok(())
}

fn next_media_wait(completed_media: usize) -> Duration {
    if completed_media == usize::from(MEDIA_BATCH_SIZE) {
        Duration::ZERO
    } else {
        CONTROL_INTERVAL
    }
}

async fn run_data_plane_workers(
    database: Arc<DbActorHandle>,
    credentials: watch::Receiver<DeviceCredential>,
    client: Arc<dyn ControlClient>,
    shutdown: watch::Receiver<bool>,
    screenshot_media_enabled: bool,
) -> Result<(), CloudControlRuntimeError> {
    let event_sync = run_restartable_data_plane_worker("event sync", shutdown.clone(), {
        let database = Arc::clone(&database);
        let credentials = credentials.clone();
        let client = Arc::clone(&client);
        let shutdown = shutdown.clone();
        move || {
            run_event_sync_loop(
                Arc::clone(&database),
                credentials.clone(),
                Arc::clone(&client),
                shutdown.clone(),
            )
        }
    });
    let communication_media =
        run_restartable_data_plane_worker("communication media", shutdown.clone(), {
            let database = Arc::clone(&database);
            let credentials = credentials.clone();
            let client = Arc::clone(&client);
            let shutdown = shutdown.clone();
            move || {
                run_communication_media_loop(
                    Arc::clone(&database),
                    credentials.clone(),
                    Arc::clone(&client),
                    shutdown.clone(),
                )
            }
        });
    let photo_media = run_restartable_data_plane_worker("photo media", shutdown.clone(), {
        let database = Arc::clone(&database);
        let credentials = credentials.clone();
        let client = Arc::clone(&client);
        let shutdown = shutdown.clone();
        move || {
            run_photo_media_loop(
                Arc::clone(&database),
                credentials.clone(),
                Arc::clone(&client),
                shutdown.clone(),
            )
        }
    });
    if screenshot_media_enabled {
        let screenshot_media =
            run_restartable_data_plane_worker("screenshot media", shutdown.clone(), {
                let database = Arc::clone(&database);
                let credentials = credentials.clone();
                let client = Arc::clone(&client);
                let shutdown = shutdown.clone();
                move || {
                    run_screenshot_media_loop(
                        Arc::clone(&database),
                        credentials.clone(),
                        Arc::clone(&client),
                        shutdown.clone(),
                    )
                }
            });
        tokio::try_join!(
            event_sync,
            communication_media,
            photo_media,
            screenshot_media
        )?;
    } else {
        tokio::try_join!(event_sync, communication_media, photo_media)?;
    }
    Ok(())
}

async fn run_restartable_data_plane_worker<F, Fut>(
    name: &'static str,
    mut shutdown: watch::Receiver<bool>,
    mut start: F,
) -> Result<(), CloudControlRuntimeError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(), CloudControlRuntimeError>> + Send + 'static,
{
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        let mut worker = tokio::spawn(start());
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                worker.abort();
                let _ = worker.await;
                if changed.is_err() || *shutdown.borrow_and_update() {
                    return Ok(());
                }
            }
            result = &mut worker => {
                if *shutdown.borrow() {
                    return Ok(());
                }
                let failure = match result {
                    Ok(Ok(())) => "stopped unexpectedly",
                    Ok(Err(_)) => "returned an error",
                    Err(error) if error.is_panic() => "panicked",
                    Err(_) => "was cancelled",
                };
                eprintln!("pca-agentd: {name} data-plane worker {failure}; restarting");
                if wait_or_shutdown(CONTROL_INTERVAL, &mut shutdown).await {
                    return Ok(());
                }
            }
        }
    }
}

async fn run_event_sync_loop(
    database: Arc<DbActorHandle>,
    credentials: watch::Receiver<DeviceCredential>,
    client: Arc<dyn ControlClient>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), CloudControlRuntimeError> {
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        let credential = credentials.borrow().clone();
        let _ = sync_pending_system_events(&database, &credential, client.as_ref()).await;
        let _ = sync_pending_communication_events(&database, &credential, client.as_ref()).await;
        if wait_or_shutdown(CONTROL_INTERVAL, &mut shutdown).await {
            return Ok(());
        }
    }
}

async fn run_communication_media_loop(
    database: Arc<DbActorHandle>,
    credentials: watch::Receiver<DeviceCredential>,
    client: Arc<dyn ControlClient>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), CloudControlRuntimeError> {
    let mut wait = Duration::ZERO;
    loop {
        if wait != Duration::ZERO && wait_or_shutdown(wait, &mut shutdown).await {
            return Ok(());
        }
        if *shutdown.borrow() {
            return Ok(());
        }
        let credential = credentials.borrow().clone();
        let (attachments, error_code) = match time::timeout(
            MEDIA_CYCLE_TIMEOUT,
            sync_pending_communication_attachments(&database, &credential, client.as_ref()),
        )
        .await
        {
            Ok(Ok(outcome)) => (outcome, COMMUNICATION_MEDIA_UPLOAD_FAILED),
            Ok(Err(_)) => (
                MediaUploadOutcome::failed(["communication.wechat", "communication.messages"]),
                COMMUNICATION_MEDIA_UPLOAD_FAILED,
            ),
            Err(_) => (
                MediaUploadOutcome::failed(["communication.wechat", "communication.messages"]),
                MEDIA_CYCLE_TIMEOUT_ERROR,
            ),
        };
        let completed = attachments.completed;
        wait = if persist_media_upload_outcome(&database, &attachments, error_code)
            .await
            .is_ok()
        {
            next_media_wait(completed)
        } else {
            CONTROL_INTERVAL
        };
    }
}

async fn run_photo_media_loop(
    database: Arc<DbActorHandle>,
    credentials: watch::Receiver<DeviceCredential>,
    client: Arc<dyn ControlClient>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), CloudControlRuntimeError> {
    let mut wait = Duration::ZERO;
    loop {
        if wait != Duration::ZERO && wait_or_shutdown(wait, &mut shutdown).await {
            return Ok(());
        }
        if *shutdown.borrow() {
            return Ok(());
        }
        let credential = credentials.borrow().clone();
        if database
            .dead_letter_mismatched_outbox_events(credential.workspace_id(), credential.device_id())
            .await
            .is_err()
        {
            wait = CONTROL_INTERVAL;
            continue;
        }
        let (photos, error_code) = match time::timeout(
            MEDIA_CYCLE_TIMEOUT,
            upload_pending_photos(&database, client.as_ref(), &credential),
        )
        .await
        {
            Ok(Ok(outcome)) => (outcome, PHOTOS_UPLOAD_FAILED),
            Ok(Err(_)) => (
                MediaUploadOutcome::failed(["photos.library"]),
                PHOTOS_UPLOAD_FAILED,
            ),
            Err(_) => (
                MediaUploadOutcome::failed(["photos.library"]),
                MEDIA_CYCLE_TIMEOUT_ERROR,
            ),
        };
        let completed = photos.completed;
        wait = if persist_media_upload_outcome(&database, &photos, error_code)
            .await
            .is_ok()
        {
            next_media_wait(completed)
        } else {
            CONTROL_INTERVAL
        };
    }
}

async fn run_screenshot_media_loop(
    database: Arc<DbActorHandle>,
    credentials: watch::Receiver<DeviceCredential>,
    client: Arc<dyn ControlClient>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), CloudControlRuntimeError> {
    let mut wait = Duration::ZERO;
    loop {
        if wait != Duration::ZERO && wait_or_shutdown(wait, &mut shutdown).await {
            return Ok(());
        }
        if *shutdown.borrow() {
            return Ok(());
        }
        let Ok(spool_root) = screenshot_spool_root() else {
            let _ = persist_media_upload_outcome(
                &database,
                &MediaUploadOutcome::failed(["screen.capture"]),
                SCREEN_UPLOAD_FAILED,
            )
            .await;
            wait = CONTROL_INTERVAL;
            continue;
        };
        let credential = credentials.borrow().clone();
        let (screenshots, error_code) = match time::timeout(
            SCREEN_UPLOAD_BATCH_TIMEOUT,
            upload_pending_screenshots(&database, client.as_ref(), &credential, &spool_root),
        )
        .await
        {
            Err(_) => (
                MediaUploadOutcome::failed(["screen.capture"]),
                SCREEN_UPLOAD_TIMEOUT,
            ),
            Ok(Err(_)) => (
                MediaUploadOutcome::failed(["screen.capture"]),
                SCREEN_UPLOAD_FAILED,
            ),
            Ok(Ok(outcome)) => (outcome, SCREEN_UPLOAD_FAILED),
        };
        let completed = screenshots.completed;
        let persisted = persist_media_upload_outcome(&database, &screenshots, error_code)
            .await
            .is_ok();
        let recovered = if screenshots.successful_collectors.is_empty() {
            true
        } else {
            recover_media_upload_health(
                &database,
                "screen.capture",
                &[SCREEN_UPLOAD_FAILED, SCREEN_UPLOAD_TIMEOUT],
            )
            .await
            .is_ok()
        };
        wait = if persisted && recovered {
            next_media_wait(completed)
        } else {
            CONTROL_INTERVAL
        };
    }
}

async fn run_collector_health_loop(
    database: Arc<DbActorHandle>,
    credentials: watch::Receiver<DeviceCredential>,
    mut controls: watch::Receiver<Option<AppliedControl>>,
    client: Arc<dyn ControlClient>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), CloudControlRuntimeError> {
    let mut wait = Duration::from_secs(10);
    loop {
        if wait != Duration::ZERO {
            tokio::select! {
                () = time::sleep(wait) => {}
                changed = controls.changed() => {
                    if changed.is_err() { return Ok(()); }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow_and_update() { return Ok(()); }
                }
            }
        }
        let mut network_observation_pending = false;
        if let Some(network_available) = client.network_observation_available() {
            for _ in 0..3 {
                let applied = controls.borrow().clone();
                if let Some(applied) = applied.as_ref() {
                    let enabled = applied.network_enabled;
                    network_observation_pending = enabled && !network_available;
                    let error_code = if enabled && !network_available {
                        Some("NETWORK_OBSERVATION_UNAVAILABLE")
                    } else {
                        None
                    };
                    let _ = persist_aux_collector_state(
                        &database,
                        "network",
                        enabled,
                        applied.configuration_revision,
                        false,
                        error_code,
                    )
                    .await;
                }
                if controls.borrow().as_ref() == applied.as_ref() {
                    break;
                }
            }
        }
        let Ok(states) = database.load_collector_states().await else {
            wait = CONTROL_INTERVAL;
            continue;
        };
        let credential = credentials.borrow().clone();
        wait = if client
            .report_collector_health(&credential, &states)
            .await
            .is_ok()
        {
            if network_observation_pending {
                CONTROL_INTERVAL
            } else {
                COLLECTOR_HEALTH_INTERVAL
            }
        } else {
            CONTROL_INTERVAL
        };
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "screenshot scheduling and health reporting share one serial Bridge owner"
)]
async fn run_screenshot_loop(
    database: Arc<DbActorHandle>,
    mut controls: watch::Receiver<Option<AppliedControl>>,
    screenshot_cloud_context: watch::Receiver<Option<ScreenshotCloudContext>>,
    bridge: ScreenCaptureCommandHandle,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), CloudControlRuntimeError> {
    let spool_root =
        screenshot_spool_root().map_err(|_| CloudControlRuntimeError::WorkerStopped)?;
    restore_screenshot_request_history(&database, &spool_root)
        .await
        .map_err(|_| CloudControlRuntimeError::WorkerStopped)?;
    let mut timer = time::interval(Duration::from_secs(5));
    timer.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut last_scheduled = Instant::now();
    let mut last_activity_capture = Instant::now();
    let mut last_activity_token: Option<String> = None;
    let mut handled_requests = HashSet::new();
    let mut last_health_persisted: Option<Instant> = None;
    let mut health_error: Option<&'static str> = None;
    let mut persisted_health_error: Option<&'static str> = None;
    let mut event_observed = false;
    loop {
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow_and_update() { return Ok(()); }
            }
            changed = controls.changed() => {
                if changed.is_err() { return Ok(()); }
            }
            _ = timer.tick() => {}
        }
        if *shutdown.borrow() {
            return Ok(());
        }
        let Some(control) = controls.borrow().clone() else {
            continue;
        };
        if !control.screen_capture.enabled {
            if last_health_persisted.is_none_or(|last| last.elapsed() >= COLLECTOR_HEALTH_INTERVAL)
                || persisted_health_error.is_some()
            {
                persist_aux_collector_state(
                    &database,
                    "screen.capture",
                    false,
                    control.configuration_revision,
                    false,
                    None,
                )
                .await
                .map_err(|_| CloudControlRuntimeError::WorkerStopped)?;
                last_health_persisted = Some(Instant::now());
                health_error = None;
                persisted_health_error = None;
            }
            continue;
        }

        if let Some(request_id) = control.screenshot_request_id.as_deref() {
            if !handled_requests.contains(request_id) {
                let was_handled = database
                    .screenshot_request_was_handled(request_id)
                    .await
                    .map_err(|_| CloudControlRuntimeError::WorkerStopped)?;
                if was_handled {
                    handled_requests.insert(request_id.to_owned());
                } else {
                    match capture_screenshot(
                        &bridge,
                        &control.screen_capture.excluded_bundle_ids,
                        ScreenshotTrigger::Manual,
                        Some(request_id.to_owned()),
                    )
                    .await
                    {
                        Ok(CaptureAttempt::Queued) => {
                            handled_requests.insert(request_id.to_owned());
                            database
                                .remember_screenshot_request(request_id)
                                .await
                                .map_err(|_| CloudControlRuntimeError::WorkerStopped)?;
                            health_error = None;
                            event_observed = true;
                        }
                        Ok(CaptureAttempt::Terminal(error_code)) => {
                            health_error = Some(error_code);
                            let cloud_context = { screenshot_cloud_context.borrow().clone() };
                            if let Some(context) = cloud_context {
                                if context
                                    .client
                                    .fail_screenshot_request(
                                        &context.credential,
                                        request_id,
                                        error_code,
                                    )
                                    .await
                                    .is_ok()
                                {
                                    handled_requests.insert(request_id.to_owned());
                                    database
                                        .remember_screenshot_request(request_id)
                                        .await
                                        .map_err(|_| CloudControlRuntimeError::WorkerStopped)?;
                                }
                            }
                        }
                        Ok(CaptureAttempt::Deferred) => {}
                        Ok(CaptureAttempt::Retry) | Err(_) => {
                            health_error = Some("SCREEN_CAPTURE_FAILED");
                        }
                    }
                }
            }
        }

        if control.screen_capture.scheduled_enabled
            && last_scheduled.elapsed()
                >= Duration::from_secs(control.screen_capture.interval_seconds)
        {
            let result = capture_screenshot(
                &bridge,
                &control.screen_capture.excluded_bundle_ids,
                ScreenshotTrigger::Scheduled,
                None,
            )
            .await;
            match result {
                Ok(CaptureAttempt::Queued) => {
                    last_scheduled = Instant::now();
                    health_error = None;
                    event_observed = true;
                }
                Ok(
                    CaptureAttempt::Terminal("SCREEN_CAPTURE_APP_EXCLUDED")
                    | CaptureAttempt::Deferred,
                ) => {}
                Ok(CaptureAttempt::Terminal(error_code)) => health_error = Some(error_code),
                Ok(CaptureAttempt::Retry) | Err(_) => health_error = Some("SCREEN_CAPTURE_FAILED"),
            }
        }

        if control.screen_capture.activity_enabled {
            if let Ok(context) = bridge.context().await {
                if !context.locked {
                    let changed = context.activity_token != last_activity_token;
                    last_activity_token = context.activity_token;
                    if changed
                        && last_activity_capture.elapsed()
                            >= Duration::from_secs(
                                control.screen_capture.activity_min_interval_seconds,
                            )
                    {
                        let result = capture_screenshot(
                            &bridge,
                            &control.screen_capture.excluded_bundle_ids,
                            ScreenshotTrigger::Activity,
                            None,
                        )
                        .await;
                        match result {
                            Ok(CaptureAttempt::Queued) => {
                                last_activity_capture = Instant::now();
                                health_error = None;
                                event_observed = true;
                            }
                            Ok(CaptureAttempt::Terminal(error_code)) => {
                                if error_code != "SCREEN_CAPTURE_APP_EXCLUDED" {
                                    health_error = Some(error_code);
                                }
                            }
                            Ok(CaptureAttempt::Deferred) => {}
                            Ok(CaptureAttempt::Retry) | Err(_) => {
                                health_error = Some("SCREEN_CAPTURE_FAILED");
                            }
                        }
                    }
                }
            }
        }
        let effective_health_error = health_error;
        if last_health_persisted.is_none_or(|last| last.elapsed() >= COLLECTOR_HEALTH_INTERVAL)
            || event_observed
            || effective_health_error != persisted_health_error
        {
            persist_aux_collector_state(
                &database,
                "screen.capture",
                control.screen_capture.enabled,
                control.configuration_revision,
                event_observed,
                effective_health_error,
            )
            .await
            .map_err(|_| CloudControlRuntimeError::WorkerStopped)?;
            last_health_persisted = Some(Instant::now());
            persisted_health_error = effective_health_error;
            event_observed = false;
        }
    }
}

enum CaptureAttempt {
    Queued,
    Deferred,
    Retry,
    Terminal(&'static str),
}

async fn capture_screenshot(
    bridge: &ScreenCaptureCommandHandle,
    excluded_bundle_ids: &[String],
    trigger: ScreenshotTrigger,
    request_id: Option<String>,
) -> Result<CaptureAttempt, ControlError> {
    let result = bridge
        .capture(excluded_bundle_ids.to_vec())
        .await
        .map_err(|_| ControlError::Transient)?;
    match result.status {
        ScreenCaptureStatus::SkippedLocked => {
            return Ok(CaptureAttempt::Deferred);
        }
        ScreenCaptureStatus::Unavailable => {
            return Ok(CaptureAttempt::Retry);
        }
        ScreenCaptureStatus::SkippedExcluded => {
            return Ok(CaptureAttempt::Terminal("SCREEN_CAPTURE_APP_EXCLUDED"));
        }
        ScreenCaptureStatus::PermissionRequired => {
            return Ok(CaptureAttempt::Terminal(
                "SCREEN_CAPTURE_PERMISSION_REQUIRED",
            ));
        }
        ScreenCaptureStatus::Captured => {}
    }
    let path = result.path.ok_or(ControlError::Contract)?;
    let spool_root = screenshot_spool_root()?;
    if path.parent() != Some(spool_root.as_path())
        || path.extension().is_none_or(|value| value != "jpg")
    {
        return Err(ControlError::Contract);
    }
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|_| ControlError::Transient)?;
    if bytes.is_empty() || bytes.len() > 100 * 1024 * 1024 {
        return Err(ControlError::Contract);
    }
    let image_file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or(ControlError::Contract)?
        .to_owned();
    let screenshot = PendingScreenshot {
        screenshot_id: Uuid::new_v4().to_string(),
        request_id,
        trigger,
        captured_at: OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .map_err(|_| ControlError::Contract)?,
        app_bundle_id: result.app_bundle_id,
        pixel_width: result.pixel_width.ok_or(ControlError::Contract)?,
        pixel_height: result.pixel_height.ok_or(ControlError::Contract)?,
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        size_bytes: u64::try_from(bytes.len()).map_err(|_| ControlError::Contract)?,
        mime_type: "image/jpeg".to_owned(),
        image_file_name,
    };
    if let Err(error) = persist_screenshot_manifest(&spool_root, &screenshot).await {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(error);
    }
    Ok(CaptureAttempt::Queued)
}

async fn persist_screenshot_manifest(
    spool_root: &Path,
    screenshot: &PendingScreenshot,
) -> Result<(), ControlError> {
    let manifest = serde_json::to_vec(screenshot).map_err(|_| ControlError::Contract)?;
    let final_path = spool_root.join(format!("{}.json", screenshot.screenshot_id));
    let temporary_path = spool_root.join(format!(".{}.tmp", screenshot.screenshot_id));
    tokio::fs::write(&temporary_path, manifest)
        .await
        .map_err(|_| ControlError::Transient)?;
    tokio::fs::set_permissions(&temporary_path, std::fs::Permissions::from_mode(0o600))
        .await
        .map_err(|_| ControlError::Transient)?;
    tokio::fs::rename(&temporary_path, &final_path)
        .await
        .map_err(|_| ControlError::Transient)
}

async fn upload_pending_screenshots(
    database: &DbActorHandle,
    client: &dyn ControlClient,
    credentials: &DeviceCredential,
    spool_root: &Path,
) -> Result<MediaUploadOutcome, ControlError> {
    let mut outcome = MediaUploadOutcome::default();
    for path in pending_manifest_paths(spool_root)
        .await?
        .into_iter()
        .take(4)
    {
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|_| ControlError::Transient)?;
        let screenshot: PendingScreenshot = if let Ok(screenshot) = serde_json::from_slice(&bytes) {
            screenshot
        } else {
            quarantine_invalid_screenshot_manifest(database, &path).await?;
            continue;
        };
        if screenshot.mime_type != "image/jpeg"
            || Uuid::parse_str(&screenshot.screenshot_id).is_err()
            || screenshot
                .request_id
                .as_ref()
                .is_some_and(|value| Uuid::parse_str(value).is_err())
        {
            quarantine_invalid_screenshot_manifest(database, &path).await?;
            continue;
        }
        let image_path = spool_root.join(&screenshot.image_file_name);
        if image_path.parent() != Some(spool_root) {
            quarantine_invalid_screenshot_manifest(database, &path).await?;
            continue;
        }
        remember_screenshot_request(database, &screenshot).await?;
        if client
            .sync_screenshot(credentials, &screenshot)
            .await
            .is_err()
        {
            rotate_manifest(&path, &bytes).await?;
            outcome.record_failure("screen.capture");
            continue;
        }
        remove_uploaded_media_file(&image_path).await?;
        tokio::fs::remove_file(&path)
            .await
            .map_err(|_| ControlError::Transient)?;
        outcome.record_success("screen.capture");
    }
    Ok(outcome)
}

async fn quarantine_invalid_screenshot_manifest(
    database: &DbActorHandle,
    path: &Path,
) -> Result<(), ControlError> {
    let subject_id = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or(ControlError::Contract)?;
    database
        .record_terminal_media_diagnostic(subject_id, SCREEN_LOCAL_MANIFEST_INVALID)
        .await
        .map_err(|_| ControlError::Transient)?;
    persist_media_upload_health(
        database,
        "screen.capture",
        SCREEN_LOCAL_MANIFEST_INVALID,
        true,
    )
    .await?;
    quarantine_manifest(path).await
}

async fn quarantine_manifest(path: &Path) -> Result<(), ControlError> {
    let quarantined = path.with_extension(format!("invalid-{}", Uuid::new_v4()));
    tokio::fs::rename(path, quarantined)
        .await
        .map_err(|_| ControlError::Transient)
}

async fn remember_screenshot_request(
    database: &DbActorHandle,
    screenshot: &PendingScreenshot,
) -> Result<(), ControlError> {
    if let Some(request_id) = &screenshot.request_id {
        database
            .remember_screenshot_request(request_id)
            .await
            .map_err(|_| ControlError::Transient)?;
    }
    Ok(())
}

async fn restore_screenshot_request_history(
    database: &DbActorHandle,
    spool_root: &Path,
) -> Result<(), ControlError> {
    for path in pending_manifest_paths(spool_root).await? {
        let Ok(bytes) = tokio::fs::read(path).await else {
            continue;
        };
        let Ok(screenshot) = serde_json::from_slice::<PendingScreenshot>(&bytes) else {
            continue;
        };
        if screenshot
            .request_id
            .as_ref()
            .is_some_and(|request_id| Uuid::parse_str(request_id).is_ok())
        {
            remember_screenshot_request(database, &screenshot).await?;
        }
    }
    Ok(())
}

fn screenshot_prepare_payload(screenshot: &PendingScreenshot) -> serde_json::Value {
    serde_json::json!({
        "screenshot_id": screenshot.screenshot_id,
        "request_id": screenshot.request_id,
        "trigger": screenshot.trigger,
        "captured_at": screenshot.captured_at,
        "app_bundle_id": screenshot.app_bundle_id,
        "pixel_width": screenshot.pixel_width,
        "pixel_height": screenshot.pixel_height,
        "sha256": screenshot.sha256,
        "size_bytes": screenshot.size_bytes,
        "mime_type": screenshot.mime_type,
    })
}

fn screenshot_spool_root() -> Result<PathBuf, ControlError> {
    let home = std::env::var_os("HOME").ok_or(ControlError::Contract)?;
    let root = PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("PersonalComputerAgent")
        .join("ScreenshotSpool");
    if !root.is_absolute() {
        return Err(ControlError::Contract);
    }
    Ok(root)
}

pub(crate) fn photo_spool_root() -> Result<PathBuf, ControlError> {
    let home = std::env::var_os("HOME").ok_or(ControlError::Contract)?;
    let root = PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("PersonalComputerAgent")
        .join("PhotoSpool");
    if !root.is_absolute() {
        return Err(ControlError::Contract);
    }
    Ok(root)
}

fn photo_handled_root(root: &Path) -> PathBuf {
    root.join("Handled")
}

fn photo_marker_path(root: &Path, photo_id: &str, marker: PhotoMarker) -> PathBuf {
    photo_handled_root(root).join(format!("{photo_id}.{}", marker.extension()))
}

pub(crate) async fn photo_asset_is_handled(
    database: &DbActorHandle,
    photo_id: &str,
) -> Result<bool, ControlError> {
    if Uuid::parse_str(photo_id).is_err() {
        return Err(ControlError::Contract);
    }
    if database
        .photo_upload_exists(photo_id)
        .await
        .map_err(|_| ControlError::Transient)?
    {
        return Ok(true);
    }
    let root = photo_spool_root()?;
    for path in [
        root.join(format!("{photo_id}.json")),
        root.join(format!("{photo_id}.oversized")),
        photo_marker_path(&root, photo_id, PhotoMarker::Completed),
        photo_marker_path(&root, photo_id, PhotoMarker::Oversized),
    ] {
        if tokio::fs::try_exists(path)
            .await
            .map_err(|_| ControlError::Transient)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) async fn persist_photo_marker(
    photo_id: &str,
    marker: PhotoMarker,
) -> Result<(), ControlError> {
    if Uuid::parse_str(photo_id).is_err() {
        return Err(ControlError::Contract);
    }
    let root = photo_spool_root()?;
    let handled = photo_handled_root(&root);
    tokio::fs::create_dir_all(&handled)
        .await
        .map_err(|_| ControlError::Transient)?;
    tokio::fs::set_permissions(&handled, std::fs::Permissions::from_mode(0o700))
        .await
        .map_err(|_| ControlError::Transient)?;
    let final_path = photo_marker_path(&root, photo_id, marker);
    let temporary_path = handled.join(format!(".{photo_id}.{}.tmp", marker.extension()));
    tokio::fs::write(&temporary_path, [])
        .await
        .map_err(|_| ControlError::Transient)?;
    tokio::fs::set_permissions(&temporary_path, std::fs::Permissions::from_mode(0o600))
        .await
        .map_err(|_| ControlError::Transient)?;
    tokio::fs::rename(temporary_path, final_path)
        .await
        .map_err(|_| ControlError::Transient)
}

async fn upload_pending_photos(
    database: &DbActorHandle,
    client: &dyn ControlClient,
    credentials: &DeviceCredential,
) -> Result<MediaUploadOutcome, ControlError> {
    let root = photo_spool_root()?;
    let mut outcome = MediaUploadOutcome::default();
    for task in database
        .load_pending_photo_uploads(4, credentials.workspace_id(), credentials.device_id())
        .await
        .map_err(|_| ControlError::Transient)?
    {
        let already_terminal = tokio::fs::try_exists(photo_marker_path(
            &root,
            &task.photo_id,
            PhotoMarker::Completed,
        ))
        .await
        .map_err(|_| ControlError::Transient)?
            || tokio::fs::try_exists(photo_marker_path(
                &root,
                &task.photo_id,
                PhotoMarker::Oversized,
            ))
            .await
            .map_err(|_| ControlError::Transient)?;
        if already_terminal {
            database
                .complete_photo_upload(&task.photo_id)
                .await
                .map_err(|_| ControlError::Transient)?;
            outcome.record_success("photos.library");
            continue;
        }
        let photo: PendingPhoto = if let Ok(photo) = serde_json::from_str(&task.manifest_json) {
            photo
        } else {
            database
                .quarantine_invalid_photo_upload(&task.photo_id)
                .await
                .map_err(|_| ControlError::Transient)?;
            continue;
        };
        if photo.photo_id != task.photo_id
            || Uuid::parse_str(&photo.photo_id).is_err()
            || Uuid::parse_str(&photo.event_id).is_err()
            || photo.media_file_name.as_deref() != Some(photo.photo_id.as_str())
        {
            database
                .quarantine_invalid_photo_upload(&task.photo_id)
                .await
                .map_err(|_| ControlError::Transient)?;
            continue;
        }
        let media_path = root.join(
            photo
                .media_file_name
                .as_deref()
                .ok_or(ControlError::Contract)?,
        );
        if media_path.parent().is_none_or(|parent| parent != root) {
            return Err(ControlError::Contract);
        }
        if photo.size_bytes > 500 * 1024 * 1024 {
            remove_uploaded_media_file(&media_path).await?;
            persist_photo_marker(&photo.photo_id, PhotoMarker::Oversized).await?;
            database
                .complete_photo_upload(&photo.photo_id)
                .await
                .map_err(|_| ControlError::Transient)?;
            outcome.record_success("photos.library");
            continue;
        }
        if client.sync_photo(credentials, &photo).await.is_err() {
            outcome.record_failure("photos.library");
            continue;
        }
        remove_uploaded_media_file(&media_path).await?;
        persist_photo_marker(&photo.photo_id, PhotoMarker::Completed).await?;
        database
            .complete_photo_upload(&photo.photo_id)
            .await
            .map_err(|_| ControlError::Transient)?;
        outcome.record_success("photos.library");
    }
    let legacy = upload_pending_legacy_photos(client, credentials, &root).await?;
    outcome.completed = outcome.completed.saturating_add(legacy.completed);
    outcome
        .successful_collectors
        .extend(legacy.successful_collectors);
    outcome.failed_collectors.extend(legacy.failed_collectors);
    Ok(outcome)
}

async fn upload_pending_legacy_photos(
    client: &dyn ControlClient,
    credentials: &DeviceCredential,
    root: &Path,
) -> Result<MediaUploadOutcome, ControlError> {
    let mut outcome = MediaUploadOutcome::default();
    for path in pending_manifest_paths(root).await?.into_iter().take(4) {
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|_| ControlError::Transient)?;
        let photo: PendingPhoto = if let Ok(photo) = serde_json::from_slice(&bytes) {
            photo
        } else {
            quarantine_manifest(&path).await?;
            continue;
        };
        if photo.completed {
            persist_photo_marker(&photo.photo_id, PhotoMarker::Completed).await?;
            tokio::fs::remove_file(&path)
                .await
                .map_err(|_| ControlError::Transient)?;
            continue;
        }
        if Uuid::parse_str(&photo.photo_id).is_err()
            || Uuid::parse_str(&photo.event_id).is_err()
            || photo.media_file_name.as_deref() != Some(photo.photo_id.as_str())
        {
            quarantine_manifest(&path).await?;
            continue;
        }
        if photo.size_bytes > 500 * 1024 * 1024 {
            let media_path = root.join(
                photo
                    .media_file_name
                    .as_deref()
                    .ok_or(ControlError::Contract)?,
            );
            remove_uploaded_media_file(&media_path).await?;
            persist_photo_marker(&photo.photo_id, PhotoMarker::Oversized).await?;
            tokio::fs::remove_file(&path)
                .await
                .map_err(|_| ControlError::Transient)?;
            continue;
        }
        if client.sync_photo(credentials, &photo).await.is_err() {
            rotate_manifest(&path, &bytes).await?;
            outcome.record_failure("photos.library");
            continue;
        }
        let media_path = root.join(
            photo
                .media_file_name
                .as_deref()
                .ok_or(ControlError::Contract)?,
        );
        if media_path.parent() != Some(root) {
            return Err(ControlError::Contract);
        }
        remove_uploaded_media_file(&media_path).await?;
        persist_photo_marker(&photo.photo_id, PhotoMarker::Completed).await?;
        tokio::fs::remove_file(&path)
            .await
            .map_err(|_| ControlError::Transient)?;
        outcome.record_success("photos.library");
    }
    Ok(outcome)
}

async fn pending_manifest_paths(root: &Path) -> Result<Vec<PathBuf>, ControlError> {
    let mut entries = match tokio::fs::read_dir(root).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err(ControlError::Transient),
    };
    let mut paths = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|_| ControlError::Transient)?
    {
        let path = entry.path();
        if path.extension().is_some_and(|value| value == "json") {
            let modified = entry
                .metadata()
                .await
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            paths.push((modified, path));
        }
    }
    paths.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    Ok(paths.into_iter().map(|(_, path)| path).collect())
}

async fn rotate_manifest(path: &Path, bytes: &[u8]) -> Result<(), ControlError> {
    let parent = path.parent().ok_or(ControlError::Contract)?;
    let temporary = parent.join(format!(".retry-{}.tmp", Uuid::new_v4()));
    tokio::fs::write(&temporary, bytes)
        .await
        .map_err(|_| ControlError::Transient)?;
    tokio::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))
        .await
        .map_err(|_| ControlError::Transient)?;
    tokio::fs::rename(temporary, path)
        .await
        .map_err(|_| ControlError::Transient)
}

async fn remove_uploaded_media_file(path: &Path) -> Result<(), ControlError> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(ControlError::Transient),
    }
}

async fn sync_pending_system_events(
    database: &DbActorHandle,
    credentials: &DeviceCredential,
    client: &dyn ControlClient,
) -> Result<(), ControlError> {
    database
        .dead_letter_mismatched_outbox_events(credentials.workspace_id(), credentials.device_id())
        .await
        .map_err(|_| ControlError::Transient)?;
    let events = database
        .load_pending_system_events(20)
        .await
        .map_err(|_| ControlError::Transient)?;
    if events.is_empty() {
        return Ok(());
    }
    let expected: std::collections::BTreeSet<_> =
        events.iter().map(|event| event.event_id.as_str()).collect();
    let response = client
        .sync_system_events(credentials, &events)
        .await
        .map_err(isolate_data_plane_error)?;
    let acknowledged: std::collections::BTreeSet<_> = response
        .accepted
        .iter()
        .chain(response.duplicates.iter())
        .map(String::as_str)
        .collect();
    let rejected = terminal_rejected_event_ids(&response)?;
    if acknowledged.len() != response.accepted.len() + response.duplicates.len()
        || rejected.len() != response.rejected.len()
        || !acknowledged.is_disjoint(&rejected)
        || acknowledged
            .union(&rejected)
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            != expected
    {
        return Err(ControlError::Transient);
    }
    let event_ids = acknowledged
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if !event_ids.is_empty() {
        database
            .acknowledge_system_events(&event_ids)
            .await
            .map_err(|_| ControlError::Transient)?;
    }
    if !rejected.is_empty() {
        let event_ids = rejected.into_iter().map(str::to_owned).collect::<Vec<_>>();
        persist_rejected_event_health(database, &events, &event_ids, SYNC_PAYLOAD_REJECTED).await?;
        database
            .dead_letter_rejected_system_events(&event_ids)
            .await
            .map_err(|_| ControlError::Transient)?;
    }
    Ok(())
}

async fn sync_pending_communication_events(
    database: &DbActorHandle,
    credentials: &DeviceCredential,
    client: &dyn ControlClient,
) -> Result<(), ControlError> {
    database
        .dead_letter_mismatched_outbox_events(credentials.workspace_id(), credentials.device_id())
        .await
        .map_err(|_| ControlError::Transient)?;
    let events = database
        .load_pending_communication_events(200)
        .await
        .map_err(|_| ControlError::Transient)?;
    if events.is_empty() {
        return Ok(());
    }
    let expected: std::collections::BTreeSet<_> =
        events.iter().map(|event| event.event_id.as_str()).collect();
    let response = client
        .sync_communication_events(credentials, &events)
        .await
        .map_err(isolate_data_plane_error)?;
    let acknowledged: std::collections::BTreeSet<_> = response
        .accepted
        .iter()
        .chain(response.duplicates.iter())
        .map(String::as_str)
        .collect();
    let rejected = terminal_rejected_event_ids(&response)?;
    if acknowledged.len() != response.accepted.len() + response.duplicates.len()
        || rejected.len() != response.rejected.len()
        || !acknowledged.is_disjoint(&rejected)
        || acknowledged
            .union(&rejected)
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            != expected
    {
        return Err(ControlError::Transient);
    }
    let event_ids = acknowledged
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if !event_ids.is_empty() {
        database
            .acknowledge_communication_events(&event_ids)
            .await
            .map_err(|_| ControlError::Transient)?;
    }
    if !rejected.is_empty() {
        let event_ids = rejected.into_iter().map(str::to_owned).collect::<Vec<_>>();
        persist_rejected_event_health(database, &events, &event_ids, SYNC_PAYLOAD_REJECTED).await?;
        database
            .dead_letter_rejected_communication_events(&event_ids)
            .await
            .map_err(|_| ControlError::Transient)?;
    }
    Ok(())
}

fn terminal_rejected_event_ids(
    response: &SyncEventsResponse,
) -> Result<std::collections::BTreeSet<&str>, ControlError> {
    if response
        .rejected
        .iter()
        .any(|rejection| rejection.retryable)
    {
        return Err(ControlError::Transient);
    }
    if response.rejected.iter().any(|rejection| {
        rejection.error_code.is_empty()
            || rejection.error_code.len() > 128
            || rejection.error_code.chars().any(char::is_control)
    }) {
        return Err(ControlError::Contract);
    }
    Ok(response
        .rejected
        .iter()
        .map(|rejection| rejection.event_id.as_str())
        .collect())
}

async fn persist_rejected_event_health(
    database: &DbActorHandle,
    events: &[EventEnvelope],
    rejected_event_ids: &[String],
    error_code: &'static str,
) -> Result<(), ControlError> {
    let rejected = rejected_event_ids
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let affected = events
        .iter()
        .filter(|event| rejected.contains(event.event_id.as_str()))
        .map(collector_key_for_event)
        .collect::<HashSet<_>>();
    let now_ms = i64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
        .map_err(|_| ControlError::Transient)?;
    for mut state in database
        .load_collector_states()
        .await
        .map_err(|_| ControlError::Transient)?
        .into_iter()
        .filter(|state| affected.contains(state.collector_key.as_str()))
    {
        if state.status == CollectorStatus::Disabled {
            continue;
        }
        state.status = CollectorStatus::Degraded;
        state.last_error_code = Some(error_code.to_owned());
        state.updated_at_ms = now_ms;
        database
            .upsert_collector_state(&state)
            .await
            .map_err(|_| ControlError::Transient)?;
    }
    Ok(())
}

fn collector_key_for_event(event: &EventEnvelope) -> &'static str {
    match event.source.as_str() {
        "communication.wechat" => "communication.wechat",
        "communication.messages" => "communication.messages",
        "photos.library" => "photos.library",
        _ if event.event_type.starts_with("network.") => "network",
        _ => "system",
    }
}

fn isolate_data_plane_error(error: ControlError) -> ControlError {
    match error {
        ControlError::Contract => ControlError::Transient,
        other => other,
    }
}

async fn sync_pending_communication_attachments(
    database: &DbActorHandle,
    credentials: &DeviceCredential,
    client: &dyn ControlClient,
) -> Result<MediaUploadOutcome, ControlError> {
    let attachments = database
        .load_pending_communication_attachments(MEDIA_BATCH_SIZE)
        .await
        .map_err(|_| ControlError::Transient)?;
    let mut outcome = MediaUploadOutcome::default();
    for attachment in attachments {
        let Ok(collector_key) = communication_attachment_collector_key(&attachment.source) else {
            for collector_key in ["communication.wechat", "communication.messages"] {
                persist_media_upload_health(
                    database,
                    collector_key,
                    MEDIA_SOURCE_UNSUPPORTED,
                    true,
                )
                .await?;
            }
            database
                .quarantine_unsupported_communication_attachment(&attachment.attachment_id)
                .await
                .map_err(|_| ControlError::Transient)?;
            continue;
        };
        if let Err(failure) = client
            .sync_communication_attachment(credentials, &attachment)
            .await
        {
            if attachment_was_superseded(failure) {
                database
                    .complete_communication_attachment(&attachment.attachment_id)
                    .await
                    .map_err(|_| ControlError::Transient)?;
                outcome.record_success(collector_key);
                continue;
            }
            database
                .defer_communication_attachment(
                    &attachment.attachment_id,
                    failure.stage.as_str(),
                    failure.error.diagnostic_category(),
                    failure.fallback_from.map(MediaUploadFailureStage::as_str),
                )
                .await
                .map_err(|_| ControlError::Transient)?;
            outcome.record_failure(collector_key);
            continue;
        }
        database
            .complete_communication_attachment(&attachment.attachment_id)
            .await
            .map_err(|_| ControlError::Transient)?;
        outcome.record_success(collector_key);
    }
    Ok(outcome)
}

fn communication_attachment_collector_key(source: &str) -> Result<&'static str, ControlError> {
    match source {
        "communication.wechat" | "wechat" => Ok("communication.wechat"),
        "communication.messages" => Ok("communication.messages"),
        _ => Err(ControlError::Contract),
    }
}

async fn apply_communication_authorization(
    authorization: &CommunicationAuthorization,
    credentials: &DeviceCredential,
    applied: AppliedControl,
    owner_epoch: u64,
) -> Result<bool, ControlError> {
    let identity =
        CommunicationIdentity::try_new(credentials.workspace_id(), credentials.device_id())
            .map_err(|_| ControlError::Contract)?;
    let control = CommunicationControl::paired(
        identity,
        applied.configuration_revision,
        applied.communication_wechat_enabled,
    )
    .map_err(|_| ControlError::Contract)?;
    authorization
        .apply_persisted_for_owner(owner_epoch, control)
        .await
        .map_err(|_| ControlError::Contract)
}

async fn mark_manually_unpaired(
    database: &DbActorHandle,
    credentials: &LoadedDeviceCredentials,
    state: &Arc<Mutex<ControlState>>,
    pairing_state_sender: &watch::Sender<bool>,
    publication: &ControlPublication,
    authorization: &CommunicationAuthorization,
    owner_epoch: u64,
) -> Result<(), CloudControlRuntimeError> {
    if authorization.owner_epoch().await != owner_epoch {
        return Ok(());
    }
    database
        .mark_pairing_manually_unpaired_and_disable_sensitive_collectors()
        .await?;
    if !authorization.disable_for_owner(owner_epoch).await {
        return Ok(());
    }
    publication.publish(owner_epoch, None).await;
    {
        let mut state = state.lock().await;
        state.unpaired = true;
        state.applied_revision = None;
    }
    pairing_state_sender.send_replace(false);
    delete_device_credential(credentials.store.as_ref())?;
    Ok(())
}

async fn wait_or_shutdown(wait: Duration, shutdown: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        () = time::sleep(wait) => false,
        changed = shutdown.changed() => changed.is_err() || *shutdown.borrow_and_update(),
    }
}

fn retry_delay(attempt: u8) -> Duration {
    let shift = u32::from(attempt.saturating_sub(1).min(8));
    let base = Duration::from_secs(1_u64 << shift).min(MAX_BACKOFF);
    let jitter = base / 4;
    if attempt.is_multiple_of(2) {
        base.saturating_sub(jitter)
    } else {
        base.saturating_add(jitter).min(MAX_BACKOFF)
    }
}

fn random_url_safe_value() -> String {
    let mut bytes = [0_u8; 32];
    bytes[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    URL_SAFE_NO_PAD.encode(bytes)
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// HTTPS adapter for the fixed S1B endpoints. It never serializes credentials to diagnostics.
pub struct HttpControlClient {
    base_url: Url,
    network_observations: Arc<NetworkObservationState>,
}

impl HttpControlClient {
    /// Creates an adapter only for an HTTPS Cloud API origin.
    ///
    /// # Errors
    ///
    /// Returns [`ControlError::Contract`] for a non-HTTPS URL.
    pub fn new(base_url: Url) -> Result<Self, ControlError> {
        if base_url.scheme() != "https"
            || base_url.host_str() != Some("pca-cloud-api-production.up.railway.app")
            || base_url.port().is_some()
            || base_url.path() != "/"
            || base_url.query().is_some()
            || base_url.fragment().is_some()
            || !base_url.username().is_empty()
            || base_url.password().is_some()
        {
            return Err(ControlError::Contract);
        }
        Ok(Self {
            base_url,
            network_observations: Arc::new(NetworkObservationState::default()),
        })
    }

    #[must_use]
    pub fn with_network_observations(
        mut self,
        network_observations: Arc<NetworkObservationState>,
    ) -> Self {
        self.network_observations = network_observations;
        self
    }

    fn client() -> Result<Client, ControlError> {
        Client::builder()
            .https_only(true)
            .timeout(CONTROL_REQUEST_TIMEOUT)
            .pool_max_idle_per_host(0)
            .build()
            .map_err(|_| ControlError::Transient)
    }

    fn direct_client() -> Result<Client, ControlError> {
        Client::builder()
            .https_only(true)
            .timeout(CONTROL_REQUEST_TIMEOUT)
            .pool_max_idle_per_host(0)
            .no_proxy()
            .build()
            .map_err(|_| ControlError::Transient)
    }

    async fn prepare_photo_upload(
        &self,
        client: &Client,
        credentials: &DeviceCredential,
        photo: &PendingPhoto,
    ) -> Result<PreparedPhoto, ControlError> {
        let response = client
            .post(self.endpoint("v1/agent/photos/prepare")?)
            .bearer_auth(credentials.access_credential())
            .json(&photo_prepare_payload(photo))
            .send()
            .await
            .map_err(|_| ControlError::Transient)?;
        let prepared = parse_response::<PreparedPhoto>(response).await?;
        if prepared.photo_id != photo.photo_id {
            return Err(ControlError::Contract);
        }
        Ok(prepared)
    }

    fn endpoint(&self, path: &str) -> Result<Url, ControlError> {
        self.base_url.join(path).map_err(|_| ControlError::Contract)
    }
}

async fn upload_communication_attachment(
    client: &Client,
    upload_url: Url,
    headers: &BTreeMap<String, String>,
    attachment: &PendingCommunicationAttachment,
) -> Result<(), ControlError> {
    let file = attachment
        .try_clone_file()
        .map_err(|_| ControlError::Transient)?;
    let body = if attachment.mime_type.starts_with("image/") {
        let expected_size =
            usize::try_from(attachment.size_bytes).map_err(|_| ControlError::Contract)?;
        let mut bytes = Vec::with_capacity(expected_size);
        tokio::fs::File::from_std(file)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| ControlError::Transient)?;
        if bytes.len() != expected_size {
            return Err(ControlError::Contract);
        }
        reqwest::Body::from(bytes)
    } else {
        reqwest::Body::wrap_stream(ReaderStream::new(tokio::fs::File::from_std(file)))
    };
    let request = client
        .put(upload_url)
        .timeout(MEDIA_UPLOAD_TIMEOUT)
        .body(body);
    let request = apply_communication_upload_headers(request, headers, attachment.size_bytes)?;
    let response = request.send().await.map_err(|_| ControlError::Transient)?;
    if response.status().is_success() {
        Ok(())
    } else if response.status().is_server_error() {
        Err(ControlError::Transient)
    } else {
        Err(ControlError::Contract)
    }
}

async fn upload_screenshot(
    client: &Client,
    upload_url: Url,
    headers: &BTreeMap<String, String>,
    screenshot: &PendingScreenshot,
    spool_root: &Path,
) -> Result<(), ControlError> {
    let path = spool_root.join(&screenshot.image_file_name);
    if path.parent() != Some(spool_root) || path.extension().is_none_or(|value| value != "jpg") {
        return Err(ControlError::Contract);
    }
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|_| ControlError::Transient)?;
    if u64::try_from(bytes.len()).map_err(|_| ControlError::Contract)? != screenshot.size_bytes
        || format!("{:x}", Sha256::digest(&bytes)) != screenshot.sha256
    {
        return Err(ControlError::Contract);
    }
    let request = client
        .put(upload_url)
        .timeout(MEDIA_UPLOAD_TIMEOUT)
        .body(bytes);
    let request = apply_communication_upload_headers(request, headers, screenshot.size_bytes)?;
    let response = request.send().await.map_err(|_| ControlError::Transient)?;
    if response.status().is_success() {
        Ok(())
    } else if response.status().is_server_error() {
        Err(ControlError::Transient)
    } else {
        Err(ControlError::Contract)
    }
}

async fn upload_photo(
    client: &Client,
    upload_url: Url,
    headers: &BTreeMap<String, String>,
    photo: &PendingPhoto,
    spool_root: &Path,
) -> Result<(), ControlError> {
    let file_name = photo
        .media_file_name
        .as_deref()
        .ok_or(ControlError::Contract)?;
    let path = spool_root.join(file_name);
    if path.parent() != Some(spool_root) || file_name != photo.photo_id {
        return Err(ControlError::Contract);
    }
    let (actual_sha256, actual_size) = hash_spool_file(&path).await?;
    if actual_size != photo.size_bytes || actual_sha256 != photo.sha256 {
        return Err(ControlError::Contract);
    }
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|_| ControlError::Transient)?;
    let request = client
        .put(upload_url)
        .timeout(photo_upload_timeout(photo.size_bytes))
        .body(reqwest::Body::wrap_stream(ReaderStream::new(file)));
    let request = apply_communication_upload_headers(request, headers, photo.size_bytes)?;
    let response = request.send().await.map_err(|_| ControlError::Transient)?;
    if response.status().is_success() {
        Ok(())
    } else if response.status().is_server_error() {
        Err(ControlError::Transient)
    } else {
        Err(ControlError::Contract)
    }
}

fn photo_upload_timeout(size_bytes: u64) -> Duration {
    let transfer_seconds = size_bytes.div_ceil(PHOTO_UPLOAD_MIN_BYTES_PER_SECOND);
    Duration::from_secs(transfer_seconds)
        .saturating_add(PHOTO_UPLOAD_GRACE)
        .max(MEDIA_UPLOAD_TIMEOUT)
        .min(PHOTO_UPLOAD_MAX_TIMEOUT)
}

fn photo_prepare_payload(photo: &PendingPhoto) -> serde_json::Value {
    serde_json::json!({
        "photo_id": photo.photo_id,
        "event_id": photo.event_id,
        "asset_id": photo.asset_id,
        "captured_at": photo.captured_at,
        "media_type": photo.media_type,
        "original_filename": photo.original_filename,
        "mime_type": photo.mime_type,
        "pixel_width": photo.pixel_width,
        "pixel_height": photo.pixel_height,
        "duration_seconds": photo.duration_seconds,
        "album_names": photo.album_names,
        "sha256": photo.sha256,
        "size_bytes": photo.size_bytes,
    })
}

async fn hash_spool_file(path: &Path) -> Result<(String, u64), ControlError> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|_| ControlError::Transient)?;
    let mut digest = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .await
            .map_err(|_| ControlError::Transient)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
        size = size
            .checked_add(u64::try_from(count).map_err(|_| ControlError::Contract)?)
            .ok_or(ControlError::Contract)?;
    }
    Ok((format!("{:x}", digest.finalize()), size))
}

fn apply_communication_upload_headers(
    mut request: reqwest::RequestBuilder,
    headers: &BTreeMap<String, String>,
    size_bytes: u64,
) -> Result<reqwest::RequestBuilder, ControlError> {
    let expected_content_length = size_bytes.to_string();
    request = request.header(reqwest::header::CONTENT_LENGTH, &expected_content_length);
    for (name, value) in headers {
        if name.eq_ignore_ascii_case(reqwest::header::CONTENT_LENGTH.as_str()) {
            if value != &expected_content_length {
                return Err(ControlError::Contract);
            }
            continue;
        }
        request = request.header(name, value);
    }
    Ok(request)
}

impl ControlClient for HttpControlClient {
    fn set_network_enabled(&self, enabled: bool) {
        self.network_observations.set_enabled(enabled);
    }

    fn network_observation_available(&self) -> Option<bool> {
        Some(self.network_observations.current_if_enabled().is_some())
    }

    fn refresh<'a>(
        &'a self,
        credentials: &'a DeviceCredential,
    ) -> ControlFuture<'a, DeviceCredential> {
        Box::pin(async move {
            let client = Self::client()?;
            let response = client
                .post(self.endpoint("v1/devices/token/refresh")?)
                .bearer_auth(credentials.refresh_credential())
                .send()
                .await
                .map_err(|_| ControlError::Transient)?;
            let grant = parse_response::<CredentialGrant>(response).await?;
            let access_expires_at_ms = parse_time_ms(&grant.access_expires_at)?;
            let refresh_expires_at_ms = parse_time_ms(&grant.refresh_expires_at)?;
            DeviceCredential::new(
                grant.device_id,
                grant.workspace_id,
                &grant.device_access_token,
                &grant.refresh_token,
            )
            .map(|credential| {
                credential.with_metadata(
                    credentials.credential_generation().saturating_add(1),
                    access_expires_at_ms,
                    refresh_expires_at_ms,
                )
            })
            .map_err(|_| ControlError::Contract)
        })
    }

    fn heartbeat_and_control<'a>(
        &'a self,
        credentials: &'a DeviceCredential,
        outbox_depth: u64,
    ) -> ControlFuture<'a, AgentControlSnapshot> {
        self.heartbeat_and_control_with_media(
            credentials,
            outbox_depth,
            CommunicationMediaStorageStats::default(),
            None,
        )
    }

    fn heartbeat_and_control_with_media<'a>(
        &'a self,
        credentials: &'a DeviceCredential,
        outbox_depth: u64,
        local_media: CommunicationMediaStorageStats,
        cleanup_result: Option<LocalMediaCleanupResult>,
    ) -> ControlFuture<'a, AgentControlSnapshot> {
        Box::pin(async move {
            let client = Self::client()?;
            let request = HeartbeatRequest {
                heartbeat_id: Uuid::new_v4().to_string(),
                agent_version: option_env!("PCA_APP_VERSION")
                    .unwrap_or(env!("CARGO_PKG_VERSION"))
                    .to_owned(),
                presence: "online",
                outbox_depth,
                local_media: LocalMediaHeartbeat {
                    completed_file_count: local_media.completed_file_count,
                    completed_bytes: local_media.completed_bytes,
                    protected_file_count: local_media.protected_file_count,
                    protected_bytes: local_media.protected_bytes,
                },
                network: self.network_observations.current_if_enabled(),
                cleanup_result,
            };
            let response = client
                .post(self.endpoint("v1/agent/control")?)
                .bearer_auth(credentials.access_credential())
                .json(&request)
                .send()
                .await
                .map_err(|_| ControlError::Transient)?;
            parse_response::<ControlResponse>(response)
                .await
                .map(|response| response.snapshot)
        })
    }

    fn report_collector_health<'a>(
        &'a self,
        credentials: &'a DeviceCredential,
        states: &'a [CollectorState],
    ) -> ControlFuture<'a, ()> {
        Box::pin(async move {
            let client = Self::client()?;
            let collectors = states
                .iter()
                .filter_map(CollectorHealthItem::from_valid_state)
                .collect::<Vec<_>>();
            if collectors.is_empty() && !states.is_empty() {
                return Err(ControlError::Contract);
            }
            for collectors in collectors.chunks(16) {
                let request = CollectorHealthRequest {
                    report_id: Uuid::new_v4().to_string(),
                    agent_version: option_env!("PCA_APP_VERSION")
                        .unwrap_or(env!("CARGO_PKG_VERSION"))
                        .to_owned(),
                    collectors: collectors.to_vec(),
                };
                let response = client
                    .post(self.endpoint("v1/agent/collector-health")?)
                    .bearer_auth(credentials.access_credential())
                    .json(&request)
                    .send()
                    .await
                    .map_err(|_| ControlError::Transient)?;
                match response.status() {
                    StatusCode::NO_CONTENT => {}
                    StatusCode::UNAUTHORIZED => return Err(ControlError::InvalidCredential),
                    StatusCode::GONE => return Err(ControlError::Revoked),
                    response_status if response_status.is_server_error() => {
                        return Err(ControlError::Transient);
                    }
                    _ => return Err(ControlError::Contract),
                }
            }
            Ok(())
        })
    }

    fn sync_system_events<'a>(
        &'a self,
        credentials: &'a DeviceCredential,
        events: &'a [EventEnvelope],
    ) -> ControlFuture<'a, SyncEventsResponse> {
        Box::pin(async move {
            let client = Self::client()?;
            let batch_id = Uuid::new_v4().to_string();
            let response = client
                .post(self.endpoint("v1/agent/sync/events")?)
                .bearer_auth(credentials.access_credential())
                .json(&SyncEventsRequest {
                    batch_id: batch_id.clone(),
                    device_id: credentials.device_id(),
                    protocol_version: 1,
                    events,
                })
                .send()
                .await
                .map_err(|_| ControlError::Transient)?;
            let parsed = parse_response::<SyncEventsResponse>(response).await?;
            if parsed.batch_id != batch_id {
                return Err(ControlError::Contract);
            }
            Ok(parsed)
        })
    }

    fn sync_communication_events<'a>(
        &'a self,
        credentials: &'a DeviceCredential,
        events: &'a [EventEnvelope],
    ) -> ControlFuture<'a, SyncEventsResponse> {
        Box::pin(async move {
            let client = Self::client()?;
            let batch_id = Uuid::new_v4().to_string();
            let response = client
                .post(self.endpoint("v1/agent/sync/communication/events")?)
                .bearer_auth(credentials.access_credential())
                .json(&SyncEventsRequest {
                    batch_id: batch_id.clone(),
                    device_id: credentials.device_id(),
                    protocol_version: 1,
                    events,
                })
                .send()
                .await
                .map_err(|_| ControlError::Transient)?;
            let parsed = parse_response::<SyncEventsResponse>(response).await?;
            if parsed.batch_id != batch_id {
                return Err(ControlError::Contract);
            }
            Ok(parsed)
        })
    }

    fn sync_communication_attachment<'a>(
        &'a self,
        credentials: &'a DeviceCredential,
        attachment: &'a PendingCommunicationAttachment,
    ) -> MediaControlFuture<'a> {
        Box::pin(async move {
            let prepare_failure =
                |error| MediaUploadFailure::new(MediaUploadFailureStage::Prepare, error);
            let client = Self::client().map_err(prepare_failure)?;
            let prepare_response = client
                .post(
                    self.endpoint("v1/agent/communication/objects/prepare")
                        .map_err(prepare_failure)?,
                )
                .bearer_auth(credentials.access_credential())
                .json(&serde_json::json!({
                    "event_id": attachment.event_id,
                    "attachment_id": attachment.attachment_id,
                }))
                .send()
                .await
                .map_err(|_| prepare_failure(ControlError::Transient))?;
            let prepared = match parse_communication_prepare_response(prepare_response).await {
                Ok(Some(prepared)) => prepared,
                Ok(None) => return Err(MediaUploadFailure::superseded()),
                Err(error) => return Err(prepare_failure(error)),
            };
            if prepared.state == "completed" {
                return Ok(());
            }
            if prepared.state != "prepared" {
                return Err(prepare_failure(ControlError::Contract));
            }
            let upload = prepared
                .upload
                .ok_or_else(|| prepare_failure(ControlError::Contract))?;
            let upload_url =
                Url::parse(&upload.url).map_err(|_| prepare_failure(ControlError::Contract))?;
            if upload_url.scheme() != "https" {
                return Err(prepare_failure(ControlError::Contract));
            }
            if upload_communication_attachment(
                &client,
                upload_url.clone(),
                &upload.headers,
                attachment,
            )
            .await
            .is_err()
            {
                upload_communication_attachment(
                    &Self::direct_client().map_err(|error| {
                        MediaUploadFailure::after_fallback(
                            MediaUploadFailureStage::DirectUpload,
                            error,
                            MediaUploadFailureStage::ProxyUpload,
                        )
                    })?,
                    upload_url,
                    &upload.headers,
                    attachment,
                )
                .await
                .map_err(|error| {
                    MediaUploadFailure::after_fallback(
                        MediaUploadFailureStage::DirectUpload,
                        error,
                        MediaUploadFailureStage::ProxyUpload,
                    )
                })?;
            }
            let complete_failure =
                |error| MediaUploadFailure::new(MediaUploadFailureStage::Complete, error);
            let completed = parse_response::<CompletedCommunicationObject>(
                Self::client()
                    .map_err(complete_failure)?
                    .post(
                        self.endpoint("v1/agent/communication/objects/complete")
                            .map_err(complete_failure)?,
                    )
                    .bearer_auth(credentials.access_credential())
                    .json(&serde_json::json!({ "object_id": prepared.object_id }))
                    .send()
                    .await
                    .map_err(|_| complete_failure(ControlError::Transient))?,
            )
            .await
            .map_err(complete_failure)?;
            if completed.object_id != prepared.object_id || completed.state != "completed" {
                return Err(complete_failure(ControlError::Contract));
            }
            Ok(())
        })
    }

    fn sync_screenshot<'a>(
        &'a self,
        credentials: &'a DeviceCredential,
        screenshot: &'a PendingScreenshot,
    ) -> ControlFuture<'a, ()> {
        Box::pin(async move {
            let client = Self::client()?;
            let response = client
                .post(self.endpoint("v1/agent/screenshots/prepare")?)
                .bearer_auth(credentials.access_credential())
                .json(&screenshot_prepare_payload(screenshot))
                .send()
                .await
                .map_err(|_| ControlError::Transient)?;
            let prepared = parse_response::<PreparedScreenshot>(response).await?;
            if prepared.screenshot_id != screenshot.screenshot_id {
                return Err(ControlError::Contract);
            }
            if prepared.state != "completed" {
                if prepared.state != "prepared" {
                    return Err(ControlError::Contract);
                }
                let upload = prepared.upload.ok_or(ControlError::Contract)?;
                let upload_url = Url::parse(&upload.url).map_err(|_| ControlError::Contract)?;
                if upload_url.scheme() != "https" {
                    return Err(ControlError::Contract);
                }
                let spool_root = screenshot_spool_root()?;
                if upload_screenshot(
                    &client,
                    upload_url.clone(),
                    &upload.headers,
                    screenshot,
                    &spool_root,
                )
                .await
                .is_err()
                {
                    upload_screenshot(
                        &Self::direct_client()?,
                        upload_url,
                        &upload.headers,
                        screenshot,
                        &spool_root,
                    )
                    .await?;
                }
            }
            let completed = parse_response::<CompletedScreenshot>(
                Self::client()?
                    .post(self.endpoint("v1/agent/screenshots/complete")?)
                    .bearer_auth(credentials.access_credential())
                    .json(&serde_json::json!({ "screenshot_id": screenshot.screenshot_id }))
                    .send()
                    .await
                    .map_err(|_| ControlError::Transient)?,
            )
            .await?;
            if completed.screenshot_id != screenshot.screenshot_id || completed.state != "completed"
            {
                return Err(ControlError::Contract);
            }
            Ok(())
        })
    }

    fn sync_photo<'a>(
        &'a self,
        credentials: &'a DeviceCredential,
        photo: &'a PendingPhoto,
    ) -> ControlFuture<'a, ()> {
        Box::pin(async move {
            let client = Self::client()?;
            let prepared = self
                .prepare_photo_upload(&client, credentials, photo)
                .await?;
            if prepared.state != "completed" {
                if prepared.state != "prepared" {
                    return Err(ControlError::Contract);
                }
                let upload = prepared.upload.ok_or(ControlError::Contract)?;
                let upload_url = Url::parse(&upload.url).map_err(|_| ControlError::Contract)?;
                if upload_url.scheme() != "https" {
                    return Err(ControlError::Contract);
                }
                let root = photo_spool_root()?;
                if upload_photo(&client, upload_url.clone(), &upload.headers, photo, &root)
                    .await
                    .is_err()
                {
                    let refreshed = self
                        .prepare_photo_upload(&client, credentials, photo)
                        .await?;
                    if refreshed.state == "completed" {
                        return Ok(());
                    }
                    if refreshed.state != "prepared" {
                        return Err(ControlError::Contract);
                    }
                    let refreshed_upload = refreshed.upload.ok_or(ControlError::Contract)?;
                    let refreshed_url =
                        Url::parse(&refreshed_upload.url).map_err(|_| ControlError::Contract)?;
                    if refreshed_url.scheme() != "https" {
                        return Err(ControlError::Contract);
                    }
                    upload_photo(
                        &Self::direct_client()?,
                        refreshed_url,
                        &refreshed_upload.headers,
                        photo,
                        &root,
                    )
                    .await?;
                }
            }
            let completed = parse_response::<CompletedPhoto>(
                Self::client()?
                    .post(self.endpoint("v1/agent/photos/complete")?)
                    .bearer_auth(credentials.access_credential())
                    .json(&serde_json::json!({ "photo_id": photo.photo_id }))
                    .send()
                    .await
                    .map_err(|_| ControlError::Transient)?,
            )
            .await?;
            if completed.photo_id != photo.photo_id || completed.state != "completed" {
                return Err(ControlError::Contract);
            }
            Ok(())
        })
    }

    fn fail_screenshot_request<'a>(
        &'a self,
        credentials: &'a DeviceCredential,
        request_id: &'a str,
        error_code: &'static str,
    ) -> ControlFuture<'a, ()> {
        Box::pin(async move {
            let response = Self::client()?
                .post(self.endpoint("v1/agent/screenshots/fail")?)
                .bearer_auth(credentials.access_credential())
                .json(&serde_json::json!({
                    "request_id": request_id,
                    "error_code": error_code,
                }))
                .send()
                .await
                .map_err(|_| ControlError::Transient)?;
            if response.status() == StatusCode::NO_CONTENT {
                Ok(())
            } else {
                Err(classify_status(response.status()))
            }
        })
    }
}

impl PairingClient for HttpControlClient {
    fn create_pairing_session<'a>(
        &'a self,
        request: &'a PairingSessionRequest,
    ) -> ControlFuture<'a, PairingSessionResponse> {
        Box::pin(async move {
            let client = Self::client()?;
            let response = client
                .post(self.endpoint("v1/device-pairing/sessions")?)
                .json(request)
                .send()
                .await
                .map_err(|_| ControlError::Transient)?;
            parse_response(response).await
        })
    }

    fn exchange_pairing_callback<'a>(
        &'a self,
        request: &'a PairingExchangeRequest,
    ) -> ControlFuture<'a, DeviceCredential> {
        Box::pin(async move {
            let client = Self::client()?;
            let response = client
                .post(self.endpoint("v1/device-pairing/exchange")?)
                .json(request)
                .send()
                .await
                .map_err(|_| ControlError::Transient)?;
            let grant = parse_response::<CredentialGrant>(response).await?;
            let access_expires_at_ms = parse_time_ms(&grant.access_expires_at)?;
            let refresh_expires_at_ms = parse_time_ms(&grant.refresh_expires_at)?;
            DeviceCredential::new(
                grant.device_id,
                grant.workspace_id,
                &grant.device_access_token,
                &grant.refresh_token,
            )
            .map(|credential| {
                credential.with_metadata(0, access_expires_at_ms, refresh_expires_at_ms)
            })
            .map_err(|_| ControlError::Contract)
        })
    }
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct HeartbeatRequest {
    heartbeat_id: String,
    agent_version: String,
    presence: &'static str,
    outbox_depth: u64,
    local_media: LocalMediaHeartbeat,
    network: Option<NetworkObservation>,
    cleanup_result: Option<LocalMediaCleanupResult>,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct LocalMediaHeartbeat {
    completed_file_count: u64,
    completed_bytes: u64,
    protected_file_count: u64,
    protected_bytes: u64,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct CollectorHealthRequest {
    report_id: String,
    agent_version: String,
    collectors: Vec<CollectorHealthItem>,
}

#[derive(Clone, Serialize)]
#[serde(deny_unknown_fields)]
struct CollectorHealthItem {
    collector_key: String,
    collector_version: String,
    status: CollectorStatus,
    desired_config_revision: u64,
    applied_config_revision: u64,
    last_event_at_ms: Option<i64>,
    last_health_at_ms: Option<i64>,
    error_code: Option<String>,
}

impl CollectorHealthItem {
    fn from_valid_state(state: &CollectorState) -> Option<Self> {
        if !valid_collector_key(&state.collector_key)
            || state.collector_version.is_empty()
            || state.collector_version.len() > 64
            || state.last_event_at_ms.is_some_and(|value| value < 0)
            || state.last_health_at_ms.is_some_and(|value| value < 0)
            || state
                .last_error_code
                .as_deref()
                .is_some_and(|value| !valid_error_code(value))
        {
            return None;
        }
        Some(Self {
            collector_key: state.collector_key.clone(),
            collector_version: state.collector_version.clone(),
            status: state.status,
            desired_config_revision: state.desired_config_revision,
            applied_config_revision: state.applied_config_revision,
            last_event_at_ms: state.last_event_at_ms,
            last_health_at_ms: state.last_health_at_ms,
            error_code: state.last_error_code.clone(),
        })
    }
}

fn valid_collector_key(value: &str) -> bool {
    let mut bytes = value.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z'))
        && value.len() <= 64
        && bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-')
        })
}

fn valid_error_code(value: &str) -> bool {
    let mut bytes = value.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z'))
        && value.len() <= 128
        && bytes.all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

#[derive(Serialize)]
struct SyncEventsRequest<'a> {
    batch_id: String,
    device_id: &'a str,
    protocol_version: u8,
    events: &'a [EventEnvelope],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialGrant {
    workspace_id: String,
    device_id: String,
    device_access_token: String,
    refresh_token: String,
    access_expires_at: String,
    refresh_expires_at: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlResponse {
    snapshot: AgentControlSnapshot,
    #[allow(dead_code)]
    server_time: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ErrorResponse {
    error: ErrorBody,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ErrorBody {
    error_code: String,
    #[allow(dead_code)]
    message: String,
    #[allow(dead_code)]
    retryable: bool,
}

async fn parse_response<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
) -> Result<T, ControlError> {
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|_| ControlError::Transient)?;
    parse_response_bytes(status, &bytes)
}

fn parse_response_bytes<T: for<'de> Deserialize<'de>>(
    status: StatusCode,
    bytes: &[u8],
) -> Result<T, ControlError> {
    if status.is_success() {
        return serde_json::from_slice(bytes).map_err(|_| ControlError::Contract);
    }
    if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        return Err(ControlError::Transient);
    }
    if matches!(
        status,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::GONE
    ) {
        return match serde_json::from_slice::<ErrorResponse>(bytes) {
            Ok(error) if error.error.error_code == "DEVICE_REVOKED" => Err(ControlError::Revoked),
            _ => Err(ControlError::InvalidCredential),
        };
    }
    Err(ControlError::Contract)
}

async fn parse_communication_prepare_response(
    response: reqwest::Response,
) -> Result<Option<PreparedCommunicationObject>, ControlError> {
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|_| ControlError::Transient)?;
    if communication_attachment_is_missing(status, &bytes) {
        return Ok(None);
    }
    parse_response_bytes(status, &bytes).map(Some)
}

fn communication_attachment_is_missing(status: StatusCode, bytes: &[u8]) -> bool {
    status == StatusCode::NOT_FOUND
        && matches!(
            serde_json::from_slice::<ErrorResponse>(bytes),
            Ok(error) if error.error.error_code == "COMMUNICATION_ATTACHMENT_NOT_FOUND"
        )
}

fn classify_status(status: StatusCode) -> ControlError {
    if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        ControlError::Transient
    } else if matches!(
        status,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::GONE
    ) {
        ControlError::InvalidCredential
    } else {
        ControlError::Contract
    }
}

fn parse_time_ms(value: &str) -> Result<i64, ControlError> {
    let timestamp = OffsetDateTime::parse(value, &Rfc3339).map_err(|_| ControlError::Contract)?;
    i64::try_from(timestamp.unix_timestamp_nanos() / 1_000_000).map_err(|_| ControlError::Contract)
}

#[cfg(test)]
mod tests {
    use super::{
        apply_communication_upload_headers, attachment_was_superseded,
        communication_attachment_collector_key, communication_attachment_is_missing,
        complete_persisted_revision, mark_manually_unpaired, next_media_wait,
        persist_aux_collector_state, persist_media_upload_outcome, photo_marker_path,
        photo_upload_timeout, quarantine_invalid_screenshot_manifest, quarantine_manifest,
        recover_media_upload_health, remove_uploaded_media_file,
        restore_screenshot_request_history, retry_delay, run_collector_health_loop,
        run_restartable_data_plane_worker, screenshot_prepare_payload,
        sync_pending_communication_events, sync_pending_system_events, terminal_rejected_event_ids,
        upload_pending_screenshots, AgentControlSnapshot, AgentPairingService, AppliedControl,
        CollectorHealthItem, CommunicationAuthorization, ControlClient, ControlError,
        ControlFuture, ControlPublication, ControlState, DeviceCredential, HttpControlClient,
        LoadedDeviceCredentials, MediaUploadFailure, MediaUploadFailureStage, MediaUploadOutcome,
        PairingCallbackHandoff, PairingClient, PairingExchangeRequest, PairingSessionRequest,
        PairingSessionResponse, PairingStartHandoff, PendingScreenshot, PhotoMarker,
        ScreenCaptureControl, ScreenshotTrigger, SyncEventRejection, SyncEventsResponse,
        COMMUNICATION_MEDIA_UPLOAD_FAILED, CONTROL_INTERVAL, CONTROL_REQUEST_TIMEOUT, MAX_BACKOFF,
        MEDIA_BATCH_SIZE, MEDIA_CYCLE_TIMEOUT, MEDIA_CYCLE_TIMEOUT_ERROR, MEDIA_UPLOAD_TIMEOUT,
        PHOTOS_UPLOAD_FAILED, PHOTO_UPLOAD_MAX_TIMEOUT, PRODUCTION_CLOUD_API_ORIGIN,
        SCREEN_LOCAL_MANIFEST_INVALID, SCREEN_UPLOAD_FAILED, SCREEN_UPLOAD_TIMEOUT,
        SYNC_PAYLOAD_REJECTED,
    };
    use pca_db_local::{
        AppliedCollectorControl, CommunicationMessageCommit, DbActorHandle, PairingState,
    };
    use pca_domain::{
        CollectorState, CollectorStatus, CommunicationMessageRecorded,
        CommunicationMessageRecordedInput, ConversationScope, Direction, EventEnvelope,
        MessageKind, Sensitivity,
    };
    use pca_keychain::{CredentialError, CredentialStore};
    use reqwest::Url;
    use serde_json::{Map, Value};
    use sha2::{Digest, Sha256};
    use std::{
        env,
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc, Mutex as StdMutex,
        },
        time::Duration,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::{oneshot, watch, Mutex as AsyncMutex, Notify},
        time::timeout,
    };

    static PROXY_ENVIRONMENT_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());

    #[tokio::test(start_paused = true)]
    async fn panicked_data_plane_worker_restarts_without_stopping_its_supervisor() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let restarted = Arc::new(Notify::new());
        let (shutdown, shutdown_receiver) = watch::channel(false);
        let restarted_wait = restarted.notified();
        let worker = tokio::spawn(run_restartable_data_plane_worker(
            "test",
            shutdown_receiver,
            {
                let attempts = Arc::clone(&attempts);
                let restarted = Arc::clone(&restarted);
                move || {
                    let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                    let restarted = Arc::clone(&restarted);
                    async move {
                        assert!(attempt != 0, "deterministic data-plane panic");
                        restarted.notify_one();
                        std::future::pending().await
                    }
                }
            },
        ));

        tokio::task::yield_now().await;
        tokio::time::advance(CONTROL_INTERVAL).await;
        restarted_wait.await;
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(!worker.is_finished());

        shutdown.send_replace(true);
        assert!(worker.await.expect("join supervisor").is_ok());
    }

    #[derive(Default)]
    struct EmptyCredentialStore;

    impl CredentialStore for EmptyCredentialStore {
        fn load(&self, _: &str, _: &str) -> Result<Option<Vec<u8>>, CredentialError> {
            Ok(None)
        }

        fn store(&self, _: &str, _: &str, _: &[u8]) -> Result<(), CredentialError> {
            Ok(())
        }

        fn delete(&self, _: &str, _: &str) -> Result<(), CredentialError> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct DeleteCountingCredentialStore {
        deletes: AtomicUsize,
    }

    impl CredentialStore for DeleteCountingCredentialStore {
        fn load(&self, _: &str, _: &str) -> Result<Option<Vec<u8>>, CredentialError> {
            Ok(None)
        }

        fn store(&self, _: &str, _: &str, _: &[u8]) -> Result<(), CredentialError> {
            Ok(())
        }

        fn delete(&self, _: &str, _: &str) -> Result<(), CredentialError> {
            self.deletes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn legacy_bootstrap_does_not_suppress_the_first_complete_same_revision_snapshot() {
        let credential = DeviceCredential::new(
            "01982222-7222-8222-8222-222222222222".to_owned(),
            "01983333-7333-8333-8333-333333333333".to_owned(),
            "access",
            "refresh",
        )
        .expect("valid credential");
        let mut control = AppliedCollectorControl {
            device_id: credential.device_id().to_owned(),
            workspace_id: credential.workspace_id().to_owned(),
            configuration_revision: 0,
            communication_wechat_enabled: true,
            screen_capture_enabled: false,
            screen_capture_scheduled_enabled: false,
            screen_capture_interval_seconds: 300,
            screen_capture_activity_enabled: false,
            screen_capture_activity_min_interval_seconds: 30,
            screen_capture_excluded_bundle_ids: Vec::new(),
            updated_at_ms: 1,
        };

        assert_eq!(
            complete_persisted_revision(&credential, 5, Some(&control)),
            0
        );
        control.configuration_revision = 5;
        assert_eq!(
            complete_persisted_revision(&credential, 5, Some(&control)),
            5
        );
    }

    #[tokio::test]
    async fn failed_manual_unpair_persistence_does_not_delete_the_credential() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let database = Arc::new(
            DbActorHandle::open(&directory.path().join("agent.sqlite3"), "test")
                .await
                .expect("open database"),
        );
        let store = Arc::new(DeleteCountingCredentialStore::default());
        let credentials = LoadedDeviceCredentials::new(
            DeviceCredential::new(
                "01982222-7222-8222-8222-222222222222".to_owned(),
                "01983333-7333-8333-8333-333333333333".to_owned(),
                "access",
                "refresh",
            )
            .expect("valid credential"),
            Arc::clone(&store) as Arc<dyn CredentialStore>,
        );
        let authorization = CommunicationAuthorization::new();
        let owner_epoch = authorization.owner_epoch().await;
        let (control_sender, _) = watch::channel(None);
        let publication = ControlPublication::new(control_sender, owner_epoch);
        let state = Arc::new(AsyncMutex::new(ControlState {
            unpaired: false,
            applied_revision: Some(5),
            communication_hydrated: true,
            pending_media_cleanup_result: None,
        }));
        let (pairing_state_sender, _) = watch::channel(true);

        assert!(mark_manually_unpaired(
            database.as_ref(),
            &credentials,
            &state,
            &pairing_state_sender,
            &publication,
            &authorization,
            owner_epoch,
        )
        .await
        .is_err());
        assert_eq!(store.deletes.load(Ordering::SeqCst), 0);

        drop(credentials);
        drop(state);
        match Arc::try_unwrap(database) {
            Ok(database) => database.shutdown().await.expect("shutdown database"),
            Err(error) => {
                drop(error);
                panic!("release database");
            }
        }
    }

    struct CapturingPairingClient {
        request: Arc<StdMutex<Option<PairingSessionRequest>>>,
    }

    struct RetryingPairingClient {
        exchanges: Arc<AtomicUsize>,
    }

    struct BlockingPairingClient {
        calls: AtomicUsize,
        entered: Notify,
        release: Notify,
    }

    impl PairingClient for CapturingPairingClient {
        fn create_pairing_session<'a>(
            &'a self,
            request: &'a PairingSessionRequest,
        ) -> ControlFuture<'a, PairingSessionResponse> {
            let captured = Arc::clone(&self.request);
            let request = request.clone();
            Box::pin(async move {
                *captured.lock().expect("pairing request lock") = Some(request);
                Ok(PairingSessionResponse {
                    session_id: "01981111-7111-8111-8111-111111111111".to_owned(),
                    authorization_url: "https://dashboard.example.invalid/pair".to_owned(),
                })
            })
        }

        fn exchange_pairing_callback<'a>(
            &'a self,
            _: &'a PairingExchangeRequest,
        ) -> ControlFuture<'a, DeviceCredential> {
            Box::pin(async { Err(ControlError::Contract) })
        }
    }

    impl PairingClient for RetryingPairingClient {
        fn create_pairing_session<'a>(
            &'a self,
            _: &'a PairingSessionRequest,
        ) -> ControlFuture<'a, PairingSessionResponse> {
            Box::pin(async {
                Ok(PairingSessionResponse {
                    session_id: "01981111-7111-8111-8111-111111111111".to_owned(),
                    authorization_url: "https://dashboard.example.invalid/pair".to_owned(),
                })
            })
        }

        fn exchange_pairing_callback<'a>(
            &'a self,
            _: &'a PairingExchangeRequest,
        ) -> ControlFuture<'a, DeviceCredential> {
            Box::pin(async move {
                if self.exchanges.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Err(ControlError::Transient);
                }
                DeviceCredential::new(
                    "01982222-7222-8222-8222-222222222222".to_owned(),
                    "01983333-7333-8333-8333-333333333333".to_owned(),
                    "access",
                    "refresh",
                )
                .map_err(|_| ControlError::Contract)
            })
        }
    }

    impl PairingClient for BlockingPairingClient {
        fn create_pairing_session<'a>(
            &'a self,
            _: &'a PairingSessionRequest,
        ) -> ControlFuture<'a, PairingSessionResponse> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.entered.notify_one();
                self.release.notified().await;
                Ok(PairingSessionResponse {
                    session_id: "01981111-7111-8111-8111-111111111111".to_owned(),
                    authorization_url: "https://dashboard.example.invalid/pair".to_owned(),
                })
            })
        }

        fn exchange_pairing_callback<'a>(
            &'a self,
            _: &'a PairingExchangeRequest,
        ) -> ControlFuture<'a, DeviceCredential> {
            Box::pin(async { Err(ControlError::Contract) })
        }
    }

    #[tokio::test]
    async fn concurrent_pairing_begin_creates_only_one_cloud_session() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let database = Arc::new(
            DbActorHandle::open(&directory.path().join("agent.sqlite3"), "test")
                .await
                .expect("open database"),
        );
        let client = Arc::new(BlockingPairingClient {
            calls: AtomicUsize::new(0),
            entered: Notify::new(),
            release: Notify::new(),
        });
        let service = Arc::new(AgentPairingService::new(
            Arc::clone(&database),
            Arc::new(EmptyCredentialStore),
            Arc::clone(&client) as Arc<dyn PairingClient>,
        ));
        let entered = client.entered.notified();
        let begin = || PairingStartHandoff {
            callback_uri: "http://127.0.0.1:43123/pca/pair/callback".to_owned(),
        };

        let first_service = Arc::clone(&service);
        let first = tokio::spawn(async move { first_service.begin(begin()).await });
        entered.await;
        let second_service = Arc::clone(&service);
        let second = tokio::spawn(async move { second_service.begin(begin()).await });
        tokio::task::yield_now().await;
        assert_eq!(client.calls.load(Ordering::SeqCst), 1);

        client.release.notify_one();
        assert!(first.await.expect("join first begin").is_ok());
        assert!(second.await.expect("join second begin").is_err());
        assert_eq!(client.calls.load(Ordering::SeqCst), 1);

        drop(service);
        drop(client);
        let database = Arc::try_unwrap(database).unwrap_or_else(|_| panic!("release database"));
        database.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn pairing_repair_requests_the_durable_existing_device_id() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let database = Arc::new(
            DbActorHandle::open(&directory.path().join("agent.sqlite3"), "test")
                .await
                .expect("open database"),
        );
        let device_id = "01982222-7222-8222-8222-222222222222";
        database
            .save_pairing_state(&PairingState::paired(
                device_id,
                "01983333-7333-8333-8333-333333333333",
                "keychain://pca/device/current",
                4,
                PRODUCTION_CLOUD_API_ORIGIN,
            ))
            .await
            .expect("seed pairing state");
        let request = Arc::new(StdMutex::new(None));
        let client = Arc::new(CapturingPairingClient {
            request: Arc::clone(&request),
        });
        let service = AgentPairingService::new(
            Arc::clone(&database),
            Arc::new(EmptyCredentialStore),
            client,
        );

        service
            .begin(PairingStartHandoff {
                callback_uri: "http://127.0.0.1:43123/pca/pair/callback".to_owned(),
            })
            .await
            .expect("begin repair pairing");

        assert_eq!(
            request
                .lock()
                .expect("pairing request lock")
                .as_ref()
                .and_then(|request| request.existing_device_id.as_deref()),
            Some(device_id),
        );
        drop(service);
        let Ok(database) = Arc::try_unwrap(database) else {
            panic!("release database");
        };
        database.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn pairing_completion_survives_wrong_session_and_transient_exchange_failure() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let database = Arc::new(
            DbActorHandle::open(&directory.path().join("agent.sqlite3"), "test")
                .await
                .expect("open database"),
        );
        let exchanges = Arc::new(AtomicUsize::new(0));
        let service = AgentPairingService::new(
            Arc::clone(&database),
            Arc::new(EmptyCredentialStore),
            Arc::new(RetryingPairingClient {
                exchanges: Arc::clone(&exchanges),
            }),
        );
        let session = service
            .begin(PairingStartHandoff {
                callback_uri: "http://127.0.0.1:43123/pca/pair/callback".to_owned(),
            })
            .await
            .expect("begin pairing");

        assert!(service
            .complete(PairingCallbackHandoff {
                session_id: "01984444-7444-8444-8444-444444444444".to_owned(),
                authorization_code: "code".to_owned(),
            })
            .await
            .is_err());
        assert_eq!(exchanges.load(Ordering::SeqCst), 0);

        let callback = PairingCallbackHandoff {
            session_id: session.session_id,
            authorization_code: "code".to_owned(),
        };
        assert!(service.complete(callback.clone()).await.is_err());
        assert_eq!(exchanges.load(Ordering::SeqCst), 1);
        let completion = service
            .complete(callback.clone())
            .await
            .expect("retry pairing completion");
        assert_eq!(completion.device_id, "01982222-7222-8222-8222-222222222222");
        assert_eq!(exchanges.load(Ordering::SeqCst), 2);
        assert!(service.complete(callback).await.is_err());
        assert_eq!(exchanges.load(Ordering::SeqCst), 2);

        drop(service);
        let Ok(database) = Arc::try_unwrap(database) else {
            panic!("release database");
        };
        database.shutdown().await.expect("shutdown database");
    }

    #[test]
    fn communication_attachment_health_uses_only_canonical_collector_sources() {
        assert_eq!(
            communication_attachment_collector_key("communication.wechat"),
            Ok("communication.wechat")
        );
        assert_eq!(
            communication_attachment_collector_key("communication.messages"),
            Ok("communication.messages")
        );
        assert_eq!(
            communication_attachment_collector_key("wechat"),
            Ok("communication.wechat")
        );
        assert!(communication_attachment_collector_key("unknown").is_err());
    }

    #[tokio::test]
    async fn media_upload_failure_degrades_only_its_collector_and_success_recovers() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let database = DbActorHandle::open(&directory.path().join("agent.sqlite3"), "test")
            .await
            .expect("open database");
        for collector_key in ["communication.wechat", "communication.messages"] {
            database
                .upsert_collector_state(&CollectorState {
                    collector_key: collector_key.to_owned(),
                    collector_version: "test".to_owned(),
                    status: CollectorStatus::Running,
                    desired_config_revision: 1,
                    applied_config_revision: 1,
                    last_event_at_ms: Some(1),
                    last_health_at_ms: Some(1),
                    last_error_code: None,
                    created_at_ms: 1,
                    updated_at_ms: 1,
                })
                .await
                .expect("seed collector health");
        }

        let failed = MediaUploadOutcome::failed(["communication.wechat"]);
        persist_media_upload_outcome(&database, &failed, COMMUNICATION_MEDIA_UPLOAD_FAILED)
            .await
            .expect("persist failed upload health");
        let states = database
            .load_collector_states()
            .await
            .expect("load degraded collector states");
        assert!(states.iter().any(|state| {
            state.collector_key == "communication.wechat"
                && state.status == CollectorStatus::Degraded
                && state.last_error_code.as_deref() == Some(COMMUNICATION_MEDIA_UPLOAD_FAILED)
        }));
        assert!(states.iter().any(|state| {
            state.collector_key == "communication.messages"
                && state.status == CollectorStatus::Running
                && state.last_error_code.is_none()
        }));

        let mut succeeded = MediaUploadOutcome::default();
        succeeded.record_success("communication.wechat");
        persist_media_upload_outcome(&database, &succeeded, COMMUNICATION_MEDIA_UPLOAD_FAILED)
            .await
            .expect("persist recovered upload health");
        let recovered = database
            .load_collector_states()
            .await
            .expect("load recovered collector state")
            .into_iter()
            .find(|state| state.collector_key == "communication.wechat")
            .expect("WeChat collector state");
        assert_eq!(recovered.status, CollectorStatus::Running);
        assert_eq!(recovered.last_error_code, None);
        database.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn media_cycle_timeout_degrades_every_affected_media_collector() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let database = DbActorHandle::open(&directory.path().join("agent.sqlite3"), "test")
            .await
            .expect("open database");
        for collector_key in [
            "communication.wechat",
            "communication.messages",
            "photos.library",
        ] {
            database
                .upsert_collector_state(&CollectorState {
                    collector_key: collector_key.to_owned(),
                    collector_version: "test".to_owned(),
                    status: CollectorStatus::Running,
                    desired_config_revision: 1,
                    applied_config_revision: 1,
                    last_event_at_ms: Some(1),
                    last_health_at_ms: Some(1),
                    last_error_code: None,
                    created_at_ms: 1,
                    updated_at_ms: 1,
                })
                .await
                .expect("seed collector health");
        }

        persist_media_upload_outcome(
            &database,
            &MediaUploadOutcome::failed([
                "communication.wechat",
                "communication.messages",
                "photos.library",
            ]),
            MEDIA_CYCLE_TIMEOUT_ERROR,
        )
        .await
        .expect("persist media cycle timeout");
        let states = database
            .load_collector_states()
            .await
            .expect("load degraded collector states");
        for collector_key in [
            "communication.wechat",
            "communication.messages",
            "photos.library",
        ] {
            let state = states
                .iter()
                .find(|state| state.collector_key == collector_key)
                .expect("seeded collector state");
            assert_eq!(state.status, CollectorStatus::Degraded);
            assert_eq!(
                state.last_error_code.as_deref(),
                Some(MEDIA_CYCLE_TIMEOUT_ERROR)
            );
        }
        database.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn media_uploads_do_not_mask_a_primary_failure_or_clear_their_own_error_from_collection()
    {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let database = DbActorHandle::open(&directory.path().join("agent.sqlite3"), "test")
            .await
            .expect("open database");
        for (collector_key, status, error_code) in [
            (
                "communication.wechat",
                CollectorStatus::PermissionRequired,
                Some("WECHAT_PERMISSION_REQUIRED"),
            ),
            (
                "photos.library",
                CollectorStatus::Degraded,
                Some(PHOTOS_UPLOAD_FAILED),
            ),
        ] {
            database
                .upsert_collector_state(&CollectorState {
                    collector_key: collector_key.to_owned(),
                    collector_version: "test".to_owned(),
                    status,
                    desired_config_revision: 1,
                    applied_config_revision: 1,
                    last_event_at_ms: Some(1),
                    last_health_at_ms: Some(1),
                    last_error_code: error_code.map(str::to_owned),
                    created_at_ms: 1,
                    updated_at_ms: 1,
                })
                .await
                .expect("seed collector state");
        }

        let failed = MediaUploadOutcome::failed(["communication.wechat"]);
        persist_media_upload_outcome(&database, &failed, COMMUNICATION_MEDIA_UPLOAD_FAILED)
            .await
            .expect("persist media failure");
        let mut succeeded = MediaUploadOutcome::default();
        succeeded.record_success("communication.wechat");
        persist_media_upload_outcome(&database, &succeeded, COMMUNICATION_MEDIA_UPLOAD_FAILED)
            .await
            .expect("persist media success");
        persist_aux_collector_state(&database, "photos.library", true, 1, true, None)
            .await
            .expect("persist successful photo collection");

        let states = database
            .load_collector_states()
            .await
            .expect("load collector states");
        for (collector_key, status, error_code) in [
            (
                "communication.wechat",
                CollectorStatus::PermissionRequired,
                Some("WECHAT_PERMISSION_REQUIRED"),
            ),
            (
                "photos.library",
                CollectorStatus::Degraded,
                Some(PHOTOS_UPLOAD_FAILED),
            ),
        ] {
            let state = states
                .iter()
                .find(|state| state.collector_key == collector_key)
                .expect("seeded collector state");
            assert_eq!(state.status, status);
            assert_eq!(state.last_error_code.as_deref(), error_code);
        }
        database.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn repeated_disabled_aux_collector_state_does_not_refresh_updated_at() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let database = DbActorHandle::open(&directory.path().join("agent.sqlite3"), "test")
            .await
            .expect("open database");
        let version = option_env!("PCA_APP_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"));
        database
            .upsert_collector_state(&CollectorState {
                collector_key: "photos.library".to_owned(),
                collector_version: version.to_owned(),
                status: CollectorStatus::Disabled,
                desired_config_revision: 5,
                applied_config_revision: 5,
                last_event_at_ms: Some(1),
                last_health_at_ms: Some(2),
                last_error_code: None,
                created_at_ms: 1,
                updated_at_ms: 7,
            })
            .await
            .expect("seed disabled collector");

        persist_aux_collector_state(&database, "photos.library", false, 5, false, None)
            .await
            .expect("observe repeated disabled state");
        let state = database
            .load_collector_states()
            .await
            .expect("load collector states")
            .into_iter()
            .find(|state| state.collector_key == "photos.library")
            .expect("photos collector exists");
        assert_eq!(state.updated_at_ms, 7);
        assert_eq!(state.last_health_at_ms, Some(2));
        database.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn successful_screenshot_upload_recovers_a_matching_upload_failure() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let database = DbActorHandle::open(&directory.path().join("agent.sqlite3"), "test")
            .await
            .expect("open database");
        database
            .upsert_collector_state(&CollectorState {
                collector_key: "screen.capture".to_owned(),
                collector_version: "test".to_owned(),
                status: CollectorStatus::Degraded,
                desired_config_revision: 1,
                applied_config_revision: 1,
                last_event_at_ms: Some(1),
                last_health_at_ms: Some(1),
                last_error_code: Some(SCREEN_UPLOAD_TIMEOUT.to_owned()),
                created_at_ms: 1,
                updated_at_ms: 1,
            })
            .await
            .expect("seed screenshot upload failure");

        recover_media_upload_health(
            &database,
            "screen.capture",
            &[SCREEN_UPLOAD_FAILED, SCREEN_UPLOAD_TIMEOUT],
        )
        .await
        .expect("recover screenshot upload health");

        let state = database
            .load_collector_states()
            .await
            .expect("load collector state")
            .into_iter()
            .find(|state| state.collector_key == "screen.capture")
            .expect("screen capture collector state");
        assert_eq!(state.status, CollectorStatus::Running);
        assert_eq!(state.last_error_code, None);
        database.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn first_screenshot_upload_timeout_degrades_a_running_collector() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let database = DbActorHandle::open(&directory.path().join("agent.sqlite3"), "test")
            .await
            .expect("open database");
        database
            .upsert_collector_state(&CollectorState {
                collector_key: "screen.capture".to_owned(),
                collector_version: "test".to_owned(),
                status: CollectorStatus::Running,
                desired_config_revision: 1,
                applied_config_revision: 1,
                last_event_at_ms: Some(1),
                last_health_at_ms: Some(1),
                last_error_code: None,
                created_at_ms: 1,
                updated_at_ms: 1,
            })
            .await
            .expect("seed running screenshot collector");

        persist_aux_collector_state(
            &database,
            "screen.capture",
            true,
            1,
            false,
            Some(SCREEN_UPLOAD_TIMEOUT),
        )
        .await
        .expect("persist first screenshot timeout");

        let state = database
            .load_collector_states()
            .await
            .expect("load collector state")
            .into_iter()
            .find(|state| state.collector_key == "screen.capture")
            .expect("screen capture collector state");
        assert_eq!(state.status, CollectorStatus::Degraded);
        assert_eq!(
            state.last_error_code.as_deref(),
            Some(SCREEN_UPLOAD_TIMEOUT)
        );
        database.shutdown().await.expect("shutdown database");
    }

    struct CollectorHealthCountingClient(AtomicUsize);

    struct NetworkCollectorHealthClient {
        reports: AtomicUsize,
        report_updates: watch::Sender<usize>,
        available: AtomicBool,
    }

    impl ControlClient for CollectorHealthCountingClient {
        fn refresh<'a>(&'a self, _: &'a DeviceCredential) -> ControlFuture<'a, DeviceCredential> {
            Box::pin(async { Err(ControlError::Contract) })
        }

        fn heartbeat_and_control<'a>(
            &'a self,
            _: &'a DeviceCredential,
            _: u64,
        ) -> ControlFuture<'a, AgentControlSnapshot> {
            Box::pin(async { Err(ControlError::Contract) })
        }

        fn report_collector_health<'a>(
            &'a self,
            _: &'a DeviceCredential,
            states: &'a [CollectorState],
        ) -> ControlFuture<'a, ()> {
            assert_eq!(states.len(), 1);
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    }

    impl ControlClient for NetworkCollectorHealthClient {
        fn network_observation_available(&self) -> Option<bool> {
            Some(self.available.load(Ordering::SeqCst))
        }

        fn refresh<'a>(&'a self, _: &'a DeviceCredential) -> ControlFuture<'a, DeviceCredential> {
            Box::pin(async { Err(ControlError::Contract) })
        }

        fn heartbeat_and_control<'a>(
            &'a self,
            _: &'a DeviceCredential,
            _: u64,
        ) -> ControlFuture<'a, AgentControlSnapshot> {
            Box::pin(async { Err(ControlError::Contract) })
        }

        fn report_collector_health<'a>(
            &'a self,
            _: &'a DeviceCredential,
            _: &'a [CollectorState],
        ) -> ControlFuture<'a, ()> {
            Box::pin(async {
                let reports = self.reports.fetch_add(1, Ordering::SeqCst) + 1;
                self.report_updates.send_replace(reports);
                Ok(())
            })
        }
    }

    #[test]
    fn collector_health_wire_item_rejects_one_invalid_state_without_rewriting_it() {
        let valid = CollectorState {
            collector_key: "wechat.messages".to_owned(),
            collector_version: "0.2.0".to_owned(),
            status: CollectorStatus::Degraded,
            desired_config_revision: 5,
            applied_config_revision: 5,
            last_event_at_ms: Some(1),
            last_health_at_ms: Some(2),
            last_error_code: Some("COMMUNICATION_INVALID_RECORD".to_owned()),
            created_at_ms: 1,
            updated_at_ms: 2,
        };
        assert!(CollectorHealthItem::from_valid_state(&valid).is_some());

        let mut invalid = valid;
        invalid.last_error_code = Some("invalid error".to_owned());
        assert!(CollectorHealthItem::from_valid_state(&invalid).is_none());
        assert_eq!(invalid.last_error_code.as_deref(), Some("invalid error"));
    }

    #[test]
    fn collector_health_wire_item_rejects_fields_that_would_reject_the_cloud_batch() {
        let baseline = CollectorState {
            collector_key: "system".to_owned(),
            collector_version: "0.2.0".to_owned(),
            status: CollectorStatus::Running,
            desired_config_revision: 0,
            applied_config_revision: 0,
            last_event_at_ms: None,
            last_health_at_ms: None,
            last_error_code: None,
            created_at_ms: 1,
            updated_at_ms: 1,
        };

        let mut invalid_key = baseline.clone();
        invalid_key.collector_key = "System".to_owned();
        assert!(CollectorHealthItem::from_valid_state(&invalid_key).is_none());

        let mut invalid_version = baseline.clone();
        invalid_version.collector_version = "x".repeat(65);
        assert!(CollectorHealthItem::from_valid_state(&invalid_version).is_none());

        let mut invalid_timestamp = baseline;
        invalid_timestamp.last_health_at_ms = Some(-1);
        assert!(CollectorHealthItem::from_valid_state(&invalid_timestamp).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn collector_health_reports_only_once_per_thirty_minutes() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let database = Arc::new(
            DbActorHandle::open(&directory.path().join("agent.sqlite3"), "test")
                .await
                .expect("open database"),
        );
        database
            .upsert_collector_state(&CollectorState {
                collector_key: "system".to_owned(),
                collector_version: "test".to_owned(),
                status: CollectorStatus::Running,
                desired_config_revision: 1,
                applied_config_revision: 1,
                last_event_at_ms: Some(1),
                last_health_at_ms: Some(1),
                last_error_code: None,
                created_at_ms: 1,
                updated_at_ms: 1,
            })
            .await
            .expect("store collector state");
        let credential = DeviceCredential::new(
            "01985555-7555-8555-8555-555555555555".to_owned(),
            "01982222-7222-8222-8222-222222222222".to_owned(),
            "access-credential-for-health-test",
            "refresh-credential-for-health-test",
        )
        .expect("valid credential");
        let (_, credential_receiver) = watch::channel(credential);
        let (_control, control_receiver) = watch::channel(None);
        let (shutdown, shutdown_receiver) = watch::channel(false);
        let client = Arc::new(CollectorHealthCountingClient(AtomicUsize::new(0)));
        let worker = tokio::spawn(run_collector_health_loop(
            Arc::clone(&database),
            credential_receiver,
            control_receiver,
            Arc::clone(&client) as Arc<dyn ControlClient>,
            shutdown_receiver,
        ));

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(9)).await;
        tokio::task::yield_now().await;
        assert_eq!(client.0.load(Ordering::SeqCst), 0);
        tokio::time::advance(Duration::from_secs(1)).await;
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        assert_eq!(client.0.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(30 * 60 - 1)).await;
        tokio::task::yield_now().await;
        assert_eq!(client.0.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(1)).await;
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        assert_eq!(client.0.load(Ordering::SeqCst), 2);

        shutdown.send_replace(true);
        worker
            .await
            .expect("join health worker")
            .expect("health worker");
        drop(client);
        match Arc::try_unwrap(database) {
            Ok(database) => database.shutdown().await.expect("shutdown database"),
            Err(database) => {
                drop(database);
                panic!("database released");
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn unavailable_network_health_rechecks_after_thirty_seconds() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let database = Arc::new(
            DbActorHandle::open(&directory.path().join("agent.sqlite3"), "test")
                .await
                .expect("open database"),
        );
        let credential = DeviceCredential::new(
            "01985555-7555-8555-8555-555555555555".to_owned(),
            "01982222-7222-8222-8222-222222222222".to_owned(),
            "access-credential-for-network-health-test",
            "refresh-credential-for-network-health-test",
        )
        .expect("valid credential");
        let (_credential, credential_receiver) = watch::channel(credential);
        let applied_control = Some(AppliedControl {
            configuration_revision: 1,
            network_enabled: true,
            communication_wechat_enabled: false,
            communication_messages_enabled: false,
            photos_library_enabled: false,
            screen_capture: ScreenCaptureControl::default(),
            screenshot_request_id: None,
        });
        let (control, control_receiver) = watch::channel(applied_control.clone());
        let (shutdown, shutdown_receiver) = watch::channel(false);
        let (report_updates, mut report_receiver) = watch::channel(0);
        let client = Arc::new(NetworkCollectorHealthClient {
            reports: AtomicUsize::new(0),
            report_updates,
            available: AtomicBool::new(false),
        });
        let worker = tokio::spawn(run_collector_health_loop(
            Arc::clone(&database),
            credential_receiver,
            control_receiver,
            Arc::clone(&client) as Arc<dyn ControlClient>,
            shutdown_receiver,
        ));

        control.send_replace(applied_control);
        report_receiver
            .changed()
            .await
            .expect("first health report");
        assert_eq!(client.reports.load(Ordering::SeqCst), 1);
        client.available.store(true, Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(29)).await;
        tokio::task::yield_now().await;
        assert_eq!(client.reports.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(1)).await;
        report_receiver
            .changed()
            .await
            .expect("second health report");
        assert_eq!(client.reports.load(Ordering::SeqCst), 2);

        shutdown.send_replace(true);
        worker
            .await
            .expect("join health worker")
            .expect("health worker");
        drop(client);
        Arc::try_unwrap(database)
            .unwrap_or_else(|database| {
                drop(database);
                panic!("database released");
            })
            .shutdown()
            .await
            .expect("shutdown database");
    }

    #[test]
    fn pairing_debug_output_redacts_one_time_secrets() {
        let callback = PairingCallbackHandoff {
            session_id: "session".to_owned(),
            authorization_code: "authorization-must-not-appear".to_owned(),
        };
        let exchange = PairingExchangeRequest {
            session_id: "session".to_owned(),
            authorization_code: "authorization-must-not-appear".to_owned(),
            code_verifier: "verifier-must-not-appear".to_owned(),
        };
        let debug = format!("{callback:?} {exchange:?}");
        assert!(!debug.contains("authorization-must-not-appear"));
        assert!(!debug.contains("verifier-must-not-appear"));
        assert!(debug.contains("redacted"));
    }

    struct ProxyEnvironment {
        values: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl ProxyEnvironment {
        fn replace_with(proxy: &str) -> Self {
            let names = [
                "HTTPS_PROXY",
                "https_proxy",
                "HTTP_PROXY",
                "http_proxy",
                "ALL_PROXY",
                "all_proxy",
                "NO_PROXY",
                "no_proxy",
            ];
            let values = names
                .into_iter()
                .map(|name| (name, env::var_os(name)))
                .collect();
            for name in names {
                env::remove_var(name);
            }
            env::set_var("HTTPS_PROXY", proxy);
            Self { values }
        }

        fn set_proxy(&self, proxy: &str) {
            for (name, _) in &self.values {
                env::remove_var(name);
            }
            env::set_var("HTTPS_PROXY", proxy);
        }
    }

    impl Drop for ProxyEnvironment {
        fn drop(&mut self) {
            for (name, value) in &self.values {
                match value {
                    Some(value) => env::set_var(name, value),
                    None => env::remove_var(name),
                }
            }
        }
    }

    async fn failing_proxy() -> (String, oneshot::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test proxy");
        let proxy = format!(
            "http://{}",
            listener.local_addr().expect("test proxy address")
        );
        let (request_sender, request_receiver) = oneshot::channel();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept proxy request");
            let mut bytes = Vec::new();
            loop {
                let mut chunk = [0_u8; 512];
                let read = stream.read(&mut chunk).await.expect("read proxy request");
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..read]);
                if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let _ = request_sender.send(String::from_utf8(bytes).expect("UTF-8 proxy request"));
            let _ = stream
                .write_all(
                    b"HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await;
        });
        (proxy, request_receiver)
    }

    #[test]
    fn retry_backoff_is_jittered_and_bounded() {
        assert_ne!(retry_delay(1), Duration::from_secs(1));
        assert!(retry_delay(20) <= MAX_BACKOFF);
        assert!(retry_delay(20) <= CONTROL_INTERVAL);
        assert_eq!(CONTROL_INTERVAL, Duration::from_secs(30));
    }

    #[test]
    fn completed_media_batches_continue_without_the_control_interval_delay() {
        assert_eq!(CONTROL_REQUEST_TIMEOUT, Duration::from_secs(15));
        assert_eq!(MEDIA_UPLOAD_TIMEOUT, Duration::from_mins(5));
        assert_eq!(photo_upload_timeout(0), MEDIA_UPLOAD_TIMEOUT);
        assert_eq!(
            photo_upload_timeout(500 * 1024 * 1024),
            Duration::from_secs(1_120)
        );
        assert!(photo_upload_timeout(464_811_997) > MEDIA_UPLOAD_TIMEOUT);
        assert!(photo_upload_timeout(u64::MAX) <= PHOTO_UPLOAD_MAX_TIMEOUT);
        assert!(PHOTO_UPLOAD_MAX_TIMEOUT.saturating_mul(2) < MEDIA_CYCLE_TIMEOUT);
        assert_eq!(
            next_media_wait(usize::from(MEDIA_BATCH_SIZE)),
            Duration::ZERO
        );
        assert_eq!(
            next_media_wait(usize::from(MEDIA_BATCH_SIZE - 1)),
            CONTROL_INTERVAL
        );
    }

    #[test]
    fn only_a_missing_prepare_projection_marks_old_media_superseded() {
        assert!(attachment_was_superseded(MediaUploadFailure::superseded()));
        assert!(!attachment_was_superseded(MediaUploadFailure::new(
            MediaUploadFailureStage::Prepare,
            ControlError::Contract,
        )));
        assert!(!attachment_was_superseded(MediaUploadFailure::new(
            MediaUploadFailureStage::Complete,
            ControlError::Contract,
        )));
    }

    #[test]
    fn only_the_dedicated_attachment_not_found_error_supersedes_media() {
        let missing = br#"{"error":{"error_code":"COMMUNICATION_ATTACHMENT_NOT_FOUND","message":"missing","retryable":false}}"#;
        let invalid_credential = br#"{"error":{"error_code":"CREDENTIAL_INVALID","message":"invalid","retryable":false}}"#;

        assert!(communication_attachment_is_missing(
            reqwest::StatusCode::NOT_FOUND,
            missing,
        ));
        assert!(!communication_attachment_is_missing(
            reqwest::StatusCode::NOT_FOUND,
            invalid_credential,
        ));
        assert!(!communication_attachment_is_missing(
            reqwest::StatusCode::UNAUTHORIZED,
            missing,
        ));
    }

    #[tokio::test]
    async fn uploaded_photo_cleanup_accepts_an_already_missing_file() {
        let directory = tempfile::tempdir().expect("temporary photo spool");
        let path = directory.path().join("already-removed-photo");

        remove_uploaded_media_file(&path)
            .await
            .expect("cloud-completed photo can recover after a crash following local removal");
    }

    #[tokio::test]
    async fn uploaded_photo_cleanup_removes_an_existing_file() {
        let directory = tempfile::tempdir().expect("temporary photo spool");
        let path = directory.path().join("uploaded-photo");
        tokio::fs::write(&path, b"photo")
            .await
            .expect("write photo spool file");

        remove_uploaded_media_file(&path)
            .await
            .expect("remove cloud-completed photo");

        assert!(!path.exists());
    }

    #[tokio::test]
    async fn corrupt_manifest_is_quarantined_out_of_the_upload_queue() {
        let directory = tempfile::tempdir().expect("temporary spool");
        let path = directory.path().join("broken.json");
        tokio::fs::write(&path, b"not-json")
            .await
            .expect("write corrupt manifest");

        quarantine_manifest(&path)
            .await
            .expect("quarantine corrupt manifest");

        assert!(!path.exists());
        let entries = std::fs::read_dir(directory.path())
            .expect("read quarantine directory")
            .collect::<Result<Vec<_>, _>>()
            .expect("read quarantine entries");
        assert_eq!(entries.len(), 1);
        assert!(entries[0]
            .file_name()
            .to_string_lossy()
            .starts_with("broken.invalid-"));
    }

    #[tokio::test]
    async fn corrupt_screenshot_manifest_is_diagnostic_and_stays_degraded() {
        let directory = tempfile::tempdir().expect("temporary spool");
        let database = DbActorHandle::open(&directory.path().join("agent.sqlite3"), "test")
            .await
            .expect("open database");
        database
            .upsert_collector_state(&CollectorState {
                collector_key: "screen.capture".to_owned(),
                collector_version: "test".to_owned(),
                status: CollectorStatus::Running,
                desired_config_revision: 1,
                applied_config_revision: 1,
                last_event_at_ms: Some(1),
                last_health_at_ms: Some(1),
                last_error_code: None,
                created_at_ms: 1,
                updated_at_ms: 1,
            })
            .await
            .expect("seed Screenshot Collector state");
        let path = directory.path().join("broken.json");
        tokio::fs::write(&path, b"not-json")
            .await
            .expect("write corrupt manifest");

        quarantine_invalid_screenshot_manifest(&database, &path)
            .await
            .expect("quarantine corrupt Screenshot manifest");
        persist_aux_collector_state(&database, "screen.capture", true, 1, false, None)
            .await
            .expect("persist later healthy Screenshot observation");

        let state = database
            .load_collector_states()
            .await
            .expect("load Screenshot Collector state")
            .into_iter()
            .find(|state| state.collector_key == "screen.capture")
            .expect("Screenshot Collector state");
        assert_eq!(state.status, CollectorStatus::Degraded);
        assert_eq!(
            state.last_error_code.as_deref(),
            Some(SCREEN_LOCAL_MANIFEST_INVALID)
        );
        let diagnostics = rusqlite::Connection::open(directory.path().join("agent.sqlite3"))
            .expect("open diagnostic database")
            .query_row(
                "SELECT COUNT(*) FROM diagnostic_events
                 WHERE code = 'SCREEN_LOCAL_MANIFEST_INVALID'",
                [],
                |row| row.get::<_, u64>(0),
            )
            .expect("count Screenshot diagnostics");
        assert_eq!(diagnostics, 1);
        database.shutdown().await.expect("shutdown database");
    }

    #[test]
    fn photo_completion_markers_live_outside_the_hot_upload_directory() {
        let root = std::path::Path::new("/private/photo-spool");
        let photo_id = "01985555-7555-8555-8555-555555555555";

        assert_eq!(
            photo_marker_path(root, photo_id, PhotoMarker::Completed),
            root.join("Handled").join(format!("{photo_id}.completed"))
        );
        assert_eq!(
            photo_marker_path(root, photo_id, PhotoMarker::Oversized),
            root.join("Handled").join(format!("{photo_id}.oversized"))
        );
    }

    #[tokio::test]
    async fn screenshot_upload_batch_drains_an_explicit_spool_independently() {
        let directory = tempfile::tempdir().expect("temporary screenshot spool");
        let database = DbActorHandle::open(&directory.path().join("agent.sqlite3"), "test")
            .await
            .expect("open database");
        let spool = directory.path().join("screenshots");
        tokio::fs::create_dir(&spool)
            .await
            .expect("create screenshot spool");
        let screenshot_id = "01985555-7555-8555-8555-555555555555";
        let image_file_name = format!("{screenshot_id}.jpg");
        let image = b"independent screenshot upload";
        tokio::fs::write(spool.join(&image_file_name), image)
            .await
            .expect("write screenshot image");
        let screenshot = PendingScreenshot {
            screenshot_id: screenshot_id.to_owned(),
            request_id: None,
            trigger: ScreenshotTrigger::Scheduled,
            captured_at: "2026-08-16T00:00:00Z".to_owned(),
            app_bundle_id: Some("com.example.App".to_owned()),
            pixel_width: 1920,
            pixel_height: 1080,
            sha256: format!("{:x}", Sha256::digest(image)),
            size_bytes: u64::try_from(image.len()).expect("screenshot size"),
            mime_type: "image/jpeg".to_owned(),
            image_file_name: image_file_name.clone(),
        };
        tokio::fs::write(
            spool.join(format!("{screenshot_id}.json")),
            serde_json::to_vec(&screenshot).expect("serialize screenshot manifest"),
        )
        .await
        .expect("write screenshot manifest");
        let credential = DeviceCredential::new(
            "01985555-7555-8555-8555-555555555556".to_owned(),
            "01982222-7222-8222-8222-222222222222".to_owned(),
            "access-credential-for-screenshot-test",
            "refresh-credential-for-screenshot-test",
        )
        .expect("valid credential");
        let client = AcceptingScreenshotClient(AtomicUsize::new(0));
        let outcome = upload_pending_screenshots(&database, &client, &credential, &spool)
            .await
            .expect("upload screenshot batch");

        assert_eq!(outcome.completed, 1);
        assert_eq!(client.0.load(Ordering::SeqCst), 1);
        assert!(!spool.join(image_file_name).exists());
        assert!(!spool.join(format!("{screenshot_id}.json")).exists());
        database.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn restored_manual_screenshot_is_remembered_before_upload() {
        let directory = tempfile::tempdir().expect("temporary database");
        let database = DbActorHandle::open(&directory.path().join("agent.sqlite3"), "test")
            .await
            .expect("open database");
        let spool = directory.path().join("screenshots");
        tokio::fs::create_dir(&spool)
            .await
            .expect("create screenshot spool");
        let request_id = "01984444-7444-8444-8444-444444444444".to_owned();
        let screenshot = PendingScreenshot {
            screenshot_id: "01985555-7555-8555-8555-555555555555".to_owned(),
            request_id: Some(request_id.clone()),
            trigger: ScreenshotTrigger::Manual,
            captured_at: "2026-08-05T00:00:00Z".to_owned(),
            app_bundle_id: Some("com.example.App".to_owned()),
            pixel_width: 1920,
            pixel_height: 1080,
            sha256: "a".repeat(64),
            size_bytes: 1,
            mime_type: "image/jpeg".to_owned(),
            image_file_name: "capture.jpg".to_owned(),
        };
        tokio::fs::write(
            spool.join(format!("{}.json", screenshot.screenshot_id)),
            serde_json::to_vec(&screenshot).expect("serialize screenshot manifest"),
        )
        .await
        .expect("write screenshot manifest");

        restore_screenshot_request_history(&database, &spool)
            .await
            .expect("remember restored request");

        assert!(database
            .screenshot_request_was_handled(&request_id)
            .await
            .expect("read restored request"));
        database.shutdown().await.expect("shutdown database");
    }

    #[test]
    fn screenshot_prepare_payload_excludes_the_local_spool_file_name() {
        let screenshot = PendingScreenshot {
            screenshot_id: "01985555-7555-8555-8555-555555555555".to_owned(),
            request_id: None,
            trigger: ScreenshotTrigger::Activity,
            captured_at: "2026-08-05T00:00:00Z".to_owned(),
            app_bundle_id: Some("com.example.App".to_owned()),
            pixel_width: 1920,
            pixel_height: 1080,
            sha256: "a".repeat(64),
            size_bytes: 1,
            mime_type: "image/jpeg".to_owned(),
            image_file_name: "local-only.jpg".to_owned(),
        };

        let payload = screenshot_prepare_payload(&screenshot);

        assert_eq!(payload.as_object().expect("object").len(), 10);
        assert!(payload.get("image_file_name").is_none());
        assert_eq!(payload["trigger"], "activity");
    }

    #[test]
    fn communication_upload_sends_one_validated_content_length_header() {
        let headers = std::collections::BTreeMap::from([
            ("content-length".to_owned(), "7".to_owned()),
            ("content-type".to_owned(), "image/jpeg".to_owned()),
        ]);
        let request = apply_communication_upload_headers(
            reqwest::Client::new().put("https://example.test/upload"),
            &headers,
            7,
        )
        .expect("valid signed upload headers")
        .build()
        .expect("build upload request");

        assert_eq!(
            request
                .headers()
                .get_all(reqwest::header::CONTENT_LENGTH)
                .iter()
                .count(),
            1
        );
        assert_eq!(request.headers()[reqwest::header::CONTENT_LENGTH], "7");
    }

    #[test]
    fn production_cloud_origin_rejects_paths_queries_and_fragments() {
        for value in [
            "https://pca-cloud-api-production.up.railway.app/internal",
            "https://pca-cloud-api-production.up.railway.app/?redirect=other",
            "https://pca-cloud-api-production.up.railway.app/#fragment",
        ] {
            assert!(matches!(
                HttpControlClient::new(Url::parse(value).expect("valid URL")),
                Err(ControlError::Contract)
            ));
        }
    }

    #[tokio::test]
    async fn control_client_uses_the_current_system_proxy_after_a_proxy_switch() {
        let _lock = PROXY_ENVIRONMENT_LOCK.lock().await;
        let (first_proxy, first_request) = failing_proxy().await;
        let environment = ProxyEnvironment::replace_with(&first_proxy);
        let client = HttpControlClient::new(
            Url::parse(PRODUCTION_CLOUD_API_ORIGIN).expect("production Cloud origin"),
        )
        .expect("production Cloud client");
        let credential = DeviceCredential::new(
            "01983333-7333-8333-8333-333333333333".to_owned(),
            "01982222-7222-8222-8222-222222222222".to_owned(),
            "access-credential-for-proxy-test",
            "refresh-credential-for-proxy-test",
        )
        .expect("valid device credential");

        assert!(matches!(
            client.heartbeat_and_control(&credential, 0).await,
            Err(ControlError::Transient)
        ));
        assert!(timeout(Duration::from_secs(2), first_request)
            .await
            .expect("first proxy receives a request")
            .expect("first proxy request channel")
            .starts_with("CONNECT pca-cloud-api-production.up.railway.app:443"));

        let (second_proxy, second_request) = failing_proxy().await;
        environment.set_proxy(&second_proxy);
        assert!(matches!(
            client.heartbeat_and_control(&credential, 0).await,
            Err(ControlError::Transient)
        ));
        assert!(timeout(Duration::from_secs(2), second_request)
            .await
            .expect("new proxy receives the next request")
            .expect("new proxy request channel")
            .starts_with("CONNECT pca-cloud-api-production.up.railway.app:443"));
    }

    struct AcceptingSyncClient;

    struct AcceptingScreenshotClient(AtomicUsize);

    struct RejectingSystemSyncClient;

    struct RetryableRejectingSystemSyncClient;

    struct MixedSystemSyncClient {
        rejected_event_id: String,
    }

    impl ControlClient for AcceptingSyncClient {
        fn refresh<'a>(&'a self, _: &'a DeviceCredential) -> ControlFuture<'a, DeviceCredential> {
            Box::pin(async { Err(ControlError::Contract) })
        }

        fn heartbeat_and_control<'a>(
            &'a self,
            _: &'a DeviceCredential,
            _: u64,
        ) -> ControlFuture<'a, AgentControlSnapshot> {
            Box::pin(async { Err(ControlError::Contract) })
        }

        fn sync_system_events<'a>(
            &'a self,
            _: &'a DeviceCredential,
            events: &'a [EventEnvelope],
        ) -> ControlFuture<'a, SyncEventsResponse> {
            let accepted = events.iter().map(|event| event.event_id.clone()).collect();
            Box::pin(async move {
                Ok(SyncEventsResponse {
                    batch_id: "test-batch".to_owned(),
                    accepted,
                    duplicates: Vec::new(),
                    rejected: Vec::new(),
                })
            })
        }
    }

    impl ControlClient for AcceptingScreenshotClient {
        fn refresh<'a>(&'a self, _: &'a DeviceCredential) -> ControlFuture<'a, DeviceCredential> {
            Box::pin(async { Err(ControlError::Contract) })
        }

        fn heartbeat_and_control<'a>(
            &'a self,
            _: &'a DeviceCredential,
            _: u64,
        ) -> ControlFuture<'a, AgentControlSnapshot> {
            Box::pin(async { Err(ControlError::Contract) })
        }

        fn sync_screenshot<'a>(
            &'a self,
            _: &'a DeviceCredential,
            _: &'a PendingScreenshot,
        ) -> ControlFuture<'a, ()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    }

    impl ControlClient for RejectingSystemSyncClient {
        fn refresh<'a>(&'a self, _: &'a DeviceCredential) -> ControlFuture<'a, DeviceCredential> {
            Box::pin(async { Err(ControlError::Contract) })
        }

        fn heartbeat_and_control<'a>(
            &'a self,
            _: &'a DeviceCredential,
            _: u64,
        ) -> ControlFuture<'a, AgentControlSnapshot> {
            Box::pin(async { Err(ControlError::Contract) })
        }

        fn sync_system_events<'a>(
            &'a self,
            _: &'a DeviceCredential,
            events: &'a [EventEnvelope],
        ) -> ControlFuture<'a, SyncEventsResponse> {
            let rejected = events
                .iter()
                .map(|event| SyncEventRejection {
                    event_id: event.event_id.clone(),
                    error_code: SYNC_PAYLOAD_REJECTED.to_owned(),
                    retryable: false,
                })
                .collect();
            Box::pin(async move {
                Ok(SyncEventsResponse {
                    batch_id: "test-batch".to_owned(),
                    accepted: Vec::new(),
                    duplicates: Vec::new(),
                    rejected,
                })
            })
        }
    }

    impl ControlClient for RetryableRejectingSystemSyncClient {
        fn refresh<'a>(&'a self, _: &'a DeviceCredential) -> ControlFuture<'a, DeviceCredential> {
            Box::pin(async { Err(ControlError::Contract) })
        }

        fn heartbeat_and_control<'a>(
            &'a self,
            _: &'a DeviceCredential,
            _: u64,
        ) -> ControlFuture<'a, AgentControlSnapshot> {
            Box::pin(async { Err(ControlError::Contract) })
        }

        fn sync_system_events<'a>(
            &'a self,
            _: &'a DeviceCredential,
            events: &'a [EventEnvelope],
        ) -> ControlFuture<'a, SyncEventsResponse> {
            let rejected = events
                .iter()
                .map(|event| SyncEventRejection {
                    event_id: event.event_id.clone(),
                    error_code: "SYNC_RATE_LIMITED".to_owned(),
                    retryable: true,
                })
                .collect();
            Box::pin(async move {
                Ok(SyncEventsResponse {
                    batch_id: "test-batch".to_owned(),
                    accepted: Vec::new(),
                    duplicates: Vec::new(),
                    rejected,
                })
            })
        }
    }

    impl ControlClient for MixedSystemSyncClient {
        fn refresh<'a>(&'a self, _: &'a DeviceCredential) -> ControlFuture<'a, DeviceCredential> {
            Box::pin(async { Err(ControlError::Contract) })
        }

        fn heartbeat_and_control<'a>(
            &'a self,
            _: &'a DeviceCredential,
            _: u64,
        ) -> ControlFuture<'a, AgentControlSnapshot> {
            Box::pin(async { Err(ControlError::Contract) })
        }

        fn sync_system_events<'a>(
            &'a self,
            _: &'a DeviceCredential,
            events: &'a [EventEnvelope],
        ) -> ControlFuture<'a, SyncEventsResponse> {
            let mut accepted = Vec::new();
            let mut rejected = Vec::new();
            for event in events {
                if event.event_id == self.rejected_event_id {
                    rejected.push(SyncEventRejection {
                        event_id: event.event_id.clone(),
                        error_code: SYNC_PAYLOAD_REJECTED.to_owned(),
                        retryable: false,
                    });
                } else {
                    accepted.push(event.event_id.clone());
                }
            }
            Box::pin(async move {
                Ok(SyncEventsResponse {
                    batch_id: "test-batch".to_owned(),
                    accepted,
                    duplicates: Vec::new(),
                    rejected,
                })
            })
        }
    }

    struct RejectingCommunicationSyncClient;

    impl ControlClient for RejectingCommunicationSyncClient {
        fn refresh<'a>(&'a self, _: &'a DeviceCredential) -> ControlFuture<'a, DeviceCredential> {
            Box::pin(async { Err(ControlError::Contract) })
        }

        fn heartbeat_and_control<'a>(
            &'a self,
            _: &'a DeviceCredential,
            _: u64,
        ) -> ControlFuture<'a, AgentControlSnapshot> {
            Box::pin(async { Err(ControlError::Contract) })
        }

        fn sync_communication_events<'a>(
            &'a self,
            _: &'a DeviceCredential,
            events: &'a [EventEnvelope],
        ) -> ControlFuture<'a, SyncEventsResponse> {
            let rejected = events
                .iter()
                .map(|event| SyncEventRejection {
                    event_id: event.event_id.clone(),
                    error_code: SYNC_PAYLOAD_REJECTED.to_owned(),
                    retryable: false,
                })
                .collect();
            Box::pin(async move {
                Ok(SyncEventsResponse {
                    batch_id: "test-batch".to_owned(),
                    accepted: Vec::new(),
                    duplicates: Vec::new(),
                    rejected,
                })
            })
        }
    }

    #[tokio::test]
    async fn rejected_communication_event_is_quarantined_without_retry() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let database = DbActorHandle::open(&directory.path().join("agent.sqlite3"), "test")
            .await
            .expect("open database");
        let message = CommunicationMessageRecorded::try_new(CommunicationMessageRecordedInput {
            message_id: "message-1".to_owned(),
            conversation_id: "conversation-1".to_owned(),
            sender_id: "wxid_sender".to_owned(),
            sender_display_name: "Sender".to_owned(),
            source_key: "source-key-1".to_owned(),
            occurred_at: "2026-08-02T00:00:00Z".to_owned(),
            direction: Direction::Incoming,
            kind: MessageKind::Text,
            conversation: ConversationScope::Direct,
            text: Some("private body".to_owned()),
            attachments: Vec::new(),
        })
        .expect("valid communication message");
        let payload = serde_json::to_value(&message)
            .expect("serialize message")
            .as_object()
            .cloned()
            .expect("message payload object");
        database
            .commit_communication_message(&CommunicationMessageCommit {
                account_id: "wechat-account-1".to_owned(),
                source_sequence: 1,
                cursor_sequence: 1,
                event: EventEnvelope {
                    event_id: "01986666-7666-8666-8666-666666666668".to_owned(),
                    workspace_id: "01982222-7222-8222-8222-222222222222".to_owned(),
                    device_id: "01983333-7333-8333-8333-333333333333".to_owned(),
                    event_type: "communication.message_recorded".to_owned(),
                    source: "communication.wechat".to_owned(),
                    schema_version: 1,
                    occurred_at: "2026-08-02T00:00:00Z".to_owned(),
                    created_at: "2026-08-02T00:00:00Z".to_owned(),
                    sensitivity: Sensitivity::High,
                    payload,
                    attachment_refs: Vec::new(),
                    idempotency_key: Some("source-key-1".to_owned()),
                },
                metadata_events: Vec::new(),
                message,
                attachment_spool: Vec::new(),
            })
            .await
            .expect("commit communication message");
        let credential = DeviceCredential::new(
            "01983333-7333-8333-8333-333333333333".to_owned(),
            "01982222-7222-8222-8222-222222222222".to_owned(),
            "access-credential-for-sync-test",
            "refresh-credential-for-sync-test",
        )
        .expect("valid device credential");

        sync_pending_communication_events(
            &database,
            &credential,
            &RejectingCommunicationSyncClient,
        )
        .await
        .expect("quarantine rejected communication event");
        assert!(database
            .load_pending_communication_events(200)
            .await
            .expect("pending communication events")
            .is_empty());
        assert_eq!(
            database.active_outbox_depth().await.expect("outbox depth"),
            0
        );
        database.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn accepted_system_and_legacy_lifecycle_events_are_acknowledged() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let database = DbActorHandle::open(&directory.path().join("agent.sqlite3"), "test")
            .await
            .expect("open database");
        let system_event = system_metric_event("01984444-7444-8444-8444-444444444444");
        let lifecycle_event = EventEnvelope {
            event_id: "01985555-7555-8555-8555-555555555555".to_owned(),
            workspace_id: system_event.workspace_id.clone(),
            device_id: system_event.device_id.clone(),
            event_type: "AGENT_STARTED".to_owned(),
            source: "runtime".to_owned(),
            schema_version: 1,
            occurred_at: system_event.occurred_at.clone(),
            created_at: system_event.created_at.clone(),
            sensitivity: Sensitivity::Normal,
            payload: Map::new(),
            attachment_refs: Vec::new(),
            idempotency_key: None,
        };
        database
            .append_event_with_outbox(&system_event)
            .await
            .expect("persist system event");
        database
            .append_event_with_outbox(&lifecycle_event)
            .await
            .expect("persist lifecycle event");
        let credential = DeviceCredential::new(
            system_event.device_id.clone(),
            system_event.workspace_id.clone(),
            "access-credential-for-sync-test",
            "refresh-credential-for-sync-test",
        )
        .expect("valid device credential");

        sync_pending_system_events(&database, &credential, &AcceptingSyncClient)
            .await
            .expect("sync accepted system event");

        assert!(database
            .load_pending_system_events(20)
            .await
            .expect("load pending system events")
            .is_empty());
        assert_eq!(
            database.active_outbox_depth().await.expect("outbox depth"),
            0
        );
        database.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn retryable_cloud_rejection_remains_pending_without_dead_letter() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let database = DbActorHandle::open(&directory.path().join("agent.sqlite3"), "test")
            .await
            .expect("open database");
        let event = system_metric_event("01986666-7666-8666-8666-66666666666d");
        database
            .append_event_with_outbox(&event)
            .await
            .expect("persist system event");
        let credential = DeviceCredential::new(
            event.device_id.clone(),
            event.workspace_id.clone(),
            "access-credential-for-sync-test",
            "refresh-credential-for-sync-test",
        )
        .expect("valid device credential");

        let error =
            sync_pending_system_events(&database, &credential, &RetryableRejectingSystemSyncClient)
                .await
                .expect_err("retryable rejection must not become terminal");

        assert_eq!(error, ControlError::Transient);
        assert_eq!(
            database
                .load_pending_system_events(20)
                .await
                .expect("load pending system events")
                .len(),
            1
        );
        assert_eq!(
            database.active_outbox_depth().await.expect("outbox depth"),
            1
        );
        database.shutdown().await.expect("shutdown database");
    }

    #[test]
    fn unknown_nonretryable_rejection_cannot_wedge_the_outbox() {
        let response = SyncEventsResponse {
            batch_id: "test-batch".to_owned(),
            accepted: Vec::new(),
            duplicates: Vec::new(),
            rejected: vec![SyncEventRejection {
                event_id: "01986666-7666-8666-8666-66666666666f".to_owned(),
                error_code: "SYNC_PROJECTION_CONFLICT".to_owned(),
                retryable: false,
            }],
        };

        assert_eq!(
            terminal_rejected_event_ids(&response)
                .expect("nonretryable rejection is terminal")
                .into_iter()
                .collect::<Vec<_>>(),
            vec!["01986666-7666-8666-8666-66666666666f"]
        );
    }

    #[tokio::test]
    async fn mixed_cloud_response_acknowledges_valid_event_and_unblocks_the_next_batch() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let database = DbActorHandle::open(&directory.path().join("agent.sqlite3"), "test")
            .await
            .expect("open database");
        let rejected = system_metric_event("01986666-7666-8666-8666-66666666666e");
        let accepted = system_metric_event("01986666-7666-8666-8666-66666666666f");
        for event in [&rejected, &accepted] {
            database
                .append_event_with_outbox(event)
                .await
                .expect("persist system event");
        }
        let credential = DeviceCredential::new(
            rejected.device_id.clone(),
            rejected.workspace_id.clone(),
            "access-credential-for-sync-test",
            "refresh-credential-for-sync-test",
        )
        .expect("valid device credential");
        let client = MixedSystemSyncClient {
            rejected_event_id: rejected.event_id.clone(),
        };

        sync_pending_system_events(&database, &credential, &client)
            .await
            .expect("apply mixed sync response");
        assert!(database
            .load_pending_system_events(20)
            .await
            .expect("load pending system events")
            .is_empty());

        let later = system_metric_event("01986666-7666-8666-8666-666666666670");
        database
            .append_event_with_outbox(&later)
            .await
            .expect("persist later event");
        sync_pending_system_events(&database, &credential, &client)
            .await
            .expect("sync later event");
        assert!(database
            .load_pending_system_events(20)
            .await
            .expect("load later pending events")
            .is_empty());
        assert_eq!(
            database.active_outbox_depth().await.expect("outbox depth"),
            0
        );
        database.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "the regression verifies rejection, sync fencing, and recovery in one lifecycle"
    )]
    async fn cloud_rejected_system_event_is_quarantined_without_outbox_backpressure() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let database = DbActorHandle::open(&directory.path().join("agent.sqlite3"), "test")
            .await
            .expect("open database");
        let event = system_metric_event("01986666-7666-8666-8666-666666666669");
        database
            .append_event_with_outbox(&event)
            .await
            .expect("persist rejected system event");
        database
            .upsert_collector_state(&CollectorState {
                collector_key: "system".to_owned(),
                collector_version: "test".to_owned(),
                status: CollectorStatus::Running,
                desired_config_revision: 1,
                applied_config_revision: 1,
                last_event_at_ms: Some(1),
                last_health_at_ms: Some(1),
                last_error_code: None,
                created_at_ms: 1,
                updated_at_ms: 1,
            })
            .await
            .expect("seed collector health");
        let credential = DeviceCredential::new(
            event.device_id.clone(),
            event.workspace_id.clone(),
            "access-credential-for-sync-test",
            "refresh-credential-for-sync-test",
        )
        .expect("valid device credential");

        sync_pending_system_events(&database, &credential, &RejectingSystemSyncClient)
            .await
            .expect("quarantine rejected event");

        assert!(database
            .load_pending_system_events(20)
            .await
            .expect("load pending system events")
            .is_empty());
        assert_eq!(
            database.active_outbox_depth().await.expect("outbox depth"),
            0
        );
        let state = database
            .load_collector_states()
            .await
            .expect("load collector health")
            .into_iter()
            .find(|state| state.collector_key == "system")
            .expect("system collector health");
        assert_eq!(state.status, CollectorStatus::Degraded);
        assert_eq!(
            state.last_error_code.as_deref(),
            Some("SYNC_PAYLOAD_REJECTED")
        );

        let healthy_observation = CollectorState {
            collector_key: "system".to_owned(),
            collector_version: "test".to_owned(),
            status: CollectorStatus::Running,
            desired_config_revision: 1,
            applied_config_revision: 1,
            last_event_at_ms: Some(2),
            last_health_at_ms: Some(2),
            last_error_code: None,
            created_at_ms: 1,
            updated_at_ms: 2,
        };
        database
            .upsert_collector_state(&healthy_observation)
            .await
            .expect("persist local healthy observation");
        let fenced = database
            .load_collector_states()
            .await
            .expect("load fenced collector health")
            .into_iter()
            .find(|state| state.collector_key == "system")
            .expect("system collector health");
        assert_eq!(fenced.status, CollectorStatus::Degraded);
        assert_eq!(
            fenced.last_error_code.as_deref(),
            Some("SYNC_PAYLOAD_REJECTED")
        );

        let accepted = system_metric_event("01986666-7666-8666-8666-666666666670");
        database
            .append_event_with_outbox(&accepted)
            .await
            .expect("persist recovery event");
        sync_pending_system_events(&database, &credential, &AcceptingSyncClient)
            .await
            .expect("accept recovery event");
        database
            .upsert_collector_state(&healthy_observation)
            .await
            .expect("persist recovered collector health");
        let recovered = database
            .load_collector_states()
            .await
            .expect("load recovered collector health")
            .into_iter()
            .find(|state| state.collector_key == "system")
            .expect("system collector health");
        assert_eq!(recovered.status, CollectorStatus::Running);
        assert_eq!(recovered.last_error_code, None);
        database.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn malformed_sync_response_does_not_acknowledge_the_local_event() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let database = DbActorHandle::open(&directory.path().join("agent.sqlite3"), "test")
            .await
            .expect("open database");
        let event = system_metric_event("01986666-7666-8666-8666-666666666667");
        database
            .append_event_with_outbox(&event)
            .await
            .expect("persist system event");
        let credential = DeviceCredential::new(
            event.device_id.clone(),
            event.workspace_id.clone(),
            "access-credential-for-sync-test",
            "refresh-credential-for-sync-test",
        )
        .expect("valid device credential");

        assert!(matches!(
            sync_pending_system_events(&database, &credential, &DuplicatingSyncClient).await,
            Err(ControlError::Transient)
        ));
        assert_eq!(
            database
                .load_pending_system_events(20)
                .await
                .expect("load pending system events")
                .len(),
            1
        );
        database.shutdown().await.expect("shutdown database");
    }

    fn system_metric_event(event_id: &str) -> EventEnvelope {
        let mut payload = Map::new();
        payload.insert(
            "metric_group".to_owned(),
            Value::String("cpu_memory".to_owned()),
        );
        payload.insert("sample_window_ms".to_owned(), Value::from(30_000));
        payload.insert("logical_cpu_count".to_owned(), Value::from(10));
        payload.insert(
            "host".to_owned(),
            serde_json::json!({
                "cpu_usage_percent": 12.34,
                "memory_total_bytes": 34_359_738_368_u64,
                "memory_used_bytes": 17_179_869_184_u64,
            }),
        );
        payload.insert(
            "agent".to_owned(),
            serde_json::json!({ "cpu_usage_percent": 0.42, "memory_resident_bytes": 73_400_320_u64 }),
        );
        EventEnvelope {
            event_id: event_id.to_owned(),
            workspace_id: "01983333-7333-8333-8333-333333333333".to_owned(),
            device_id: "01982222-7222-8222-8222-222222222222".to_owned(),
            event_type: "system.metric_sampled".to_owned(),
            source: "system".to_owned(),
            schema_version: 1,
            occurred_at: "2026-08-02T00:00:00Z".to_owned(),
            created_at: "2026-08-02T00:00:00Z".to_owned(),
            sensitivity: Sensitivity::Normal,
            payload,
            attachment_refs: Vec::new(),
            idempotency_key: Some(format!("system:{event_id}")),
        }
    }

    struct DuplicatingSyncClient;

    impl ControlClient for DuplicatingSyncClient {
        fn refresh<'a>(&'a self, _: &'a DeviceCredential) -> ControlFuture<'a, DeviceCredential> {
            Box::pin(async { Err(ControlError::Contract) })
        }

        fn heartbeat_and_control<'a>(
            &'a self,
            _: &'a DeviceCredential,
            _: u64,
        ) -> ControlFuture<'a, AgentControlSnapshot> {
            Box::pin(async { Err(ControlError::Contract) })
        }

        fn sync_system_events<'a>(
            &'a self,
            _: &'a DeviceCredential,
            events: &'a [EventEnvelope],
        ) -> ControlFuture<'a, SyncEventsResponse> {
            let event_id = events[0].event_id.clone();
            Box::pin(async move {
                Ok(SyncEventsResponse {
                    batch_id: "test-batch".to_owned(),
                    accepted: vec![event_id.clone()],
                    duplicates: vec![event_id],
                    rejected: Vec::new(),
                })
            })
        }
    }
}
