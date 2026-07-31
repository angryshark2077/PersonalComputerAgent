use pca_domain::{
    AgentCpuMemory, CollectorState, CollectorStatus, CpuMemorySample, DiskSample, DiskScope,
    EventCommit, EventEnvelope, HostCpuMemory, Sensitivity, SystemMetricSample,
    MAX_EVENTS_PER_COMMIT,
};
use serde_json::{Map, Value};

fn event(event_id: &str) -> EventEnvelope {
    EventEnvelope {
        event_id: event_id.to_owned(),
        workspace_id: "018f51d0-9067-7a76-91a0-4da99535809a".to_owned(),
        device_id: "018f51d0-9067-7a76-91a0-4da99535809b".to_owned(),
        event_type: "system.metric_sampled".to_owned(),
        source: "system".to_owned(),
        schema_version: 1,
        occurred_at: "2026-07-31T01:02:03Z".to_owned(),
        created_at: "2026-07-31T01:02:03Z".to_owned(),
        sensitivity: Sensitivity::Normal,
        payload: Map::new(),
        attachment_refs: Vec::new(),
        idempotency_key: None,
    }
}

fn state(status: CollectorStatus) -> CollectorState {
    CollectorState {
        collector_key: "system".to_owned(),
        collector_version: "0.1.0".to_owned(),
        status,
        desired_config_revision: 0,
        applied_config_revision: 0,
        last_event_at_ms: None,
        last_health_at_ms: None,
        last_error_code: None,
        created_at_ms: 1_754_013_723_000,
        updated_at_ms: 1_754_013_723_000,
    }
}

#[test]
fn event_commit_requires_one_through_four_events() {
    assert!(EventCommit::try_new(Vec::new(), None).is_err());
    assert!(EventCommit::try_new(vec![event("1")], None).is_ok());
    assert!(EventCommit::try_new(
        (0..=MAX_EVENTS_PER_COMMIT)
            .map(|index| event(&index.to_string()))
            .collect(),
        None,
    )
    .is_err());
}

#[test]
fn event_commit_exposes_its_validated_members_read_only() {
    let expected_event = event("1");
    let expected_state = state(CollectorStatus::Running);
    let commit = EventCommit::try_new(vec![expected_event.clone()], Some(expected_state.clone()))
        .expect("valid event commit");

    assert_eq!(commit.events(), &[expected_event]);
    assert_eq!(commit.collector_state(), Some(&expected_state));
}

#[test]
fn collector_state_uses_canonical_snake_case_status() {
    let json =
        serde_json::to_value(state(CollectorStatus::Degraded)).expect("serialize collector state");

    assert_eq!(json["status"], "degraded");
    assert_eq!(json["desired_config_revision"], 0);
    assert_eq!(json["applied_config_revision"], 0);
}

#[test]
fn cpu_memory_fixture_deserializes_and_round_trips() {
    let raw =
        include_str!("../../../packages/contracts/fixtures/system-metric.cpu-memory.valid.json");
    let parsed: SystemMetricSample =
        serde_json::from_str(raw).expect("deserialize valid cpu and memory fixture");

    let SystemMetricSample::CpuMemory(sample) = &parsed else {
        panic!("expected cpu_memory metric group");
    };
    assert_eq!(sample.sample_window_ms, 30_000);
    assert_eq!(sample.logical_cpu_count, 8);
    assert!((sample.host.cpu_usage_percent - 42.5).abs() < f64::EPSILON);
    assert_eq!(sample.host.memory_total_bytes, 17_179_869_184);
    assert_eq!(sample.host.memory_used_bytes, 8_589_934_592);
    assert!((sample.agent.cpu_usage_percent - 2.5).abs() < f64::EPSILON);
    assert_eq!(sample.agent.memory_resident_bytes, 134_217_728);

    let expected: Value = serde_json::from_str(raw).expect("parse fixture as JSON");
    assert_eq!(
        serde_json::to_value(parsed).expect("serialize cpu and memory sample"),
        expected
    );
}

#[test]
fn disk_fixture_deserializes_and_round_trips() {
    let raw = include_str!("../../../packages/contracts/fixtures/system-metric.disk.valid.json");
    let parsed: SystemMetricSample =
        serde_json::from_str(raw).expect("deserialize valid disk fixture");

    let SystemMetricSample::Disk(sample) = &parsed else {
        panic!("expected disk metric group");
    };
    assert_eq!(sample.scope, DiskScope::PcaDataVolume);
    assert_eq!(sample.total_bytes, 107_374_182_400);
    assert_eq!(sample.available_bytes, 53_687_091_200);
    assert!((sample.used_percent - 50.0).abs() < f64::EPSILON);
    assert!(!sample.low_space);
    assert_eq!(sample.low_space_threshold_bytes, 2_147_483_648);
    assert_eq!(sample.warning_code, None);

    let serialized = serde_json::to_value(parsed).expect("serialize disk sample");
    assert_eq!(serialized["metric_group"], "disk");
    assert_eq!(serialized["scope"], "pca_data_volume");
    assert_eq!(serialized["total_bytes"], 107_374_182_400_u64);
    assert_eq!(serialized["available_bytes"], 53_687_091_200_u64);
    assert_eq!(serialized["used_percent"].as_f64(), Some(50.0));
    assert_eq!(serialized["low_space"], false);
    assert_eq!(serialized["low_space_threshold_bytes"], 2_147_483_648_u64);
    assert_eq!(serialized["warning_code"], Value::Null);
}

#[test]
fn invalid_percentage_fixture_is_rejected_during_deserialization() {
    let raw =
        include_str!("../../../packages/contracts/fixtures/system-metric.invalid-percent.json");

    assert!(serde_json::from_str::<SystemMetricSample>(raw).is_err());
}

#[test]
fn checked_constructors_reject_non_finite_percentages_and_inconsistent_totals() {
    assert!(HostCpuMemory::try_new(f64::NAN, 16, 8).is_err());
    assert!(HostCpuMemory::try_new(42.5, 8, 16).is_err());
    assert!(AgentCpuMemory::try_new(f64::INFINITY, 8).is_err());

    let host = HostCpuMemory::try_new(42.5, 16, 8).expect("valid host sample");
    let agent = AgentCpuMemory::try_new(2.5, 4).expect("valid agent sample");
    assert!(CpuMemorySample::try_new(0, 8, host.clone(), agent.clone()).is_err());
    assert!(CpuMemorySample::try_new(30_000, 0, host, agent).is_err());

    assert!(DiskSample::try_new(
        DiskScope::PcaDataVolume,
        100,
        101,
        0.0,
        true,
        2_147_483_648,
        Some("DISK_SPACE_LOW".to_owned()),
    )
    .is_err());
    assert!(DiskSample::try_new(
        DiskScope::PcaDataVolume,
        100,
        50,
        49.0,
        true,
        2_147_483_648,
        Some("DISK_SPACE_LOW".to_owned()),
    )
    .is_err());
}
