use std::{
    collections::VecDeque,
    fs,
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
    time::Duration,
};

use pca_bridge_client::{
    auth::{create_proof, verify_proof},
    framing::{read_frame, write_frame, FrameError, MAX_FRAME_BYTES},
    supervisor::{BridgeSupervisor, BridgeSupervisorConfig},
    BridgeClient, BridgeClientConfig, BridgeClientError,
};
use pca_domain::{
    BridgeEnvelope, BridgeMessageKind, BridgeStatus, HandshakeResponse, HandshakeResponsePhase,
};
use pca_keychain::{
    CredentialError, CredentialStore, BRIDGE_CREDENTIAL_ACCOUNT, BRIDGE_CREDENTIAL_SERVICE,
};
use serde_json::{json, Map, Value};
use tokio::{
    io::{duplex, AsyncReadExt, AsyncWriteExt},
    net::UnixListener,
    sync::{mpsc, watch},
};
use uuid::Uuid;

const SECRET: [u8; 32] = [0x5a; 32];

#[derive(Clone)]
struct TestStore {
    result: Arc<Mutex<Result<Option<Vec<u8>>, CredentialError>>>,
}

impl TestStore {
    fn with_secret(secret: Option<Vec<u8>>) -> Self {
        Self {
            result: Arc::new(Mutex::new(Ok(secret))),
        }
    }

    fn unavailable() -> Self {
        Self {
            result: Arc::new(Mutex::new(Err(CredentialError::Unavailable))),
        }
    }
}

