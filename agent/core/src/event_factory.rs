use crate::collector_registry::{
    CollectorIdentity, CollectorStatusTransition, DiskHealthChange, DISK_SPACE_LOW,
};
use pca_domain::{DomainError, EventEnvelope, Sensitivity, SystemMetricSample};
use serde::Serialize;
use serde_json::{Map, Value};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use uuid::Uuid;

const SCHEMA_VERSION: u32 = 1;

#[derive(Serialize)]
struct CollectorStatusPayload<'a> {
    collector_key: &'static str,
    previous_status: pca_domain::CollectorStatus,
    status: pca_domain::CollectorStatus,
    desired_config_revision: u64,
    applied_config_revision: u64,
    reason: &'a str,
    error_code: Option<&'a str>,
}

#[derive(Serialize)]
struct DiskHealthPayload {
    condition: &'static str,
    active: bool,
    error_code: &'static str,
    available_bytes: u64,
    threshold_bytes: u64,
}

pub(crate) fn metric_event(
    identity: &CollectorIdentity,
    sample: &SystemMetricSample,
    event_id: Uuid,
    occurred_at: OffsetDateTime,
    created_at: OffsetDateTime,
) -> Result<EventEnvelope, DomainError> {
    build_event(
        identity,
        event_id,
        "system.metric_sampled",
        "system",
        serialize_payload(sample)?,
        occurred_at,
        created_at,
    )
}

pub(crate) fn status_event(
    identity: &CollectorIdentity,
    transition: &CollectorStatusTransition,
    event_id: Uuid,
    occurred_at: OffsetDateTime,
    created_at: OffsetDateTime,
) -> Result<EventEnvelope, DomainError> {
    let payload = CollectorStatusPayload {
        collector_key: "system",
        previous_status: transition.previous_status,
        status: transition.status,
        desired_config_revision: transition.desired_config_revision,
        applied_config_revision: transition.applied_config_revision,
        reason: transition.reason,
        error_code: transition.error_code,
    };
    build_event(
        identity,
        event_id,
        "collector.status_changed",
        "collector.registry",
        serialize_payload(&payload)?,
        occurred_at,
        created_at,
    )
}

pub(crate) fn health_event(
    identity: &CollectorIdentity,
    change: &DiskHealthChange,
    event_id: Uuid,
    occurred_at: OffsetDateTime,
    created_at: OffsetDateTime,
) -> Result<EventEnvelope, DomainError> {
    let payload = DiskHealthPayload {
        condition: "disk_space_low",
        active: change.active,
        error_code: DISK_SPACE_LOW,
        available_bytes: change.available_bytes,
        threshold_bytes: change.threshold_bytes,
    };
    build_event(
        identity,
        event_id,
        "system.health_changed",
        "system",
        serialize_payload(&payload)?,
        occurred_at,
        created_at,
    )
}

fn build_event(
    identity: &CollectorIdentity,
    event_id: Uuid,
    event_type: &str,
    source: &str,
    payload: Map<String, Value>,
    occurred_at: OffsetDateTime,
    created_at: OffsetDateTime,
) -> Result<EventEnvelope, DomainError> {
    if identity.workspace_id.is_nil() || identity.device_id.is_nil() || event_id.is_nil() {
        return Err(DomainError::new(
            "COLLECTOR_DEGRADED",
            "collector event identity must use non-nil UUIDs",
            false,
        ));
    }
    Ok(EventEnvelope {
        event_id: event_id.to_string(),
        workspace_id: identity.workspace_id.to_string(),
        device_id: identity.device_id.to_string(),
        event_type: event_type.to_owned(),
        source: source.to_owned(),
        schema_version: SCHEMA_VERSION,
        occurred_at: format_time(occurred_at)?,
        created_at: format_time(created_at)?,
        sensitivity: Sensitivity::Normal,
        payload,
        attachment_refs: Vec::new(),
        idempotency_key: None,
    })
}

fn serialize_payload<T: Serialize>(payload: &T) -> Result<Map<String, Value>, DomainError> {
    match serde_json::to_value(payload).map_err(|_| {
        DomainError::new(
            "COLLECTOR_DEGRADED",
            "collector event payload serialization failed",
            false,
        )
    })? {
        Value::Object(object) => Ok(object),
        _ => Err(DomainError::new(
            "COLLECTOR_DEGRADED",
            "collector event payload must be a JSON object",
            false,
        )),
    }
}

