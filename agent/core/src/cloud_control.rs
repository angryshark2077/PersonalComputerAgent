use std::{error::Error, fmt, future::Future, pin::Pin, sync::Arc, time::Duration};

use ::time::{format_description::well_known::Rfc3339, OffsetDateTime};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use pca_db_local::{DbActorHandle, DbError, PairingState};
use pca_domain::{CommunicationScopeV2, EventEnvelope};
use pca_keychain::{
    delete_device_credential, load_device_credential, store_device_credential, CredentialError,
    CredentialStore, DeviceCredential,
};
use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    sync::{mpsc, oneshot, watch, Mutex},
    task::JoinHandle,
    time,
};
use uuid::Uuid;

use crate::communication::{
    CommunicationAuthorization, CommunicationControl, CommunicationIdentity,
};

const CONTROL_INTERVAL: Duration = Duration::from_secs(30);
const MAX_BACKOFF: Duration = Duration::from_mins(5);
const CREDENTIAL_REF: &str = "keychain://pca/device/current";
const CONTROL_OWNER_COMMAND_CAPACITY: usize = 8;
pub const PRODUCTION_CLOUD_API_ORIGIN: &str = "https://pca-cloud-api-production.up.railway.app";

/// Future returned by the small Cloud-control port.
pub type ControlFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ControlError>> + Send + 'a>>;

/// Failures a Cloud-control adapter can return without exposing response bodies or credentials.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlError {
    Transient,
    Revoked,
    InvalidCredential,
    Contract,
}

/// Authenticated, bounded Cloud-control operations owned by Agent Core.
pub trait ControlClient: Send + Sync {
    fn refresh<'a>(
        &'a self,
        credentials: &'a DeviceCredential,
    ) -> ControlFuture<'a, DeviceCredential>;

    fn heartbeat_and_control<'a>(
        &'a self,
        credentials: &'a DeviceCredential,
        outbox_depth: u64,
    ) -> ControlFuture<'a, AgentControlSnapshot>;

    fn sync_system_events<'a>(
        &'a self,
        _: &'a DeviceCredential,
        _: &'a [EventEnvelope],
    ) -> ControlFuture<'a, SyncEventsResponse> {
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
    pub collectors: CollectorControls,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CollectorControls {
    pub network: EnabledControl,
    #[serde(rename = "communication.wechat")]
    pub communication_wechat: CommunicationScopeV2,
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
        Ok(())
    }
}

/// Complete, durable desired configuration. S1B intentionally does not start either source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AppliedControl {
    pub configuration_revision: u64,
    pub network_enabled: bool,
    pub communication_wechat_enabled: bool,
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
    }))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ControlState {
    unpaired: bool,
    applied_revision: Option<u64>,
    communication_hydrated: bool,
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
) -> Result<CloudControlHandle, CloudControlRuntimeError> {
    let applied_revision = ensure_pairing_state(&database, credentials.credential()).await?;
    pairing_state_sender.send_replace(true);
    let state = Arc::new(Mutex::new(ControlState {
        unpaired: false,
        applied_revision: Some(applied_revision),
        communication_hydrated: false,
    }));
    let (shutdown_sender, shutdown_receiver) = watch::channel(false);
    let worker = tokio::spawn(run_control_loop(
        database,
        credentials,
        client,
        Arc::clone(&state),
        shutdown_receiver,
        pairing_state_sender,
        publication.clone(),
        authorization.clone(),
        owner_epoch,
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

impl CloudControlOwner {
    #[must_use]
    pub fn start(
        database: Arc<DbActorHandle>,
        pairing_state_sender: watch::Sender<bool>,
        authorization: CommunicationAuthorization,
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

    /// Reconciles startup Keychain state through the same serialized owner.
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
    current: &mut Option<CloudControlHandle>,
) -> Result<bool, CloudControlRuntimeError> {
    let owner_epoch =
        invalidate_and_stop_owned_control(authorization, publication, current).await?;
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

    /// Stops the worker without aborting an in-flight HTTPS request.
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
                        store_device_credential(credentials.store.as_ref(), &next)?;
                        credentials.credential = next;
                        ensure_pairing_state(&database, &credentials.credential).await?;
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

async fn control_once(
    database: &DbActorHandle,
    credentials: &mut LoadedDeviceCredentials,
    client: &dyn ControlClient,
    state: &Arc<Mutex<ControlState>>,
    publication: &ControlPublication,
    authorization: &CommunicationAuthorization,
    owner_epoch: u64,
) -> Result<(), ControlError> {
    let outbox_depth = database
        .active_outbox_depth()
        .await
        .map_err(|_| ControlError::Transient)?;
    let snapshot = client
        .heartbeat_and_control(&credentials.credential, outbox_depth)
        .await?;
    if snapshot.revoked {
        if !authorization.disable_for_owner(owner_epoch).await {
            return Err(ControlError::Transient);
        }
        publication.publish(owner_epoch, None).await;
        return Err(ControlError::Revoked);
    }
    if snapshot.device_id != credentials.credential.device_id()
        || snapshot.workspace_id != credentials.credential.workspace_id()
    {
        if !authorization.disable_for_owner(owner_epoch).await {
            return Err(ControlError::Transient);
        }
        publication.publish(owner_epoch, None).await;
        return Err(ControlError::Contract);
    }
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
        })
    } else {
        apply_snapshot(current, &snapshot)?
    };
    if applied.is_some_and(|applied| !applied.communication_wechat_enabled) {
        if !apply_communication_authorization(
            authorization,
            &credentials.credential,
            applied.unwrap(),
            owner_epoch,
        )
        .await?
        {
            return Err(ControlError::Transient);
        }
        publication.publish(owner_epoch, applied).await;
    }
    sync_pending_system_events(database, &credentials.credential, client).await?;
    let Some(applied) = applied else {
        return Ok(());
    };
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
            applied,
            owner_epoch,
        )
        .await?
        {
            return Err(ControlError::Transient);
        }
        publication.publish(owner_epoch, Some(applied)).await;
    }
    Ok(())
}