impl CredentialStore for TestStore {
    fn load(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, CredentialError> {
        assert_eq!(service, BRIDGE_CREDENTIAL_SERVICE);
        assert_eq!(account, BRIDGE_CREDENTIAL_ACCOUNT);
        self.result.lock().expect("test store lock").clone()
    }

    fn store(&self, _: &str, _: &str, _: &[u8]) -> Result<(), CredentialError> {
        unreachable!("client never stores credentials")
    }

    fn delete(&self, _: &str, _: &str) -> Result<(), CredentialError> {
        unreachable!("client never deletes credentials")
    }
}

#[tokio::test]
async fn fragmented_reads_preserve_two_back_to_back_frames() {
    let (mut writer, mut reader) = duplex(128);
    let first = json!({"sequence": 1});
    let second = json!({"sequence": 2});
    let mut bytes = encoded_frame(&first);
    bytes.extend(encoded_frame(&second));

    let writer_task = tokio::spawn(async move {
        for byte in bytes {
            writer.write_all(&[byte]).await.expect("fragment write");
        }
    });

    assert_eq!(read_frame(&mut reader).await.expect("first frame"), first);
    assert_eq!(read_frame(&mut reader).await.expect("second frame"), second);
    writer_task.await.expect("writer task");
}

#[tokio::test]
async fn framing_rejects_zero_oversized_truncated_invalid_utf8_and_invalid_json() {
    for (bytes, expected) in [
        (0_u32.to_be_bytes().to_vec(), FrameError::ZeroLength),
        (
            u32::try_from(MAX_FRAME_BYTES + 1)
                .expect("test frame length fits u32")
                .to_be_bytes()
                .to_vec(),
            FrameError::Oversized,
        ),
        (with_length(4, b"{}"), FrameError::Truncated),
        (with_length(1, &[0xff]), FrameError::InvalidUtf8),
        (with_length(1, b"{"), FrameError::InvalidJson),
    ] {
        let (mut writer, mut reader) = duplex(32);
        writer.write_all(&bytes).await.expect("fixture write");
        writer.shutdown().await.expect("fixture shutdown");
        assert_eq!(
            read_frame(&mut reader).await.expect_err("invalid frame"),
            expected
        );
    }
}

#[tokio::test]
async fn write_frame_rejects_payload_over_one_mib_before_writing() {
    let (mut writer, mut reader) = duplex(16);
    let payload = json!({"value": "x".repeat(MAX_FRAME_BYTES)});
    assert_eq!(
        write_frame(&mut writer, &payload)
            .await
            .expect_err("oversized payload"),
        FrameError::Oversized
    );
    drop(writer);
    let mut prefix = [0_u8; 1];
    assert_eq!(reader.read(&mut prefix).await.expect("empty stream"), 0);
}

#[test]
fn hmac_transcript_is_raw_nonce_then_u32_be_then_exact_agent_version_utf8() {
    let nonce = [0x11; 32];
    let proof = create_proof(&SECRET, &nonce, 0x0102_0304, "v1.β");
    assert_eq!(proof, "ZzHI3PgX7xuVBQpbtbnGsqP8Tvcu9WBICkuw1YUGwmc=");
    verify_proof(&SECRET, &nonce, 0x0102_0304, "v1.β", &proof).expect("valid proof");
    assert!(verify_proof(&SECRET, &nonce, 0x0102_0305, "v1.β", &proof).is_err());
}

#[tokio::test]
async fn handshake_rejects_deadline_nonce_hmac_and_protocol_failures() {
    for behavior in [
        ServerBehavior::Stall,
        ServerBehavior::WrongNonce,
        ServerBehavior::WrongProof,
        ServerBehavior::Incompatible,
        ServerBehavior::WrongKind,
        ServerBehavior::WrongCapability,
        ServerBehavior::WrongRequestId,
        ServerBehavior::WrongPhase,
        ServerBehavior::EmptyBridgeVersion,
    ] {
        let result = connect_to_fake(behavior, Duration::from_millis(40)).await;
        match behavior {
            ServerBehavior::Stall => assert!(matches!(result, Err(BridgeClientError::Timeout))),
            ServerBehavior::WrongNonce => {
                assert!(matches!(result, Err(BridgeClientError::NonceMismatch)));
            }
            ServerBehavior::WrongProof => {
                assert!(matches!(
                    result,
                    Err(BridgeClientError::AuthenticationFailed)
                ));
            }
            ServerBehavior::Incompatible => {
                assert!(matches!(
                    result,
                    Err(BridgeClientError::IncompatibleProtocol { .. })
                ));
            }
            ServerBehavior::WrongKind
            | ServerBehavior::WrongCapability
            | ServerBehavior::WrongRequestId => {
                assert!(matches!(result, Err(BridgeClientError::InvalidEnvelope)));
            }
            ServerBehavior::WrongPhase | ServerBehavior::EmptyBridgeVersion => {
                assert!(matches!(result, Err(BridgeClientError::InvalidHandshake)));
            }
            ServerBehavior::Valid => unreachable!(),
        }
    }
}

#[tokio::test]
async fn each_connection_uses_a_fresh_cryptographic_nonce() {
    let seen = Arc::new(Mutex::new(VecDeque::new()));
    for _ in 0..2 {
        let observed = Arc::clone(&seen);
        connect_to_fake_observing(ServerBehavior::Valid, Duration::from_secs(1), observed)
            .await
            .expect("handshake");
    }
    let seen = seen.lock().expect("seen nonce lock");
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].len(), 32);
    assert_ne!(seen[0], seen[1]);
}

#[tokio::test]
async fn credential_load_is_fixed_and_has_no_missing_or_unavailable_fallback() {
    for (store, expected) in [
        (
            TestStore::with_secret(None),
            BridgeClientError::CredentialMissing,
        ),
        (
            TestStore::unavailable(),
            BridgeClientError::CredentialUnavailable,
        ),
    ] {
        let config = BridgeClientConfig::new("/tmp/does-not-matter.sock", "0.0.0-s1a")
            .expect("absolute socket");
        let error = BridgeClient::connect_and_handshake(config, Arc::new(store))
            .await
            .expect_err("credential failure before connect");
        assert_eq!(error, expected);
    }
}

#[test]
fn client_and_supervisor_reject_non_absolute_or_root_paths() {
    assert_eq!(
        BridgeClientConfig::new("relative.sock", "0.0.0-s1a").expect_err("relative socket"),
        BridgeClientError::InvalidConfiguration
    );
    assert_eq!(
        BridgeClientConfig::new("/", "0.0.0-s1a").expect_err("root socket"),
        BridgeClientError::InvalidConfiguration
    );
    assert_eq!(
        BridgeSupervisorConfig::new("relative-bridge", "/tmp/bridge.sock", "0.0.0-s1a")
            .expect_err("relative executable"),
        BridgeClientError::InvalidConfiguration
    );
}

