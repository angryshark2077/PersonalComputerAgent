use std::{
    net::IpAddr,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use pca_domain::{
    BridgeEnvelope, BridgeMessageKind, ErrorEnvelope, HandshakeChallenge, HandshakeChallengePhase,
    HandshakeResponsePhase,
};
use pca_keychain::{load_bridge_shared_secret, CredentialError, CredentialStore};
use rand::{rngs::OsRng, RngCore};
use serde::Deserialize;
use serde_json::{value::RawValue, Map, Value};
use thiserror::Error;
use tokio::{net::UnixStream, time::timeout};
use uuid::Uuid;

use crate::{
    auth::verify_proof,
    framing::{read_frame_bytes, write_frame, FrameError},
};

pub const PROTOCOL_VERSION: u32 = 1;
const HANDSHAKE_CAPABILITY: &str = "bridge.handshake";
const NETWORK_OBSERVE_CAPABILITY: &str = "network.observe";
const LIFECYCLE_POLL_CAPABILITY: &str = "system.lifecycle.poll";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_CLIENT_TIMEOUT: Duration = Duration::from_secs(30);

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
            || socket_path
                .components()
                .any(|component| component == Component::ParentDir)
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
        self.timeout = timeout
            .max(Duration::from_millis(1))
            .min(MAX_CLIENT_TIMEOUT);
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
    #[error("Bridge connection is disconnected")]
    Disconnected,
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
    stream: Option<UnixStream>,
    operation_timeout: Duration,
}

#[derive(Clone, Debug, Deserialize, serde::Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NetworkObservation {
    pub interface_type: String,
    pub wifi_identity_available: bool,
    pub ssid: Option<String>,
    pub bssid: Option<String>,
    pub local_ipv4: Option<String>,
    pub local_ipv6: Option<String>,
    pub location: Option<DeviceLocationObservation>,
}

#[derive(Clone, Debug, Deserialize, serde::Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DeviceLocationObservation {
    pub latitude: f64,
    pub longitude: f64,
    pub horizontal_accuracy_meters: f64,
    pub observed_at: String,
}

#[derive(Clone, Debug, Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlatformLifecycleEvent {
    pub sequence: u64,
    pub event_id: Uuid,
    pub event_type: String,
    pub occurred_at: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlatformLifecycleBatch {
    events: Vec<PlatformLifecycleEvent>,
    latest_sequence: u64,
}

