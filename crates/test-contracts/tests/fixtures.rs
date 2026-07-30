use pca_domain::{BridgeEnvelope, EventEnvelope};

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