#[tokio::test]
async fn request_enforces_wire_deadline_and_correlates_every_response_field() {
    for (behavior, expected) in [
        (RequestBehavior::Stall, BridgeClientError::Timeout),
        (
            RequestBehavior::WrongVersion,
            BridgeClientError::IncompatibleProtocol {
                expected: 1,
                actual: 999,
            },
        ),
        (
            RequestBehavior::WrongKind,
            BridgeClientError::InvalidEnvelope,
        ),
        (
            RequestBehavior::WrongCapability,
            BridgeClientError::InvalidEnvelope,
        ),
        (
            RequestBehavior::WrongRequestId,
            BridgeClientError::InvalidEnvelope,
        ),
        (
            RequestBehavior::WrongDeadline,
            BridgeClientError::InvalidEnvelope,
        ),
    ] {
        let (mut client, server) = connect_for_request(behavior).await;
        let mut envelope = request(Map::new());
        envelope.deadline_ms = 35;
        let error = client
            .request(envelope)
            .await
            .expect_err("invalid response");
        assert_eq!(error, expected);
        server.abort();
    }

    let (mut client, server) = connect_for_request(RequestBehavior::Valid).await;
    let response = client
        .request(request(Map::new()))
        .await
        .expect("correlated response");
    assert_eq!(response.payload["screen_capture"], "available");
    server.await.expect("request server");
}

#[tokio::test]
async fn request_rejects_invalid_outgoing_envelopes_before_io() {
    let (mut client, server) = connect_for_request(RequestBehavior::Stall).await;
    for invalid in [
        BridgeEnvelope {
            protocol_version: 999,
            ..request(Map::new())
        },
        BridgeEnvelope {
            message_kind: BridgeMessageKind::Response,
            ..request(Map::new())
        },
        BridgeEnvelope {
            capability: String::new(),
            ..request(Map::new())
        },
        BridgeEnvelope {
            deadline_ms: 0,
            ..request(Map::new())
        },
    ] {
        assert_eq!(
            client.request(invalid).await.expect_err("invalid request"),
            BridgeClientError::InvalidEnvelope
        );
    }
    server.abort();
}