impl std::fmt::Debug for BridgeClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BridgeClient")
            .field("connected", &self.stream.is_some())
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
            read_handshake_response(&mut stream).await
        })
        .await
        .map_err(|_| BridgeClientError::Timeout)??;
        validate_response_correlators(
            &challenge,
            response.request_id,
            response.message_kind,
            &response.capability,
            response.deadline_ms,
        )?;
        if response.error.is_some() {
            return Err(BridgeClientError::InvalidHandshake);
        }
        let handshake = response.payload;
        if handshake.phase != HandshakeResponsePhase::Response {
            return Err(BridgeClientError::InvalidHandshake);
        }
        if handshake.nonce != encoded_nonce {
            return Err(BridgeClientError::NonceMismatch);
        }
        if handshake.bridge_version.is_empty() {
            return Err(BridgeClientError::InvalidHandshake);
        }
        verify_proof(
            &secret,
            &nonce,
            response.protocol_version,
            &config.agent_version,
            &handshake.proof,
        )
        .map_err(|_| BridgeClientError::AuthenticationFailed)?;
        if response.protocol_version != PROTOCOL_VERSION {
            return Err(BridgeClientError::IncompatibleProtocol {
                expected: PROTOCOL_VERSION,
                actual: response.protocol_version,
            });
        }

        Ok(Self {
            stream: Some(stream),
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
        let mut stream = self.stream.take().ok_or(BridgeClientError::Disconnected)?;
        let wire_deadline = Duration::from_millis(request.deadline_ms);
        let operation_timeout = self.operation_timeout.min(wire_deadline);
        let response = timeout(operation_timeout, async {
            write_envelope(&mut stream, &request).await?;
            read_response_envelope(&mut stream).await
        })
        .await
        .map_err(|_| BridgeClientError::Timeout)??;
        validate_response_correlators(
            &request,
            response.request_id,
            response.message_kind,
            &response.capability,
            response.deadline_ms,
        )?;
        if response.protocol_version != PROTOCOL_VERSION {
            return Err(BridgeClientError::IncompatibleProtocol {
                expected: PROTOCOL_VERSION,
                actual: response.protocol_version,
            });
        }
        self.stream = Some(stream);
        Ok(response)
    }

    /// Requests one current platform network observation without performing geo lookup.
    ///
    /// # Errors
    ///
    /// Rejects malformed interface, Wi-Fi identity, or local address fields.
    pub async fn observe_network(&mut self) -> Result<NetworkObservation, BridgeClientError> {
        let request = BridgeEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: Uuid::new_v4(),
            message_kind: BridgeMessageKind::Request,
            capability: NETWORK_OBSERVE_CAPABILITY.to_owned(),
            deadline_ms: duration_millis(self.operation_timeout)?,
            payload: object_payload(&serde_json::json!({ "include_wifi_identity": true }))?,
            error: None,
        };
        let response = self.request(request).await?;
        if response.error.is_some() {
            return Err(BridgeClientError::InvalidEnvelope);
        }
        let observation: NetworkObservation =
            serde_json::from_value(Value::Object(response.payload))
                .map_err(|_| BridgeClientError::InvalidEnvelope)?;
        validate_network_observation(&observation)?;
        Ok(observation)
    }

    /// Polls bounded platform lifecycle events after the last successfully consumed sequence.
    ///
    /// # Errors
    ///
    /// Rejects unknown event types, malformed timestamps, nil identifiers, and non-monotonic
    /// sequence batches.
    pub async fn poll_lifecycle(
        &mut self,
        after_sequence: u64,
    ) -> Result<(Vec<PlatformLifecycleEvent>, u64), BridgeClientError> {
        let request = BridgeEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: Uuid::new_v4(),
            message_kind: BridgeMessageKind::Request,
            capability: LIFECYCLE_POLL_CAPABILITY.to_owned(),
            deadline_ms: duration_millis(self.operation_timeout)?,
            payload: object_payload(&serde_json::json!({ "after_sequence": after_sequence }))?,
            error: None,
        };
        let response = self.request(request).await?;
        if response.error.is_some() {
            return Err(BridgeClientError::InvalidEnvelope);
        }
        let batch: PlatformLifecycleBatch = serde_json::from_value(Value::Object(response.payload))
            .map_err(|_| BridgeClientError::InvalidEnvelope)?;
        validate_lifecycle_batch(after_sequence, &batch)?;
        Ok((batch.events, batch.latest_sequence))
    }
}

fn validate_lifecycle_batch(
    after_sequence: u64,
    batch: &PlatformLifecycleBatch,
) -> Result<(), BridgeClientError> {
    if batch.latest_sequence < after_sequence {
        return Err(BridgeClientError::InvalidEnvelope);
    }
    let mut previous = after_sequence;
    for event in &batch.events {
        if event.sequence <= previous
            || event.sequence > batch.latest_sequence
            || event.event_id.is_nil()
            || !matches!(
                event.event_type.as_str(),
                "system.sleep"
                    | "system.wake"
                    | "network.offline"
                    | "network.online"
                    | "network.changed"
            )
            || time::OffsetDateTime::parse(
                &event.occurred_at,
                &time::format_description::well_known::Rfc3339,
            )
            .is_err()
        {
            return Err(BridgeClientError::InvalidEnvelope);
        }
        previous = event.sequence;
    }
    Ok(())
}

