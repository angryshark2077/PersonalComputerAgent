use std::{
    collections::{BTreeMap, HashSet},
    error::Error,
    fmt,
    future::Future,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};

use ::time::{format_description::well_known::Rfc3339, OffsetDateTime};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use pca_bridge_client::{
    NetworkObservation, NetworkObservationState, ScreenCaptureCommandHandle, ScreenCaptureStatus,
};
use pca_db_local::{
    CommunicationMediaStorageStats, DbActorHandle, DbError, PairingState,
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
    time,
};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::communication::{
    CommunicationAuthorization, CommunicationControl, CommunicationIdentity,
};

const CONTROL_INTERVAL: Duration = Duration::from_secs(30);
const CONTROL_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MEDIA_UPLOAD_TIMEOUT: Duration = Duration::from_mins(5);
const MEDIA_BATCH_SIZE: u16 = 4;
const MAX_BACKOFF: Duration = CONTROL_INTERVAL;
const CREDENTIAL_REF: &str = "keychain://pca/device/current";
const CONTROL_OWNER_COMMAND_CAPACITY: usize = 8;
pub const PRODUCTION_CLOUD_API_ORIGIN: &str = "https://pca-cloud-api-production.up.railway.app";

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
    _event_id: String,
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
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PairingCallbackHandoff {
    pub session_id: String,
    pub authorization_code: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct PairingSessionRequest {
    pub device_public_key: String,
    pub code_challenge: String,
    pub callback_uri: String,
    pub callback_state: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PairingSessionResponse {
    pub session_id: String,
    pub authorization_url: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct PairingExchangeRequest {
    pub session_id: String,
    pub authorization_code: String,
    pub code_verifier: String,
}

#[derive(Clone, Debug)]
struct PendingPairing {
    session_id: String,
    code_verifier: String,
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
        let code_verifier = random_url_safe_value();
        let callback_state = random_url_safe_value();
        let request = PairingSessionRequest {
            device_public_key: random_url_safe_value(),
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
        let mut pending = self.pending.lock().await;
        if pending.is_some() {
            return Err(ControlError::Contract);
        }
        *pending = Some(PendingPairing {
            session_id: response.session_id.clone(),
            code_verifier,
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
        let pending = self
            .pending
            .lock()
            .await
            .take()
            .filter(|pending| pending.session_id == handoff.session_id)
            .ok_or(CloudControlRuntimeError::Pairing(ControlError::Contract))?;
        if handoff.authorization_code.is_empty() {
            return Err(CloudControlRuntimeError::Pairing(ControlError::Contract));
        }
        let credential = self
            .client
            .exchange_pairing_callback(&PairingExchangeRequest {
                session_id: handoff.session_id,
                authorization_code: handoff.authorization_code,
                code_verifier: pending.code_verifier,
            })
            .await
            .map_err(CloudControlRuntimeError::Pairing)?;
        store_device_credential(self.store.as_ref(), &credential)?;
        ensure_pairing_state(&self.database, &credential).await?;
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

/// Starts the authenticated Cloud-control worker.
pub struct CloudControlRuntime;

/// Handle for observing and stopping the bounded Cloud-control worker.
pub struct CloudControlHandle {
    state: Arc<Mutex<ControlState>>,
    authorization: CommunicationAuthorization,
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
}

impl CloudControlRuntime {
    /// Loads the Keychain record at Agent startup. Missing or corrupt records fail closed to the
    /// unpaired state and leave no stale `SQLite` pointer behind.
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
        let credential = load_device_credential(store.as_ref())?.ok_or(
            CloudControlRuntimeError::Keychain(CredentialError::InvalidCredential),
        )?;
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
) -> Result<CloudControlHandle, CloudControlRuntimeError> {
    let applied_revision = ensure_pairing_state(&database, credentials.credential()).await?;
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
    ));
    Ok(CloudControlHandle {
        state,
        authorization,
        communication_controls,
        publication,
        owner_epoch,
        shutdown: Some(shutdown_sender),
        worker: Some(worker),
    })
}

#[allow(
    clippy::too_many_arguments,
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
) -> Result<(), CloudControlRuntimeError> {
    let (media_credentials, media_credential_receiver) =
        watch::channel(credentials.credential.clone());
    let (media_shutdown, media_shutdown_receiver) = watch::channel(false);
    let media_worker = tokio::spawn(run_media_loop(
        Arc::clone(&database),
        media_credential_receiver,
        Arc::clone(&client),
        media_shutdown_receiver,
    ));
    let apple_worker = screen_capture.as_ref().map(|bridge| {
        tokio::spawn(crate::apple_messages::run(
            Arc::clone(&database),
            bridge.clone(),
            credentials.credential().workspace_id().to_owned(),
            credentials.credential().device_id().to_owned(),
            screen_controls.clone(),
            media_shutdown.subscribe(),
        ))
    });
    let photo_worker = screen_capture.as_ref().map(|bridge| {
        tokio::spawn(crate::apple_photos::run(
            Arc::clone(&database),
            bridge.clone(),
            credentials.credential().workspace_id().to_owned(),
            credentials.credential().device_id().to_owned(),
            screen_controls.clone(),
            media_shutdown.subscribe(),
        ))
    });
    let screen_worker = screen_capture.map(|bridge| {
        tokio::spawn(run_screenshot_loop(
            screen_controls,
            media_credentials.subscribe(),
            Arc::clone(&client),
            bridge,
            media_shutdown.subscribe(),
        ))
    });
    let control_result = run_control_loop(
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
    )
    .await;
    media_shutdown.send_replace(true);
    media_worker.abort();
    if let Some(worker) = &screen_worker {
        worker.abort();
    }
    if let Some(worker) = &apple_worker {
        worker.abort();
    }
    if let Some(worker) = &photo_worker {
        worker.abort();
    }
    let media_result = match media_worker.await {
        Ok(result) => result,
        Err(error) if error.is_cancelled() => Ok(()),
        Err(_) => Err(CloudControlRuntimeError::WorkerStopped),
    };
    control_result?;
    media_result?;
    let screen_result = if let Some(worker) = screen_worker {
        match worker.await {
            Ok(result) => result,
            Err(error) if error.is_cancelled() => Ok(()),
            Err(_) => Err(CloudControlRuntimeError::WorkerStopped),
        }
    } else {
        Ok(())
    };
    if let Some(worker) = apple_worker {
        let _ = worker.await;
    }
    if let Some(worker) = photo_worker {
        let _ = worker.await;
    }
    screen_result
}

impl CloudControlOwner {
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
        let worker = tokio::spawn(run_control_owner(
            database,
            pairing_state_sender,
            authorization,
            communication_controls.clone(),
            publication,
            screen_capture,
            command_receiver,
            shutdown_receiver,
        ));
        (
            Self {
                communication_controls,
                shutdown: Some(shutdown),
                worker: Some(worker),
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
        match self.worker.take() {
            Some(worker) => worker
                .await
                .map_err(|_| CloudControlRuntimeError::WorkerStopped)?,
            None => Ok(()),
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
    mut commands: mpsc::Receiver<CloudControlOwnerCommand>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), CloudControlRuntimeError> {
    let mut current = None;
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
                            &mut current,
                        )
                        .await;
                        let _ = response.send(result);
                    }
                }
            }
        }
    }

    invalidate_and_stop_owned_control(&authorization, &publication, &mut current)
        .await
        .map(|_| ())
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
    current: &mut Option<CloudControlHandle>,
) -> Result<(), CloudControlRuntimeError> {
    let owner_epoch =
        invalidate_and_stop_owned_control(authorization, publication, current).await?;
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
    current: &mut Option<CloudControlHandle>,
) -> Result<bool, CloudControlRuntimeError> {
    if current.as_ref().is_some_and(|worker| !worker.is_finished()) {
        return Ok(true);
    }
    let owner_epoch = authorization.replace_owner().await;
    publication.replace_owner(owner_epoch).await;
    if let Some(worker) = current.take() {
        let _ = worker.shutdown().await;
    }
    if !synchronize_pairing_state_with_authorization(database, store.as_ref(), authorization)
        .await?
    {
        pairing_state_sender.send_replace(false);
        return Ok(false);
    }
    let credential = load_device_credential(store.as_ref())?.ok_or(
        CloudControlRuntimeError::Keychain(CredentialError::InvalidCredential),
    )?;
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
        )
        .await?,
    );
    Ok(true)
}