#[tokio::test]
async fn supervisor_restarts_a_crashed_child_reconnects_and_cancels_without_leaking() {
    let directory = tempfile::tempdir().expect("tempdir");
    let executable = directory.path().join("fake bridge;no-shell");
    let socket = directory.path().join("bridge.sock");
    fs::write(&executable, fake_bridge_script(false)).expect("write fake bridge");
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
        .expect("make fake executable");

    let config = BridgeSupervisorConfig::new(&executable, &socket, "0.0.0-s1a")
        .expect("valid supervisor config")
        .with_operation_timeout(Duration::from_secs(1))
        .with_backoff(
            Duration::from_millis(5),
            Duration::from_millis(20),
            Duration::from_millis(50),
        );
    let (status_tx, mut status_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let supervisor = BridgeSupervisor::new(
        config,
        Arc::new(TestStore::with_secret(Some(SECRET.to_vec()))),
        status_tx,
    );
    let task = tokio::spawn(supervisor.run(shutdown_rx));

    let mut statuses = Vec::new();
    while statuses
        .iter()
        .filter(|status| **status == BridgeStatus::Ready)
        .count()
        < 2
    {
        statuses.push(
            tokio::time::timeout(Duration::from_secs(3), status_rx.recv())
                .await
                .expect("status deadline")
                .expect("status channel"),
        );
    }
    shutdown_tx.send(true).expect("request shutdown");
    task.await
        .expect("supervisor task")
        .expect("clean supervisor shutdown");

    assert!(contains_ordered(
        &statuses,
        &[
            BridgeStatus::Disconnected,
            BridgeStatus::Handshaking,
            BridgeStatus::Ready,
            BridgeStatus::Degraded,
            BridgeStatus::Handshaking,
            BridgeStatus::Ready,
        ]
    ));
    let runs = fs::read_to_string(directory.path().join("runs")).expect("run count");
    assert_eq!(runs.trim(), "2");
}

#[tokio::test]
async fn supervisor_emits_incompatible_once_and_does_not_restart() {
    let directory = tempfile::tempdir().expect("tempdir");
    let executable = directory.path().join("incompatible-bridge");
    let socket = directory.path().join("bridge.sock");
    fs::write(&executable, fake_bridge_script(true)).expect("write fake bridge");
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
        .expect("make fake executable");
    let config = BridgeSupervisorConfig::new(&executable, &socket, "0.0.0-s1a")
        .expect("valid supervisor config")
        .with_operation_timeout(Duration::from_secs(1));
    let (status_tx, mut status_rx) = mpsc::unbounded_channel();
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    BridgeSupervisor::new(
        config,
        Arc::new(TestStore::with_secret(Some(SECRET.to_vec()))),
        status_tx,
    )
    .run(shutdown_rx)
    .await
    .expect("incompatible is a terminal supervised state");

    let mut statuses = Vec::new();
    while let Ok(status) = status_rx.try_recv() {
        statuses.push(status);
    }
    assert_eq!(statuses.last(), Some(&BridgeStatus::Incompatible));
    let runs = fs::read_to_string(directory.path().join("runs")).expect("run count");
    assert_eq!(runs.trim(), "1");
}

#[derive(Clone, Copy)]
enum ServerBehavior {
    Valid,
    Stall,
    WrongNonce,
    WrongProof,
    Incompatible,
    WrongKind,
    WrongCapability,
    WrongRequestId,
    WrongPhase,
    EmptyBridgeVersion,
}

#[derive(Clone, Copy)]
enum RequestBehavior {
    Valid,
    Stall,
    WrongVersion,
    WrongKind,
    WrongCapability,
    WrongRequestId,
    WrongDeadline,
}

async fn connect_to_fake(
    behavior: ServerBehavior,
    timeout: Duration,
) -> Result<BridgeClient, BridgeClientError> {
    connect_to_fake_observing(behavior, timeout, Arc::new(Mutex::new(VecDeque::new()))).await
}

async fn connect_to_fake_observing(
    behavior: ServerBehavior,
    timeout: Duration,
    observed: Arc<Mutex<VecDeque<Vec<u8>>>>,
) -> Result<BridgeClient, BridgeClientError> {
    let directory = tempfile::tempdir().expect("tempdir");
    let socket = directory.path().join("bridge.sock");
    let listener = UnixListener::bind(&socket).expect("bind fake bridge");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept client");
        let challenge: BridgeEnvelope =
            serde_json::from_value(read_frame(&mut stream).await.expect("challenge frame"))
                .expect("challenge envelope");
        let nonce = challenge.payload["nonce"].as_str().expect("nonce string");
        let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, nonce)
            .expect("base64 nonce");
        observed
            .lock()
            .expect("observed lock")
            .push_back(decoded.clone());
        if matches!(behavior, ServerBehavior::Stall) {
            tokio::time::sleep(Duration::from_secs(5)).await;
            return;
        }
        let mut nonce_bytes = [0_u8; 32];
        nonce_bytes.copy_from_slice(&decoded);
        let response_nonce = if matches!(behavior, ServerBehavior::WrongNonce) {
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [0_u8; 32])
        } else {
            nonce.to_owned()
        };
        let proof = if matches!(behavior, ServerBehavior::WrongProof) {
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [0_u8; 32])
        } else {
            create_proof(&SECRET, &nonce_bytes, 1, "0.0.0-s1a")
        };
        let mut payload = serde_json::to_value(HandshakeResponse {
            phase: HandshakeResponsePhase::Response,
            nonce: response_nonce,
            proof,
            bridge_version: if matches!(behavior, ServerBehavior::EmptyBridgeVersion) {
                String::new()
            } else {
                "0.0.0-s1a".to_owned()
            },
        })
        .expect("response payload")
        .as_object()
        .expect("object payload")
        .clone();
        if matches!(behavior, ServerBehavior::WrongPhase) {
            payload.insert("phase".to_owned(), Value::String("challenge".to_owned()));
        }
        let response = BridgeEnvelope {
            protocol_version: if matches!(behavior, ServerBehavior::Incompatible) {
                999
            } else {
                1
            },
            request_id: if matches!(behavior, ServerBehavior::WrongRequestId) {
                Uuid::new_v4()
            } else {
                challenge.request_id
            },
            message_kind: if matches!(behavior, ServerBehavior::WrongKind) {
                BridgeMessageKind::Event
            } else {
                BridgeMessageKind::Response
            },
            capability: if matches!(behavior, ServerBehavior::WrongCapability) {
                "system.capabilities".to_owned()
            } else {
                "bridge.handshake".to_owned()
            },
            deadline_ms: challenge.deadline_ms,
            payload,
            error: None,
        };
        write_frame(
            &mut stream,
            &serde_json::to_value(response).expect("response envelope"),
        )
        .await
        .expect("response frame");
    });

    let config = BridgeClientConfig::new(&socket, "0.0.0-s1a")
        .expect("valid config")
        .with_timeout(timeout);
    let result = BridgeClient::connect_and_handshake(
        config,
        Arc::new(TestStore::with_secret(Some(SECRET.to_vec()))),
    )
    .await;
    server.abort();
    result
}