fn validate_network_observation(observation: &NetworkObservation) -> Result<(), BridgeClientError> {
    if !matches!(
        observation.interface_type.as_str(),
        "wifi" | "wired" | "other" | "none"
    ) {
        return Err(BridgeClientError::InvalidEnvelope);
    }
    if observation.interface_type != "wifi"
        && (observation.ssid.is_some() || observation.bssid.is_some())
    {
        return Err(BridgeClientError::InvalidEnvelope);
    }
    if observation.wifi_identity_available
        != (observation.interface_type == "wifi"
            && observation.ssid.is_some()
            && observation.bssid.is_some())
    {
        return Err(BridgeClientError::InvalidEnvelope);
    }
    if observation
        .ssid
        .as_ref()
        .is_some_and(|ssid| ssid.is_empty() || ssid.len() > 128)
        || observation
            .bssid
            .as_ref()
            .is_some_and(|bssid| !valid_bssid(bssid))
        || observation.local_ipv4.as_ref().is_some_and(|value| {
            value
                .parse::<IpAddr>()
                .map_or(true, |address| !address.is_ipv4() || !usable_ip(address))
        })
        || observation.local_ipv6.as_ref().is_some_and(|value| {
            value
                .parse::<IpAddr>()
                .map_or(true, |address| !address.is_ipv6() || !usable_ip(address))
        })
        || observation.location.as_ref().is_some_and(|location| {
            !location.latitude.is_finite()
                || !(-90.0..=90.0).contains(&location.latitude)
                || !location.longitude.is_finite()
                || !(-180.0..=180.0).contains(&location.longitude)
                || !location.horizontal_accuracy_meters.is_finite()
                || !(0.0..=100_000.0).contains(&location.horizontal_accuracy_meters)
                || time::OffsetDateTime::parse(
                    &location.observed_at,
                    &time::format_description::well_known::Rfc3339,
                )
                .is_err()
        })
    {
        return Err(BridgeClientError::InvalidEnvelope);
    }
    Ok(())
}

fn valid_bssid(value: &str) -> bool {
    value.len() == 17
        && value.as_bytes().iter().enumerate().all(|(index, byte)| {
            if index % 3 == 2 {
                *byte == b':'
            } else {
                byte.is_ascii_digit() || (b'A'..=b'F').contains(byte)
            }
        })
}