fn format_time(value: OffsetDateTime) -> Result<String, DomainError> {
    value.format(&Rfc3339).map_err(|_| {
        DomainError::new(
            "COLLECTOR_DEGRADED",
            "collector event timestamp could not be formatted",
            false,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{health_event, metric_event, serialize_payload, status_event};
    use crate::collector_registry::{
        CollectorIdentity, CollectorStatusTransition, DiskHealthChange,
    };
    use pca_domain::{
        AgentCpuMemory, CollectorStatus, CpuMemorySample, HostCpuMemory, Sensitivity,
        SystemMetricSample,
    };
    use serde_json::json;
    use time::OffsetDateTime;
    use uuid::Uuid;

    fn identity() -> CollectorIdentity {
        CollectorIdentity {
            workspace_id: Uuid::parse_str("91b1d43c-f018-45e0-8cee-2c702d66d258")
                .expect("workspace UUID"),
            device_id: Uuid::parse_str("50e57743-760b-4aba-b7d1-5f4689c3efaa")
                .expect("device UUID"),
        }
    }

    fn event_id() -> Uuid {
        Uuid::parse_str("f9c0f70b-d978-42e1-b3e1-f523f85275fd").expect("event UUID")
    }

    fn occurred_at() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("occurred time")
    }

    fn created_at() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_001).expect("created time")
    }

    fn cpu_sample() -> SystemMetricSample {
        SystemMetricSample::CpuMemory(
            CpuMemorySample::try_new(
                30_000,
                8,
                HostCpuMemory::try_new(25.0, 16_000, 8_000).expect("host fixture"),
                AgentCpuMemory::try_new(2.0, 128).expect("agent fixture"),
            )
            .expect("CPU fixture"),
        )
    }

    fn assert_common_envelope(
        event: &pca_domain::EventEnvelope,
        expected_type: &str,
        expected_source: &str,
    ) {
        assert_eq!(event.event_id, event_id().to_string());
        assert_eq!(event.workspace_id, identity().workspace_id.to_string());
        assert_eq!(event.device_id, identity().device_id.to_string());
        assert_eq!(event.event_type, expected_type);
        assert_eq!(event.source, expected_source);
        assert_eq!(event.schema_version, 1);
        assert_eq!(event.occurred_at, "2023-11-14T22:13:20Z");
        assert_eq!(event.created_at, "2023-11-14T22:13:21Z");
        assert_eq!(event.sensitivity, Sensitivity::Normal);
        assert!(event.attachment_refs.is_empty());
        assert_eq!(event.idempotency_key, None);
    }

    #[test]
    fn metric_event_uses_the_strict_system_envelope_and_typed_payload() {
        let event = metric_event(
            &identity(),
            &cpu_sample(),
            event_id(),
            occurred_at(),
            created_at(),
        )
        .expect("metric event");

        assert_common_envelope(&event, "system.metric_sampled", "system");
        assert_eq!(
            serde_json::Value::Object(event.payload),
            json!({
                "metric_group": "cpu_memory",
                "sample_window_ms": 30_000,
                "logical_cpu_count": 8,
                "host": {
                    "cpu_usage_percent": 25.0,
                    "memory_total_bytes": 16_000,
                    "memory_used_bytes": 8_000
                },
                "agent": {
                    "cpu_usage_percent": 2.0,
                    "memory_resident_bytes": 128
                }
            })
        );
    }

    #[test]
    fn status_event_uses_registry_source_and_nullable_error() {
        let transition = CollectorStatusTransition {
            previous_status: CollectorStatus::Initializing,
            status: CollectorStatus::Running,
            desired_config_revision: 7,
            applied_config_revision: 6,
            reason: "initial_samples_succeeded",
            error_code: None,
        };

        let event = status_event(
            &identity(),
            &transition,
            event_id(),
            occurred_at(),
            created_at(),
        )
        .expect("status event");

        assert_common_envelope(&event, "collector.status_changed", "collector.registry");
        assert_eq!(
            serde_json::Value::Object(event.payload),
            json!({
                "collector_key": "system",
                "previous_status": "initializing",
                "status": "running",
                "desired_config_revision": 7,
                "applied_config_revision": 6,
                "reason": "initial_samples_succeeded",
                "error_code": null
            })
        );
    }

    #[test]
    fn health_event_uses_only_the_disk_condition_contract() {
        let change = DiskHealthChange {
            active: true,
            available_bytes: 1_073_741_824,
            threshold_bytes: 2_147_483_648,
        };

        let event = health_event(
            &identity(),
            &change,
            event_id(),
            occurred_at(),
            created_at(),
        )
        .expect("health event");

        assert_common_envelope(&event, "system.health_changed", "system");
        assert_eq!(
            serde_json::Value::Object(event.payload),
            json!({
                "condition": "disk_space_low",
                "active": true,
                "error_code": "DISK_SPACE_LOW",
                "available_bytes": 1_073_741_824_u64,
                "threshold_bytes": 2_147_483_648_u64
            })
        );
    }

    #[test]
    fn payload_serialization_rejects_non_object_values() {
        let error = serialize_payload(&1_u64).expect_err("scalar payload must fail");

        assert_eq!(error.code, "COLLECTOR_DEGRADED");
        assert_eq!(
            error.message,
            "collector event payload must be a JSON object"
        );
        assert!(!error.retryable);
    }
}