async fn connect_for_request(
    behavior: RequestBehavior,
) -> (BridgeClient, tokio::task::JoinHandle<()>) {
    let directory = tempfile::tempdir().expect("tempdir");
    let socket = directory.path().join("request-bridge.sock");
    let listener = UnixListener::bind(&socket).expect("bind request fake");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept client");
        let challenge: BridgeEnvelope =
            serde_json::from_value(read_frame(&mut stream).await.expect("challenge frame"))
                .expect("challenge envelope");
        let nonce = challenge.payload["nonce"].as_str().expect("nonce string");
        let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, nonce)
            .expect("base64 nonce");
        let mut nonce_bytes = [0_u8; 32];
        nonce_bytes.copy_from_slice(&decoded);
        let handshake_payload = serde_json::to_value(HandshakeResponse {
            phase: HandshakeResponsePhase::Response,
            nonce: nonce.to_owned(),
            proof: create_proof(&SECRET, &nonce_bytes, 1, "0.0.0-s1a"),
            bridge_version: "0.0.0-s1a".to_owned(),
        })
        .expect("handshake payload")
        .as_object()
        .expect("object payload")
        .clone();
        let handshake = BridgeEnvelope {
            protocol_version: 1,
            request_id: challenge.request_id,
            message_kind: BridgeMessageKind::Response,
            capability: challenge.capability,
            deadline_ms: challenge.deadline_ms,
            payload: handshake_payload,
            error: None,
        };
        write_frame(
            &mut stream,
            &serde_json::to_value(handshake).expect("handshake envelope"),
        )
        .await
        .expect("handshake frame");

        let request: BridgeEnvelope =
            serde_json::from_value(read_frame(&mut stream).await.expect("request frame"))
                .expect("request envelope");
        if matches!(behavior, RequestBehavior::Stall) {
            tokio::time::sleep(Duration::from_secs(5)).await;
            return;
        }
        let response = BridgeEnvelope {
            protocol_version: if matches!(behavior, RequestBehavior::WrongVersion) {
                999
            } else {
                1
            },
            request_id: if matches!(behavior, RequestBehavior::WrongRequestId) {
                Uuid::new_v4()
            } else {
                request.request_id
            },
            message_kind: if matches!(behavior, RequestBehavior::WrongKind) {
                BridgeMessageKind::Event
            } else {
                BridgeMessageKind::Response
            },
            capability: if matches!(behavior, RequestBehavior::WrongCapability) {
                "different.capability".to_owned()
            } else {
                request.capability
            },
            deadline_ms: if matches!(behavior, RequestBehavior::WrongDeadline) {
                request.deadline_ms + 1
            } else {
                request.deadline_ms
            },
            payload: json!({"screen_capture": "available"})
                .as_object()
                .expect("object response")
                .clone(),
            error: None,
        };
        write_frame(
            &mut stream,
            &serde_json::to_value(response).expect("response envelope"),
        )
        .await
        .expect("response frame");
    });
    let config = BridgeClientConfig::new(&socket, "0.0.0-s1a")
        .expect("valid config")
        .with_timeout(Duration::from_secs(1));
    let client = BridgeClient::connect_and_handshake(
        config,
        Arc::new(TestStore::with_secret(Some(SECRET.to_vec()))),
    )
    .await
    .expect("request handshake");
    (client, server)
}