/// Reconciles the non-secret `SQLite` pointer with the Keychain record at Agent startup.
///
/// A missing or corrupt record is unpaired. A Keychain availability failure is returned so the
/// caller can keep unrelated local runtime capabilities alive in `degraded` state.
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
    match load_device_credential(store) {
        Ok(Some(credential)) => {
            ensure_pairing_state(database, &credential).await?;
            Ok(true)
        }
        Ok(None) | Err(CredentialError::InvalidCredential) => {
            authorization.disable().await;
            database
                .clear_pairing_state_and_disable_sensitive_collectors()
                .await?;
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
}

impl CloudControlHandle {
    fn is_finished(&self) -> bool {
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
        if self.authorization.disable_for_owner(self.owner_epoch).await {
            self.publication.publish(self.owner_epoch, None).await;
        }
        if let Some(shutdown) = self.shutdown.take() {
            shutdown.send_replace(true);
        }
        match self.worker.take() {
            Some(worker) => worker
                .await
                .map_err(|_| CloudControlRuntimeError::WorkerStopped)?,
            None => Ok(()),
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
            Err(ControlError::Transient) => {
                retry_attempt = retry_attempt.saturating_add(1);
                wait = retry_delay(retry_attempt);
            }
            Err(ControlError::Contract) => {
                client.set_network_enabled(false);
                if authorization.disable_for_owner(owner_epoch).await {
                    publication.publish(owner_epoch, None).await;
                }
                retry_attempt = retry_attempt.saturating_add(1);
                wait = retry_delay(retry_attempt);
            }
            Err(ControlError::InvalidCredential) => {
                match client.refresh(&credentials.credential).await {
                    Ok(next) => {
                        if next.device_id() != credentials.credential.device_id()
                            || next.workspace_id() != credentials.credential.workspace_id()
                        {
                            return revoke(
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
                        if !persist_refreshed_credential(
                            &database,
                            &mut credentials,
                            next,
                            &media_credentials,
                            &mut shutdown,
                        )
                        .await
                        {
                            return Ok(());
                        }
                        retry_attempt = 0;
                        wait = Duration::ZERO;
                    }
                    Err(ControlError::Revoked | ControlError::InvalidCredential) => {
                        return revoke(
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
                    Err(ControlError::Transient) => {
                        retry_attempt = retry_attempt.saturating_add(1);
                        wait = retry_delay(retry_attempt);
                    }
                    Err(ControlError::Contract) => {
                        if authorization.disable_for_owner(owner_epoch).await {
                            publication.publish(owner_epoch, None).await;
                        }
                        retry_attempt = retry_attempt.saturating_add(1);
                        wait = retry_delay(retry_attempt);
                    }
                }
            }
            Err(ControlError::Revoked) => {
                return revoke(
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
        if !authorization.disable_for_owner(owner_epoch).await {
            return Err(ControlError::Transient);
        }
        publication.publish(owner_epoch, None).await;
        return Err(ControlError::Revoked);
    }
    if snapshot.device_id != credentials.credential.device_id()
        || snapshot.workspace_id != credentials.credential.workspace_id()
    {
        client.set_network_enabled(false);
        if !authorization.disable_for_owner(owner_epoch).await {
            return Err(ControlError::Transient);
        }
        publication.publish(owner_epoch, None).await;
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
    sync_pending_system_events(database, &credentials.credential, client).await?;
    sync_pending_communication_events(database, &credentials.credential, client).await?;
    if let Some(applied) = applied {
        persist_network_collector_state(database, &applied).await?;
        client.set_network_enabled(applied.network_enabled);
        if applied.configuration_revision > current {
            database
                .save_control_revision(applied.configuration_revision)
                .await
                .map_err(|_| ControlError::Transient)?;
        }
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
    publication
        .publish(owner_epoch, Some(observed_control))
        .await;
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

async fn run_media_loop(
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
        let completed_media =
            sync_pending_communication_attachments(&database, &credential, client.as_ref())
                .await
                .unwrap_or(0);
        let _ = upload_pending_photos(client.as_ref(), &credential).await;
        wait = next_media_wait(completed_media);
    }
}

async fn run_screenshot_loop(
    mut controls: watch::Receiver<Option<AppliedControl>>,
    credentials: watch::Receiver<DeviceCredential>,
    client: Arc<dyn ControlClient>,
    bridge: ScreenCaptureCommandHandle,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), CloudControlRuntimeError> {
    let mut timer = time::interval(Duration::from_secs(5));
    timer.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut last_scheduled = Instant::now();
    let mut last_activity_capture = Instant::now();
    let mut last_activity_token: Option<String> = None;
    let mut handled_requests = HashSet::new();
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
        let credential = credentials.borrow().clone();
        let _ =
            upload_pending_screenshots(client.as_ref(), &credential, &mut handled_requests).await;
        let Some(control) = controls.borrow().clone() else {
            continue;
        };
        if !control.screen_capture.enabled {
            continue;
        }

        if let Some(request_id) = control.screenshot_request_id.as_deref() {
            if !handled_requests.contains(request_id) {
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
                    }
                    Ok(CaptureAttempt::Terminal(error_code)) => {
                        if client
                            .fail_screenshot_request(&credential, request_id, error_code)
                            .await
                            .is_ok()
                        {
                            handled_requests.insert(request_id.to_owned());
                        }
                    }
                    Ok(CaptureAttempt::Retry) | Err(_) => {}
                }
            }
        }

        if control.screen_capture.scheduled_enabled
            && last_scheduled.elapsed()
                >= Duration::from_secs(control.screen_capture.interval_seconds)
            && capture_screenshot(
                &bridge,
                &control.screen_capture.excluded_bundle_ids,
                ScreenshotTrigger::Scheduled,
                None,
            )
            .await
            .is_ok()
        {
            last_scheduled = Instant::now();
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
                        && capture_screenshot(
                            &bridge,
                            &control.screen_capture.excluded_bundle_ids,
                            ScreenshotTrigger::Activity,
                            None,
                        )
                        .await
                        .is_ok()
                    {
                        last_activity_capture = Instant::now();
                    }
                }
            }
        }
    }
}

enum CaptureAttempt {
    Queued,
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
        ScreenCaptureStatus::SkippedLocked | ScreenCaptureStatus::Unavailable => {
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
    client: &dyn ControlClient,
    credentials: &DeviceCredential,
    handled_requests: &mut HashSet<String>,
) -> Result<(), ControlError> {
    let spool_root = screenshot_spool_root()?;
    let mut entries = match tokio::fs::read_dir(&spool_root).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(ControlError::Transient),
    };
    let mut processed = 0_u8;
    while processed < 4 {
        let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|_| ControlError::Transient)?
        else {
            break;
        };
        let path = entry.path();
        if path.extension().is_none_or(|value| value != "json") {
            continue;
        }
        processed = processed.saturating_add(1);
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|_| ControlError::Transient)?;
        let screenshot: PendingScreenshot = if let Ok(screenshot) = serde_json::from_slice(&bytes) {
            screenshot
        } else {
            quarantine_manifest(&path).await?;
            continue;
        };
        if screenshot.mime_type != "image/jpeg"
            || Uuid::parse_str(&screenshot.screenshot_id).is_err()
            || screenshot
                .request_id
                .as_ref()
                .is_some_and(|value| Uuid::parse_str(value).is_err())
        {
            quarantine_manifest(&path).await?;
            continue;
        }
        let image_path = spool_root.join(&screenshot.image_file_name);
        if image_path.parent() != Some(spool_root.as_path()) {
            quarantine_manifest(&path).await?;
            continue;
        }
        remember_screenshot_request(&screenshot, handled_requests);
        if client
            .sync_screenshot(credentials, &screenshot)
            .await
            .is_err()
        {
            continue;
        }
        remove_uploaded_media_file(&image_path).await?;
        tokio::fs::remove_file(&path)
            .await
            .map_err(|_| ControlError::Transient)?;
    }
    Ok(())
}

async fn quarantine_manifest(path: &Path) -> Result<(), ControlError> {
    let quarantined = path.with_extension(format!("invalid-{}", Uuid::new_v4()));
    tokio::fs::rename(path, quarantined)
        .await
        .map_err(|_| ControlError::Transient)
}

fn remember_screenshot_request(
    screenshot: &PendingScreenshot,
    handled_requests: &mut HashSet<String>,
) {
    if let Some(request_id) = &screenshot.request_id {
        handled_requests.insert(request_id.clone());
    }
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

pub(crate) async fn photo_asset_is_handled(photo_id: &str) -> Result<bool, ControlError> {
    if Uuid::parse_str(photo_id).is_err() {
        return Err(ControlError::Contract);
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

pub(crate) async fn persist_photo_manifest(photo: &PendingPhoto) -> Result<(), ControlError> {
    let root = photo_spool_root()?;
    tokio::fs::create_dir_all(&root)
        .await
        .map_err(|_| ControlError::Transient)?;
    tokio::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
        .await
        .map_err(|_| ControlError::Transient)?;
    let bytes = serde_json::to_vec(photo).map_err(|_| ControlError::Contract)?;
    let final_path = root.join(format!("{}.json", photo.photo_id));
    let temporary_path = root.join(format!(".{}.tmp", photo.photo_id));
    tokio::fs::write(&temporary_path, bytes)
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
    client: &dyn ControlClient,
    credentials: &DeviceCredential,
) -> Result<(), ControlError> {
    let root = photo_spool_root()?;
    let mut entries = match tokio::fs::read_dir(&root).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(ControlError::Transient),
    };
    let mut processed = 0_u8;
    while processed < 4 {
        let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|_| ControlError::Transient)?
        else {
            break;
        };
        let path = entry.path();
        if path.extension().is_none_or(|value| value != "json") {
            continue;
        }
        processed = processed.saturating_add(1);
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
            continue;
        }
        let media_path = root.join(
            photo
                .media_file_name
                .as_deref()
                .ok_or(ControlError::Contract)?,
        );
        if media_path.parent() != Some(root.as_path()) {
            return Err(ControlError::Contract);
        }
        remove_uploaded_media_file(&media_path).await?;
        persist_photo_marker(&photo.photo_id, PhotoMarker::Completed).await?;
        tokio::fs::remove_file(&path)
            .await
            .map_err(|_| ControlError::Transient)?;
    }
    Ok(())
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
        .acknowledge_mismatched_lifecycle_events(
            credentials.workspace_id(),
            credentials.device_id(),
        )
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
    let response = client.sync_system_events(credentials, &events).await?;
    let acknowledged: std::collections::BTreeSet<_> = response
        .accepted
        .iter()
        .chain(response.duplicates.iter())
        .map(String::as_str)
        .collect();
    if !response.rejected.is_empty()
        || response.accepted.len() + response.duplicates.len() != expected.len()
        || acknowledged != expected
    {
        return Err(ControlError::Contract);
    }
    let event_ids = events
        .into_iter()
        .map(|event| event.event_id)
        .collect::<Vec<_>>();
    database
        .acknowledge_system_events(&event_ids)
        .await
        .map_err(|_| ControlError::Transient)
}

async fn sync_pending_communication_events(
    database: &DbActorHandle,
    credentials: &DeviceCredential,
    client: &dyn ControlClient,
) -> Result<(), ControlError> {
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
        .await?;
    let acknowledged: std::collections::BTreeSet<_> = response
        .accepted
        .iter()
        .chain(response.duplicates.iter())
        .map(String::as_str)
        .collect();
    if !response.rejected.is_empty()
        || response.accepted.len() + response.duplicates.len() != expected.len()
        || acknowledged != expected
    {
        return Err(ControlError::Contract);
    }
    let event_ids = events
        .into_iter()
        .map(|event| event.event_id)
        .collect::<Vec<_>>();
    database
        .acknowledge_communication_events(&event_ids)
        .await
        .map_err(|_| ControlError::Transient)
}

async fn sync_pending_communication_attachments(
    database: &DbActorHandle,
    credentials: &DeviceCredential,
    client: &dyn ControlClient,
) -> Result<usize, ControlError> {
    let attachments = database
        .load_pending_communication_attachments(MEDIA_BATCH_SIZE)
        .await
        .map_err(|_| ControlError::Transient)?;
    let attachment_count = attachments.len();
    let mut completed = 0_usize;
    for attachment in attachments {
        if let Err(failure) = client
            .sync_communication_attachment(credentials, &attachment)
            .await
        {
            if attachment_was_superseded(failure) {
                database
                    .complete_communication_attachment(&attachment.attachment_id)
                    .await
                    .map_err(|_| ControlError::Transient)?;
                completed = completed.saturating_add(1);
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
            continue;
        }
        database
            .complete_communication_attachment(&attachment.attachment_id)
            .await
            .map_err(|_| ControlError::Transient)?;
        completed = completed.saturating_add(1);
    }
    Ok(if completed > 0 { attachment_count } else { 0 })
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

async fn revoke(
    database: &DbActorHandle,
    credentials: &LoadedDeviceCredentials,
    state: &Arc<Mutex<ControlState>>,
    pairing_state_sender: &watch::Sender<bool>,
    publication: &ControlPublication,
    authorization: &CommunicationAuthorization,
    owner_epoch: u64,
) -> Result<(), CloudControlRuntimeError> {
    if !authorization.disable_for_owner(owner_epoch).await {
        return Ok(());
    }
    publication.publish(owner_epoch, None).await;
    {
        let mut state = state.lock().await;
        state.unpaired = true;
        state.applied_revision = None;
    }
    let keychain_result = delete_device_credential(credentials.store.as_ref());
    database
        .clear_pairing_state_and_disable_sensitive_collectors()
        .await?;
    pairing_state_sender.send_replace(false);
    keychain_result?;
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
        .timeout(MEDIA_UPLOAD_TIMEOUT)
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
            let response = client
                .post(self.endpoint("v1/agent/photos/prepare")?)
                .bearer_auth(credentials.access_credential())
                .json(&serde_json::json!({
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
                }))
                .send()
                .await
                .map_err(|_| ControlError::Transient)?;
            let prepared = parse_response::<PreparedPhoto>(response).await?;
            if prepared.photo_id != photo.photo_id {
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
                let root = photo_spool_root()?;
                if upload_photo(&client, upload_url.clone(), &upload.headers, photo, &root)
                    .await
                    .is_err()
                {
                    upload_photo(
                        &Self::direct_client()?,
                        upload_url,
                        &upload.headers,
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
        communication_attachment_is_missing, next_media_wait, photo_marker_path,
        quarantine_manifest, remember_screenshot_request, remove_uploaded_media_file, retry_delay,
        screenshot_prepare_payload, sync_pending_communication_events, sync_pending_system_events,
        AgentControlSnapshot, ControlClient, ControlError, ControlFuture, DeviceCredential,
        HttpControlClient, MediaUploadFailure, MediaUploadFailureStage, PendingScreenshot,
        PhotoMarker, ScreenshotTrigger, SyncEventsResponse, CONTROL_INTERVAL,
        CONTROL_REQUEST_TIMEOUT, MAX_BACKOFF, MEDIA_BATCH_SIZE, MEDIA_UPLOAD_TIMEOUT,
        PRODUCTION_CLOUD_API_ORIGIN,
    };
    use pca_db_local::{CommunicationMessageCommit, DbActorHandle};
    use pca_domain::{
        CommunicationMessageRecorded, CommunicationMessageRecordedInput, ConversationScope,
        Direction, EventEnvelope, MessageKind, Sensitivity,
    };
    use reqwest::Url;
    use serde_json::{Map, Value};
    use std::{env, time::Duration};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::{oneshot, Mutex as AsyncMutex},
        time::timeout,
    };

    static PROXY_ENVIRONMENT_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());

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

    #[test]
    fn restored_manual_screenshot_is_remembered_before_upload() {
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
        let mut handled = std::collections::HashSet::new();

        remember_screenshot_request(&screenshot, &mut handled);

        assert!(handled.contains(&request_id));
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

    struct PartialCommunicationSyncClient;

    impl ControlClient for PartialCommunicationSyncClient {
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
            _: &'a [EventEnvelope],
        ) -> ControlFuture<'a, SyncEventsResponse> {
            Box::pin(async {
                Ok(SyncEventsResponse {
                    batch_id: "test-batch".to_owned(),
                    accepted: Vec::new(),
                    duplicates: Vec::new(),
                    rejected: Vec::new(),
                })
            })
        }
    }

    #[tokio::test]
    async fn partial_communication_ack_keeps_the_local_outbox_pending() {
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

        assert!(matches!(
            sync_pending_communication_events(
                &database,
                &credential,
                &PartialCommunicationSyncClient
            )
            .await,
            Err(ControlError::Contract)
        ));
        assert_eq!(
            database
                .load_pending_communication_events(200)
                .await
                .expect("pending communication events")
                .len(),
            1
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
            Err(ControlError::Contract)
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
