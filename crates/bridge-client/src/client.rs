use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use pca_domain::{
    BridgeEnvelope, BridgeMessageKind, HandshakeChallenge, HandshakeChallengePhase,
    HandshakeResponse,
};
use pca_keychain::{load_bridge_shared_secret, CredentialError, CredentialStore};
use rand::{rngs::OsRng, RngCore};
use serde_json::{Map, Value};
use thiserror::Error;
use tokio::{net::UnixStream, time::timeout};
use uuid::Uuid;

use crate::{
    auth::verify_proof,
    framing::{read_frame, write_frame, FrameError},
};

pub const PROTOCOL_VERSION: u32 = 1;
const HANDSHAKE_CAPABILITY: &str = "bridge.handshake";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
pub struct BridgeClientConfig {
    socket_path: PathBuf,
    agent_version: String,
    timeout: Duration,
}

impl BridgeClientConfig {
    /// Creates a Bridge client configuration for one absolute Unix socket path.
    ///
    /// # Errors
    ///
    /// Rejects relative/empty socket paths and empty agent versions.
    pub fn new(
        socket_path: impl AsRef<Path>,
        agent_version: impl Into<String>,
    ) -> Result<Self, BridgeClientError> {
        let socket_path = socket_path.as_ref();
        let agent_version = agent_version.into();
        if !socket_path.is_absolute()
            || socket_path.as_os_str().is_empty()
            || socket_path == Path::new("/")
        {
            return Err(BridgeClientError::InvalidConfiguration);
        }
        if agent_version.is_empty() {
            return Err(BridgeClientError::InvalidConfiguration);
        }
        Ok(Self {
            socket_path: socket_path.to_path_buf(),
            agent_version,
            timeout: DEFAULT_TIMEOUT,
        })
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout.max(Duration::from_millis(1));
        self
    }

    pub(crate) fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

#[derive(Debug, Error)]
pub enum BridgeClientError {
    #[error("invalid Bridge client configuration")]
    InvalidConfiguration,
    #[error("Bridge credential is missing")]
    CredentialMissing,
    #[error("Bridge credential store is unavailable")]
    CredentialUnavailable,
    #[error("Bridge credential is invalid")]
    CredentialInvalid,
    #[error("Bridge connection failed")]
    ConnectionFailed,
    #[error("Bridge operation timed out")]
    Timeout,
    #[error("Bridge frame failed")]
    Frame(#[source] FrameError),
    #[error("Bridge nonce did not match")]
    NonceMismatch,
    #[error("Bridge authentication failed")]
    AuthenticationFailed,
    #[error("incompatible Bridge protocol: expected {expected}, received {actual}")]
    IncompatibleProtocol { expected: u32, actual: u32 },
    #[error("invalid Bridge envelope")]
    InvalidEnvelope,
    #[error("invalid Bridge handshake response")]
    InvalidHandshake,
}

impl PartialEq for BridgeClientError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::IncompatibleProtocol {
                    expected: left_expected,
                    actual: left_actual,
                },
                Self::IncompatibleProtocol {
                    expected: right_expected,
                    actual: right_actual,
                },
            ) => left_expected == right_expected && left_actual == right_actual,
            _ => std::mem::discriminant(self) == std::mem::discriminant(other),
        }
    }
}

impl Eq for BridgeClientError {}

pub struct BridgeClient {
    stream: UnixStream,
    operation_timeout: Duration,
}

impl std::fmt::Debug for BridgeClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BridgeClient")
            .field("operation_timeout", &self.operation_timeout)
            .finish_non_exhaustive()
    }
}

impl BridgeClient {
    /// Connects and completes the authenticated protocol-v1 handshake.
    ///
    /// # Errors
    ///
    /// Returns a safe typed error for credential, connection, deadline, framing, protocol, or
    /// authentication failures.
    pub async fn connect_and_handshake(
        config: BridgeClientConfig,
        credential_store: Arc<dyn CredentialStore>,
    ) -> Result<Self, BridgeClientError> {
        let secret = load_secret(credential_store.as_ref())?;
        let mut stream = timeout(config.timeout, UnixStream::connect(&config.socket_path))
            .await
            .map_err(|_| BridgeClientError::Timeout)?
            .map_err(|_| BridgeClientError::ConnectionFailed)?;

        let mut nonce = [0_u8; 32];
        OsRng.fill_bytes(&mut nonce);
        let encoded_nonce = STANDARD.encode(nonce);
        let request_id = Uuid::new_v4();
        let deadline_ms = duration_millis(config.timeout)?;
        let payload = object_payload(&HandshakeChallenge {
            phase: HandshakeChallengePhase::Challenge,
            nonce: encoded_nonce.clone(),
            agent_version: config.agent_version.clone(),
        })?;
        let challenge = BridgeEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id,
            message_kind: BridgeMessageKind::Request,
            capability: HANDSHAKE_CAPABILITY.to_owned(),
            deadline_ms,
            payload,
            error: None,
        };

