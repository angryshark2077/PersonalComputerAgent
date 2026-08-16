use std::{
    fs, io,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use pca_bridge_client::{
    auth::verify_proof,
    framing::{read_frame_bytes, write_frame},
    NetworkObservationState,
};
use pca_db_local::DbActorHandle;
use pca_keychain::{load_bridge_shared_secret, load_device_credential, CredentialStore};
use reqwest::Url;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::{
    net::{UnixListener, UnixStream},
    sync::{watch, Mutex},
    time::{timeout, Duration},
};
use uuid::Uuid;

use crate::cloud_control::{
    synchronize_pairing_state, AgentPairingService, CloudControlCommands, ControlClient,
    HttpControlClient, LoadedDeviceCredentials, PairingCallbackHandoff, PairingClient,
    PairingStartHandoff, PRODUCTION_CLOUD_API_ORIGIN,
};

const PAIRING_IPC_PROTOCOL_VERSION: u32 = 1;

/// A private local endpoint reserved for the signed Setup/Repair application.
pub struct PairingSocket {
    listener: UnixListener,
    path: PathBuf,
}

/// Safe startup and cleanup failures for the local pairing socket.
#[derive(Debug)]
pub enum PairingSocketError {
    UnsafePath,
    Io(io::Error),
}

/// Runs the narrowly scoped Setup-to-Agent pairing endpoint.
pub struct PairingIpcServer {
    socket: PairingSocket,
    database: Arc<DbActorHandle>,
    store: Arc<dyn CredentialStore>,
    pending: Mutex<Option<PendingPairing>>,
    control_commands: CloudControlCommands,
    network_observations: Arc<NetworkObservationState>,
}

struct PendingPairing {
    session_id: String,
    service: Arc<AgentPairingService>,
    client: Arc<HttpControlClient>,
}

#[derive(Debug)]
pub enum PairingIpcServerError {
    Socket(PairingSocketError),
}

/// Strict, authenticated Setup-to-Agent request envelope.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingIpcRequest {
    protocol_version: u32,
    request_id: Uuid,
    operation: PairingIpcOperation,
    nonce: String,
    proof: String,
    #[serde(default)]
    payload: Option<Value>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PairingIpcOperation {
    Status,
    Begin,
    Complete,
    Cancel,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BeginPayload {
    callback_uri: String,
    cloud_api_origin: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompletePayload {
    session_id: String,
    authorization_code: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelPayload {
    session_id: String,
}

impl PairingIpcOperation {
    const fn name(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Begin => "begin",
            Self::Complete => "complete",
            Self::Cancel => "cancel",
        }
    }
}

impl PairingIpcRequest {
    /// Strictly decodes one public envelope before authentication and dispatch.
    ///
    /// # Errors
    ///
    /// Returns [`PairingIpcRequestError::Invalid`] for malformed or disallowed input.
    pub fn decode(bytes: &[u8]) -> Result<Self, PairingIpcRequestError> {
        let request: Self =
            serde_json::from_slice(bytes).map_err(|_| PairingIpcRequestError::Invalid)?;
        if request.protocol_version != PAIRING_IPC_PROTOCOL_VERSION || request.proof.is_empty() {
            return Err(PairingIpcRequestError::Invalid);
        }
        let nonce = URL_SAFE_NO_PAD
            .decode(&request.nonce)
            .map_err(|_| PairingIpcRequestError::Invalid)?;
        if nonce.len() != 32 {
            return Err(PairingIpcRequestError::Invalid);
        }
        match request.operation {
            PairingIpcOperation::Status => {
                if request.payload.is_some() {
                    return Err(PairingIpcRequestError::Invalid);
                }
            }
            PairingIpcOperation::Begin => {
                let _ = request.begin_payload()?;
            }
            PairingIpcOperation::Complete => {
                let _ = request.complete_payload()?;
            }
            PairingIpcOperation::Cancel => {
                let _ = request.cancel_payload()?;
            }
        }
        Ok(request)
    }

    fn begin_payload(&self) -> Result<BeginPayload, PairingIpcRequestError> {
        self.payload_as()
    }

    fn complete_payload(&self) -> Result<CompletePayload, PairingIpcRequestError> {
        self.payload_as()
    }

    fn cancel_payload(&self) -> Result<CancelPayload, PairingIpcRequestError> {
        self.payload_as()
    }

    fn payload_as<T: for<'de> Deserialize<'de>>(&self) -> Result<T, PairingIpcRequestError> {
        self.payload
            .clone()
            .ok_or(PairingIpcRequestError::Invalid)
            .and_then(|payload| {
                serde_json::from_value(payload).map_err(|_| PairingIpcRequestError::Invalid)
            })
    }

    /// Authenticates one request with the Keychain ACL-protected Bridge secret.
    ///
    /// # Errors
    ///
    /// Returns [`PairingIpcRequestError::Invalid`] when the request proof does not verify.
    pub fn authenticate(&self, secret: &[u8; 32]) -> Result<(), PairingIpcRequestError> {
        let nonce = URL_SAFE_NO_PAD
            .decode(&self.nonce)
            .map_err(|_| PairingIpcRequestError::Invalid)?;
        let nonce: [u8; 32] = nonce
            .try_into()
            .map_err(|_| PairingIpcRequestError::Invalid)?;
        let context = format!(
            "pca-setup-pairing-v1:{}:{}",
            self.request_id,
            self.operation.name()
        );
        verify_proof(secret, &nonce, self.protocol_version, &context, &self.proof)
            .map_err(|_| PairingIpcRequestError::Invalid)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingIpcRequestError {
    Invalid,
}

impl PairingSocket {
    /// Binds one 0600 Unix-domain socket after refusing symlinks and regular-file targets.
    ///
    /// # Errors
    ///
    /// Returns an error when the target is unsafe or the socket cannot be created securely.
    #[allow(clippy::unused_async)] // Tokio's Unix listener requires a current runtime to bind.
    pub async fn bind(path: impl AsRef<Path>) -> Result<Self, PairingSocketError> {
        let path = path.as_ref();
        if !path.is_absolute() || path == Path::new("/") || path.parent().is_none() {
            return Err(PairingSocketError::UnsafePath);
        }
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_socket() => {
                fs::remove_file(path).map_err(PairingSocketError::Io)?;
            }
            Ok(_) => return Err(PairingSocketError::UnsafePath),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(PairingSocketError::Io(error)),
        }
        let listener = UnixListener::bind(path).map_err(PairingSocketError::Io)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(PairingSocketError::Io)?;
        Ok(Self {
            listener,
            path: path.to_path_buf(),
        })
    }

    #[must_use]
    pub fn listener(&self) -> &UnixListener {
        &self.listener
    }

    /// Closes the listener and removes only the socket that this instance bound.
    ///
    /// # Errors
    ///
    /// Returns an error when the path changed into an unsafe target or cannot be removed.
    #[allow(clippy::unused_async)] // Socket teardown shares the async lifecycle API.
    pub async fn shutdown(self) -> Result<(), PairingSocketError> {
        let Self { listener, path } = self;
        drop(listener);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_socket() => {
                fs::remove_file(path).map_err(PairingSocketError::Io)
            }
            Ok(_) => Err(PairingSocketError::UnsafePath),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(PairingSocketError::Io(error)),
        }
    }
}

impl PairingIpcServer {
    #[must_use]
    pub fn new(
        socket: PairingSocket,
        database: Arc<DbActorHandle>,
        store: Arc<dyn CredentialStore>,
        control_commands: CloudControlCommands,
        network_observations: Arc<NetworkObservationState>,
    ) -> Self {
        Self {
            socket,
            database,
            store,
            pending: Mutex::new(None),
            control_commands,
            network_observations,
        }
    }

    /// Serves one bounded request per connection until the Agent lifecycle requests shutdown.
    ///
    /// # Errors
    ///
    /// Returns an error when the owned socket cannot be removed during shutdown.
    pub async fn serve(
        self,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(), PairingIpcServerError> {
        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                accepted = self.socket.listener().accept() => {
                    let (stream, _) = accepted
                        .map_err(|error| PairingIpcServerError::Socket(PairingSocketError::Io(error)))?;
                    let _ = self.handle_connection(stream).await;
                }
            }
        }
        self.socket
            .shutdown()
            .await
            .map_err(PairingIpcServerError::Socket)
    }

    #[allow(clippy::too_many_lines)] // The four protocol operations share one authenticated dispatch boundary.
    async fn handle_connection(&self, mut stream: UnixStream) -> Result<(), ()> {
        let bytes = timeout(Duration::from_secs(5), read_frame_bytes(&mut stream))
            .await
            .map_err(|_| ())?
            .map_err(|_| ())?;
        let request = PairingIpcRequest::decode(&bytes).map_err(|_| ())?;
        let secret = load_bridge_shared_secret(self.store.as_ref())
            .map_err(|_| ())?
            .ok_or(())?;
        request.authenticate(&secret).map_err(|_| ())?;

        let response = match request.operation {
            PairingIpcOperation::Status => {
                let paired = synchronize_pairing_state(self.database.as_ref(), self.store.as_ref())
                    .await
                    .unwrap_or(false);
                json!({ "paired": paired })
            }
            PairingIpcOperation::Begin => {
                let payload = request.begin_payload().map_err(|_| ())?;
                if payload.cloud_api_origin != PRODUCTION_CLOUD_API_ORIGIN {
                    error_response("REQUEST_INVALID")
                } else if self.pending.lock().await.is_some() {
                    error_response("PAIRING_IN_PROGRESS")
                } else {
                    let client = Url::parse(&payload.cloud_api_origin)
                        .ok()
                        .and_then(|origin| {
                            pairing_control_client(origin, Arc::clone(&self.network_observations))
                                .ok()
                                .map(Arc::new)
                        });
                    let Some(client) = client else {
                        return Err(());
                    };
                    let pairing_client: Arc<dyn PairingClient> = client.clone();
                    let service = Arc::new(AgentPairingService::new(
                        Arc::clone(&self.database),
                        Arc::clone(&self.store),
                        pairing_client,
                    ));
                    match service
                        .begin(PairingStartHandoff {
                            callback_uri: payload.callback_uri,
                        })
                        .await
                    {
                        Ok(session) => {
                            *self.pending.lock().await = Some(PendingPairing {
                                session_id: session.session_id.clone(),
                                service,
                                client,
                            });
                            json!({
                                "session_id": session.session_id,
                                "authorization_url": session.authorization_url,
                                "callback_state": session.callback_state,
                            })
                        }
                        Err(_) => error_response("PAIRING_UNAVAILABLE"),
                    }
                }
            }
            PairingIpcOperation::Complete => {
                let payload = request.complete_payload().map_err(|_| ())?;
                let pending = self
                    .pending
                    .lock()
                    .await
                    .as_ref()
                    .filter(|pending| pending.session_id == payload.session_id)
                    .map(|pending| (Arc::clone(&pending.service), Arc::clone(&pending.client)));
                match pending {
                    None => error_response("PAIRING_UNAVAILABLE"),
                    Some((service, client)) => match service
                        .complete(PairingCallbackHandoff {
                            session_id: payload.session_id.clone(),
                            authorization_code: payload.authorization_code,
                        })
                        .await
                    {
                        Ok(completion) => {
                            let mut pending = self.pending.lock().await;
                            if pending
                                .as_ref()
                                .is_some_and(|pending| pending.session_id == payload.session_id)
                            {
                                *pending = None;
                            }
                            drop(pending);
                            let credential =
                                load_device_credential(self.store.as_ref()).ok().flatten();
                            match credential {
                                Some(credential) => match self
                                    .control_commands
                                    .replace_identity(
                                        LoadedDeviceCredentials::new(
                                            credential,
                                            Arc::clone(&self.store),
                                        ),
                                        client as Arc<dyn ControlClient>,
                                    )
                                    .await
                                {
                                    Ok(()) => {
                                        json!({
                                            "device_id": completion.device_id,
                                            "workspace_id": completion.workspace_id,
                                        })
                                    }
                                    Err(_) => error_response("CONTROL_UNAVAILABLE"),
                                },
                                None => error_response("CONTROL_UNAVAILABLE"),
                            }
                        }
                        Err(_) => error_response("PAIRING_UNAVAILABLE"),
                    },
                }
            }
            PairingIpcOperation::Cancel => {
                let payload = request.cancel_payload().map_err(|_| ())?;
                let pending = {
                    let mut pending = self.pending.lock().await;
                    if pending
                        .as_ref()
                        .is_some_and(|pending| pending.session_id == payload.session_id)
                    {
                        pending.take()
                    } else {
                        None
                    }
                };
                if let Some(pending) = pending {
                    pending.service.cancel(&payload.session_id).await;
                }
                json!({ "cancelled": true })
            }
        };
        timeout(Duration::from_secs(5), write_frame(&mut stream, &response))
            .await
            .map_err(|_| ())?
            .map_err(|_| ())
    }
}

fn error_response(code: &str) -> Value {
    json!({ "error": { "code": code } })
}

fn pairing_control_client(
    origin: Url,
    network_observations: Arc<NetworkObservationState>,
) -> Result<HttpControlClient, crate::cloud_control::ControlError> {
    HttpControlClient::new(origin)
        .map(|client| client.with_network_observations(network_observations))
}

#[cfg(test)]
mod tests {
    use super::{pairing_control_client, ControlClient, NetworkObservationState, Url};
    use std::sync::Arc;

    #[test]
    fn pairing_control_client_uses_the_process_network_state() {
        let network_observations = Arc::new(NetworkObservationState::default());
        let client = pairing_control_client(
            Url::parse("https://pca-cloud-api-production.up.railway.app").expect("valid origin"),
            Arc::clone(&network_observations),
        )
        .expect("control client");

        client.set_network_enabled(true);

        assert!(network_observations.is_enabled());
    }
}
