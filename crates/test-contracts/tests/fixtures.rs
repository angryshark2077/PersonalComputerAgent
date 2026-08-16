use pca_domain::{
    AgentStatus, BridgeEnvelope, BridgeMessageKind, BridgeStatus, EventEnvelope,
    HandshakeChallenge, HandshakeChallengePhase, HandshakeResponse, HandshakeResponsePhase,
    RuntimeStatusEnvelope,
};

#[test]
fn bridge_fixture_decodes_snake_case_and_object_payload() {
    let raw = include_str!("../../../packages/contracts/fixtures/bridge-request.valid.json");
    let envelope: BridgeEnvelope = serde_json::from_str(raw).expect("valid bridge fixture");

    assert_eq!(envelope.protocol_version, 1);
    assert_eq!(envelope.deadline_ms, 1_000);
    assert_eq!(envelope.payload["include_permissions"], true);
}

#[test]
fn event_fixture_round_trips_without_field_loss() {
    let raw = include_str!("../../../packages/contracts/fixtures/event.valid.json");
    let event: EventEnvelope = serde_json::from_str(raw).expect("valid event fixture");
    let encoded = serde_json::to_value(event).expect("encode event");

    assert_eq!(encoded["payload"]["state"], "running");
    assert_eq!(encoded["attachment_refs"][0], "attachment-001");
}

#[test]
fn runtime_status_fixture_decodes_every_canonical_field() {
    let raw =
        include_str!("../../../packages/contracts/fixtures/runtime-status.local-healthy.json");
    let status: RuntimeStatusEnvelope =
        serde_json::from_str(raw).expect("valid runtime status fixture");

    assert_eq!(status.agent_status, AgentStatus::Unpaired);
    assert_eq!(status.bridge_status, BridgeStatus::Ready);
    assert!(status.local_healthy);
    assert_eq!(status.heartbeat_at, "2026-07-31T00:00:00Z");
    assert_eq!(status.process_id, 4242);
    assert_eq!(status.app_version, "0.0.0-s1a");
    assert_eq!(status.schema_version, 2);
}

#[test]
fn handshake_fixtures_decode_every_canonical_field() {
    let challenge_raw =
        include_str!("../../../packages/contracts/fixtures/bridge-handshake.challenge.json");
    let challenge_envelope: BridgeEnvelope =
        serde_json::from_str(challenge_raw).expect("valid handshake challenge fixture");
    let challenge: HandshakeChallenge = serde_json::from_value(serde_json::Value::Object(
        challenge_envelope.payload.clone(),
    ))
    .expect("valid handshake challenge payload");

    assert_eq!(challenge_envelope.protocol_version, 1);
    assert_eq!(
        challenge_envelope.request_id.to_string(),
        "018f3f4a-2d9b-7d21-a310-2c49d9b43c12"
    );
    assert_eq!(challenge_envelope.message_kind, BridgeMessageKind::Request);
    assert_eq!(challenge_envelope.capability, "bridge.handshake");
    assert_eq!(challenge_envelope.deadline_ms, 1_000);
    assert!(challenge_envelope.error.is_none());
    assert_eq!(challenge.phase, HandshakeChallengePhase::Challenge);
    assert_eq!(challenge.nonce, "c2VjcmV0LWZyZWUtbm9uY2UtMDE=");
    assert_eq!(challenge.agent_version, "0.0.0-s1a");
    assert_eq!(
        challenge.client_proof,
        "c3ludGhldGljLWFnZW50LWhtYWMtcHJvb2Y="
    );

    let response_raw =
        include_str!("../../../packages/contracts/fixtures/bridge-handshake.response.json");
    let response_envelope: BridgeEnvelope =
        serde_json::from_str(response_raw).expect("valid handshake response fixture");
    let response: HandshakeResponse =
        serde_json::from_value(serde_json::Value::Object(response_envelope.payload.clone()))
            .expect("valid handshake response payload");

    assert_eq!(response_envelope.protocol_version, 1);
    assert_eq!(
        response_envelope.request_id.to_string(),
        "018f3f4a-2d9b-7d21-a310-2c49d9b43c12"
    );
    assert_eq!(response_envelope.message_kind, BridgeMessageKind::Response);
    assert_eq!(response_envelope.capability, "bridge.handshake");
    assert_eq!(response_envelope.deadline_ms, 1_000);
    assert!(response_envelope.error.is_none());
    assert_eq!(response.phase, HandshakeResponsePhase::Response);
    assert_eq!(response.nonce, "c2VjcmV0LWZyZWUtbm9uY2UtMDE=");
    assert_eq!(response.proof, "c3ludGhldGljLWhtYWMtc2hhMjU2LXByb29m");
    assert_eq!(response.bridge_version, "0.0.0-s1a");
}

#[test]
fn handshake_payloads_reject_mismatched_phases() {
    let mut challenge: serde_json::Value = serde_json::from_str(include_str!(
        "../../../packages/contracts/fixtures/bridge-handshake.challenge.json"
    ))
    .expect("valid handshake challenge fixture");
    challenge["payload"]["phase"] = serde_json::Value::String("response".to_owned());
    assert!(serde_json::from_value::<HandshakeChallenge>(challenge["payload"].clone()).is_err());

    let mut response: serde_json::Value = serde_json::from_str(include_str!(
        "../../../packages/contracts/fixtures/bridge-handshake.response.json"
    ))
    .expect("valid handshake response fixture");
    response["payload"]["phase"] = serde_json::Value::String("challenge".to_owned());
    assert!(serde_json::from_value::<HandshakeResponse>(response["payload"].clone()).is_err());
}