        let response = timeout(config.timeout, async {
            write_envelope(&mut stream, &challenge).await?;
            read_envelope(&mut stream).await
        })
        .await
        .map_err(|_| BridgeClientError::Timeout)??;
        validate_response_envelope(&challenge, &response)?;
        if response.error.is_some() {
            return Err(BridgeClientError::InvalidHandshake);
        }
        let handshake: HandshakeResponse = serde_json::from_value(Value::Object(response.payload))
            .map_err(|_| BridgeClientError::InvalidHandshake)?;
        if handshake.nonce != encoded_nonce {
            return Err(BridgeClientError::NonceMismatch);
        }
        if handshake.bridge_version.is_empty() {
            return Err(BridgeClientError::InvalidHandshake);
        }
        verify_proof(
            &secret,
            &nonce,
            PROTOCOL_VERSION,
            &config.agent_version,
            &handshake.proof,
        )
        .map_err(|_| BridgeClientError::AuthenticationFailed)?;

        Ok(Self {
            stream,
            operation_timeout: config.timeout,
        })
    }

    /// Sends one request and validates its correlated response.
    ///
    /// # Errors
    ///
    /// Rejects malformed requests/responses and enforces the shorter of the configured operation
    /// timeout and the request's wire deadline.
    pub async fn request(
        &mut self,
        request: BridgeEnvelope,
    ) -> Result<BridgeEnvelope, BridgeClientError> {
        validate_request(&request)?;
        let wire_deadline = Duration::from_millis(request.deadline_ms);
        let operation_timeout = self.operation_timeout.min(wire_deadline);
        let response = timeout(operation_timeout, async {
            write_envelope(&mut self.stream, &request).await?;
            read_envelope(&mut self.stream).await
        })
        .await
        .map_err(|_| BridgeClientError::Timeout)??;
        validate_response_envelope(&request, &response)?;
        Ok(response)
    }
}

fn load_secret(store: &dyn CredentialStore) -> Result<[u8; 32], BridgeClientError> {
    load_bridge_shared_secret(store)
        .map_err(|error| match error {
            CredentialError::Unavailable | CredentialError::OperationFailed => {
                BridgeClientError::CredentialUnavailable
            }
            _ => BridgeClientError::CredentialInvalid,
        })?
        .ok_or(BridgeClientError::CredentialMissing)
}

fn validate_request(request: &BridgeEnvelope) -> Result<(), BridgeClientError> {
    if request.protocol_version != PROTOCOL_VERSION
        || request.message_kind != BridgeMessageKind::Request
        || request.capability.is_empty()
        || request.deadline_ms == 0
        || request.error.is_some()
    {
        return Err(BridgeClientError::InvalidEnvelope);
    }
    Ok(())
}

fn validate_response_envelope(
    request: &BridgeEnvelope,
    response: &BridgeEnvelope,
) -> Result<(), BridgeClientError> {
    if response.protocol_version != PROTOCOL_VERSION {
        return Err(BridgeClientError::IncompatibleProtocol {
            expected: PROTOCOL_VERSION,
            actual: response.protocol_version,
        });
    }
    if response.message_kind != BridgeMessageKind::Response
        || response.request_id != request.request_id
        || response.capability != request.capability
        || response.deadline_ms != request.deadline_ms
    {
        return Err(BridgeClientError::InvalidEnvelope);
    }
    Ok(())
}

async fn write_envelope(
    stream: &mut UnixStream,
    envelope: &BridgeEnvelope,
) -> Result<(), BridgeClientError> {
    let value = serde_json::to_value(envelope).map_err(|_| BridgeClientError::InvalidEnvelope)?;
    write_frame(stream, &value)
        .await
        .map_err(BridgeClientError::Frame)
}

async fn read_envelope(stream: &mut UnixStream) -> Result<BridgeEnvelope, BridgeClientError> {
    let value = read_frame(stream).await.map_err(BridgeClientError::Frame)?;
    serde_json::from_value(value).map_err(|_| BridgeClientError::InvalidEnvelope)
}

fn object_payload<T: serde::Serialize>(
    payload: &T,
) -> Result<Map<String, Value>, BridgeClientError> {
    serde_json::to_value(payload)
        .map_err(|_| BridgeClientError::InvalidEnvelope)?
        .as_object()
        .cloned()
        .ok_or(BridgeClientError::InvalidEnvelope)
}

fn duration_millis(duration: Duration) -> Result<u64, BridgeClientError> {
    u64::try_from(duration.as_millis()).map_err(|_| BridgeClientError::InvalidConfiguration)
}