async fn sync_pending_system_events(
    database: &DbActorHandle,
    credentials: &DeviceCredential,
    client: &dyn ControlClient,
) -> Result<(), ControlError> {
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
        Ok(Self { base_url })
    }

    fn client() -> Result<Client, ControlError> {
        Client::builder()
            .https_only(true)
            .timeout(Duration::from_secs(15))
            .pool_max_idle_per_host(0)
            .build()
            .map_err(|_| ControlError::Transient)
    }

    fn endpoint(&self, path: &str) -> Result<Url, ControlError> {
        self.base_url.join(path).map_err(|_| ControlError::Contract)
    }
}

impl ControlClient for HttpControlClient {
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
        Box::pin(async move {
            let client = Self::client()?;
            let request = HeartbeatRequest {
                heartbeat_id: Uuid::new_v4().to_string(),
                agent_version: option_env!("PCA_APP_VERSION")
                    .unwrap_or(env!("CARGO_PKG_VERSION"))
                    .to_owned(),
                presence: "online",
                outbox_depth,
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
    if status.is_success() {
        return serde_json::from_slice(&bytes).map_err(|_| ControlError::Contract);
    }
    if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        return Err(ControlError::Transient);
    }
    if matches!(
        status,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::GONE
    ) {
        return match serde_json::from_slice::<ErrorResponse>(&bytes) {
            Ok(error) if error.error.error_code == "DEVICE_REVOKED" => Err(ControlError::Revoked),
            _ => Err(ControlError::InvalidCredential),
        };
    }
    Err(ControlError::Contract)
}

fn parse_time_ms(value: &str) -> Result<i64, ControlError> {
    let timestamp = OffsetDateTime::parse(value, &Rfc3339).map_err(|_| ControlError::Contract)?;
    i64::try_from(timestamp.unix_timestamp_nanos() / 1_000_000).map_err(|_| ControlError::Contract)
}

#[cfg(test)]
mod tests {
    use super::{
        retry_delay, sync_pending_system_events, AgentControlSnapshot, ControlClient, ControlError,
        ControlFuture, DeviceCredential, HttpControlClient, SyncEventsResponse, CONTROL_INTERVAL,
        MAX_BACKOFF, PRODUCTION_CLOUD_API_ORIGIN,
    };
    use pca_db_local::DbActorHandle;
    use pca_domain::{EventEnvelope, Sensitivity};
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
        assert_eq!(CONTROL_INTERVAL, Duration::from_secs(30));
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

    #[tokio::test]
    async fn accepted_system_events_are_acknowledged_without_touching_other_outbox_rows() {
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
            1
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