fn encoded_frame(value: &Value) -> Vec<u8> {
    let json = serde_json::to_vec(value).expect("encode fixture");
    let mut frame = u32::try_from(json.len())
        .expect("test frame length fits u32")
        .to_be_bytes()
        .to_vec();
    frame.extend(json);
    frame
}

fn with_length(length: u32, bytes: &[u8]) -> Vec<u8> {
    let mut frame = length.to_be_bytes().to_vec();
    frame.extend(bytes);
    frame
}

#[allow(dead_code)]
fn request(payload: Map<String, Value>) -> BridgeEnvelope {
    BridgeEnvelope {
        protocol_version: 1,
        request_id: Uuid::new_v4(),
        message_kind: BridgeMessageKind::Request,
        capability: "system.capabilities".to_owned(),
        deadline_ms: 100,
        payload,
        error: None,
    }
}

fn contains_ordered(actual: &[BridgeStatus], expected: &[BridgeStatus]) -> bool {
    let mut remaining = expected.iter();
    let mut next = remaining.next();
    for status in actual {
        if next == Some(status) {
            next = remaining.next();
        }
    }
    next.is_none()
}

fn fake_bridge_script(incompatible: bool) -> String {
    let protocol_version = if incompatible { 999 } else { 1 };
    format!(
        r"#!/usr/bin/python3
import base64, hashlib, hmac, json, os, socket, struct, sys, time
if len(sys.argv) != 3 or sys.argv[1] != '--socket' or not os.path.isabs(sys.argv[2]):
    sys.exit(64)
socket_path = sys.argv[2]
runs_path = os.path.join(os.path.dirname(__file__), 'runs')
try:
    with open(runs_path, 'r', encoding='ascii') as handle:
        runs = int(handle.read()) + 1
except FileNotFoundError:
    runs = 1
with open(runs_path, 'w', encoding='ascii') as handle:
    handle.write(str(runs))
try:
    os.unlink(socket_path)
except FileNotFoundError:
    pass
server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
server.bind(socket_path)
server.listen(1)
connection, _ = server.accept()
def exact(count):
    data = b''
    while len(data) < count:
        chunk = connection.recv(count - len(data))
        if not chunk:
            raise RuntimeError('truncated')
        data += chunk
    return data
length = struct.unpack('>I', exact(4))[0]
challenge = json.loads(exact(length).decode('utf-8'))
nonce = base64.b64decode(challenge['payload']['nonce'])
transcript = nonce + struct.pack('>I', 1) + challenge['payload']['agent_version'].encode('utf-8')
proof = base64.b64encode(hmac.new(bytes([0x5a]) * 32, transcript, hashlib.sha256).digest()).decode('ascii')
response = {{
    'protocol_version': {protocol_version},
    'request_id': challenge['request_id'],
    'message_kind': 'response',
    'capability': 'bridge.handshake',
    'deadline_ms': challenge['deadline_ms'],
    'payload': {{'phase': 'response', 'nonce': challenge['payload']['nonce'], 'proof': proof, 'bridge_version': '0.0.0-s1a'}},
    'error': None,
}}
encoded = json.dumps(response, separators=(',', ':')).encode('utf-8')
connection.sendall(struct.pack('>I', len(encoded)) + encoded)
if {incompatible_literal}:
    time.sleep(30)
elif runs == 1:
    time.sleep(0.03)
    sys.exit(7)
else:
    time.sleep(30)
",
        incompatible_literal = if incompatible { "True" } else { "False" },
    )
}