fn usable_ip(address: IpAddr) -> bool {
    !address.is_loopback()
        && !address.is_unspecified()
        && !address.is_multicast()
        && !matches!(address, IpAddr::V4(value) if value.is_link_local())
        && !matches!(address, IpAddr::V6(value) if value.is_unicast_link_local())
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

fn validate_response_correlators(
    request: &BridgeEnvelope,
    response_request_id: Uuid,
    response_kind: BridgeMessageKind,
    response_capability: &str,
    response_deadline_ms: u64,
) -> Result<(), BridgeClientError> {
    if response_kind != BridgeMessageKind::Response
        || response_request_id != request.request_id
        || response_capability != request.capability
        || response_deadline_ms != request.deadline_ms
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

async fn read_handshake_response(
    stream: &mut UnixStream,
) -> Result<StrictEnvelope<StrictHandshakeResponse>, BridgeClientError> {
    let bytes = read_frame_bytes(stream)
        .await
        .map_err(BridgeClientError::Frame)?;
    let envelope: StrictEnvelope<Box<RawValue>> =
        serde_json::from_slice(&bytes).map_err(|_| BridgeClientError::InvalidEnvelope)?;
    let payload = serde_json::from_str(envelope.payload.get())
        .map_err(|_| BridgeClientError::InvalidHandshake)?;
    Ok(envelope.map_payload(payload))
}

async fn read_response_envelope(
    stream: &mut UnixStream,
) -> Result<BridgeEnvelope, BridgeClientError> {
    let bytes = read_frame_bytes(stream)
        .await
        .map_err(BridgeClientError::Frame)?;
    let strict: StrictEnvelope<Box<RawValue>> =
        serde_json::from_slice(&bytes).map_err(|_| BridgeClientError::InvalidEnvelope)?;
    let payload = serde_json::from_str(strict.payload.get())
        .map_err(|_| BridgeClientError::InvalidEnvelope)?;
    Ok(strict.map_payload(payload).into_domain())
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictEnvelope<T> {
    protocol_version: u32,
    request_id: Uuid,
    message_kind: BridgeMessageKind,
    capability: String,
    deadline_ms: u64,
    payload: T,
    #[serde(default)]
    error: Option<StrictErrorEnvelope>,
}

impl<T> StrictEnvelope<T> {
    fn map_payload<U>(self, payload: U) -> StrictEnvelope<U> {
        StrictEnvelope {
            protocol_version: self.protocol_version,
            request_id: self.request_id,
            message_kind: self.message_kind,
            capability: self.capability,
            deadline_ms: self.deadline_ms,
            payload,
            error: self.error,
        }
    }
}

impl StrictEnvelope<Map<String, Value>> {
    fn into_domain(self) -> BridgeEnvelope {
        BridgeEnvelope {
            protocol_version: self.protocol_version,
            request_id: self.request_id,
            message_kind: self.message_kind,
            capability: self.capability,
            deadline_ms: self.deadline_ms,
            payload: self.payload,
            error: self.error.map(StrictErrorEnvelope::into_domain),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictErrorEnvelope {
    error_code: String,
    message: String,
    retryable: bool,
    #[serde(default)]
    request_id: Option<Uuid>,
    #[serde(default)]
    details: Option<Map<String, Value>>,
}

impl StrictErrorEnvelope {
    fn into_domain(self) -> ErrorEnvelope {
        ErrorEnvelope {
            error_code: self.error_code,
            message: self.message,
            retryable: self.retryable,
            request_id: self.request_id,
            details: self.details,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictHandshakeResponse {
    phase: HandshakeResponsePhase,
    nonce: String,
    proof: String,
    bridge_version: String,
}

#[cfg(test)]
mod tests {
    use super::{
        validate_lifecycle_batch, validate_network_observation, DeviceLocationObservation,
        NetworkObservation, PlatformLifecycleBatch, PlatformLifecycleEvent,
    };
    use uuid::Uuid;

    #[test]
    fn network_observation_rejects_spoofed_wifi_and_unusable_addresses() {
        let valid = NetworkObservation {
            interface_type: "wifi".to_owned(),
            wifi_identity_available: true,
            ssid: Some("Jacob WiFi".to_owned()),
            bssid: Some("AA:BB:CC:DD:EE:FF".to_owned()),
            local_ipv4: Some("192.168.71.120".to_owned()),
            local_ipv6: None,
            location: Some(DeviceLocationObservation {
                latitude: 1.352_083,
                longitude: 103.819_836,
                horizontal_accuracy_meters: 24.5,
                observed_at: "2026-08-04T09:00:00Z".to_owned(),
            }),
        };
        validate_network_observation(&valid).expect("valid observation");

        let mut invalid = valid.clone();
        invalid.bssid = Some("aa:bb:cc:dd:ee:ff".to_owned());
        assert!(validate_network_observation(&invalid).is_err());

        invalid = valid.clone();
        invalid.local_ipv4 = Some("169.254.1.2".to_owned());
        assert!(validate_network_observation(&invalid).is_err());

        invalid = NetworkObservation {
            local_ipv4: Some("192.168.71.120".to_owned()),
            location: Some(DeviceLocationObservation {
                latitude: 91.0,
                longitude: 103.819_836,
                horizontal_accuracy_meters: 24.5,
                observed_at: "2026-08-04T09:00:00Z".to_owned(),
            }),
            ..valid
        };
        assert!(validate_network_observation(&invalid).is_err());
    }

    #[test]
    fn lifecycle_batch_requires_monotonic_known_events() {
        let event = PlatformLifecycleEvent {
            sequence: 3,
            event_id: Uuid::new_v4(),
            event_type: "system.wake".to_owned(),
            occurred_at: "2026-08-04T15:00:00Z".to_owned(),
        };
        let valid = PlatformLifecycleBatch {
            events: vec![event.clone()],
            latest_sequence: 3,
        };
        validate_lifecycle_batch(2, &valid).expect("valid lifecycle batch");

        let changed = PlatformLifecycleBatch {
            events: vec![PlatformLifecycleEvent {
                event_type: "network.changed".to_owned(),
                ..event.clone()
            }],
            latest_sequence: 3,
        };
        validate_lifecycle_batch(2, &changed).expect("network identity change is valid");

        let invalid = PlatformLifecycleBatch {
            events: vec![PlatformLifecycleEvent {
                event_type: "system.unknown".to_owned(),
                ..event
            }],
            latest_sequence: 3,
        };
        assert!(validate_lifecycle_batch(2, &invalid).is_err());
    }
}
