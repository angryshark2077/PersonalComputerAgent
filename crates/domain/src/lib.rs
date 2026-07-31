#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{fmt, future::Future, pin::Pin};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Unpaired,
    Initializing,
    WaitingPermission,
    Running,
    Degraded,
    Sleeping,
    Updating,
    Repair,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollectorStatus {
    Disabled,
    PermissionRequired,
    Initializing,
    Running,
    Paused,
    Degraded,
    Unsupported,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CollectorDefinition {
    pub key: &'static str,
    pub version: &'static str,
    pub supported_event_types: &'static [&'static str],
}

/// SQLite-facing collector state owned by Agent Core.
///
/// This persistence model is deliberately not a Task 1 wire DTO:
///
/// ```compile_fail
/// use pca_domain::{CollectorState, CollectorStatus};
///
/// fn requires_wire_serialization<T: serde::Serialize>(_value: &T) {}
///
/// let state = CollectorState {
///     collector_key: "system".to_owned(),
///     collector_version: "0.1.0".to_owned(),
///     status: CollectorStatus::Running,
///     desired_config_revision: 0,
///     applied_config_revision: 0,
///     last_event_at_ms: None,
///     last_health_at_ms: None,
///     last_error_code: None,
///     created_at_ms: 0,
///     updated_at_ms: 0,
/// };
/// requires_wire_serialization(&state);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectorState {
    pub collector_key: String,
    pub collector_version: String,
    pub status: CollectorStatus,
    pub desired_config_revision: u64,
    pub applied_config_revision: u64,
    pub last_event_at_ms: Option<i64>,
    pub last_health_at_ms: Option<i64>,
    pub last_error_code: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// A checked host CPU and memory sample.
///
/// Construct samples through [`HostCpuMemory::try_new`]:
///
/// ```compile_fail
/// use pca_domain::HostCpuMemory;
///
/// let _unchecked = HostCpuMemory {
///     cpu_usage_percent: 100.1,
///     memory_total_bytes: 16,
///     memory_used_bytes: 8,
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawHostCpuMemory")]
pub struct HostCpuMemory {
    cpu_usage_percent: f64,
    memory_total_bytes: u64,
    memory_used_bytes: u64,
}

impl HostCpuMemory {
    /// Creates a host CPU and memory sample after validating its percentage and totals.
    ///
    /// # Errors
    ///
    /// Returns a domain error when CPU usage is not finite or outside 0 through 100,
    /// or when used memory exceeds total memory.
    pub fn try_new(
        cpu_usage_percent: f64,
        memory_total_bytes: u64,
        memory_used_bytes: u64,
    ) -> Result<Self, DomainError> {
        validate_percentage(cpu_usage_percent, "host cpu usage")?;
        if memory_used_bytes > memory_total_bytes {
            return Err(invalid_system_sample(
                "host memory used bytes must not exceed total bytes",
            ));
        }

        Ok(Self {
            cpu_usage_percent,
            memory_total_bytes,
            memory_used_bytes,
        })
    }

    #[must_use]
    pub const fn cpu_usage_percent(&self) -> f64 {
        self.cpu_usage_percent
    }

    #[must_use]
    pub const fn memory_total_bytes(&self) -> u64 {
        self.memory_total_bytes
    }

    #[must_use]
    pub const fn memory_used_bytes(&self) -> u64 {
        self.memory_used_bytes
    }

    fn validate(&self) -> Result<(), DomainError> {
        validate_percentage(self.cpu_usage_percent, "host cpu usage")?;
        if self.memory_used_bytes > self.memory_total_bytes {
            return Err(invalid_system_sample(
                "host memory used bytes must not exceed total bytes",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHostCpuMemory {
    cpu_usage_percent: f64,
    memory_total_bytes: u64,
    memory_used_bytes: u64,
}

impl TryFrom<RawHostCpuMemory> for HostCpuMemory {
    type Error = DomainError;

    fn try_from(raw: RawHostCpuMemory) -> Result<Self, Self::Error> {
        Self::try_new(
            raw.cpu_usage_percent,
            raw.memory_total_bytes,
            raw.memory_used_bytes,
        )
    }
}

/// A checked Agent CPU and resident-memory sample.
///
/// Construct samples through [`AgentCpuMemory::try_new`]:
///
/// ```compile_fail
/// use pca_domain::AgentCpuMemory;
///
/// let _unchecked = AgentCpuMemory {
///     cpu_usage_percent: -0.1,
///     memory_resident_bytes: 4,
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawAgentCpuMemory")]
pub struct AgentCpuMemory {
    cpu_usage_percent: f64,
    memory_resident_bytes: u64,
}

impl AgentCpuMemory {
    /// Creates an Agent CPU and memory sample after validating its percentage.
    ///
    /// # Errors
    ///
    /// Returns a domain error when CPU usage is not finite or outside 0 through 100.
    pub fn try_new(
        cpu_usage_percent: f64,
        memory_resident_bytes: u64,
    ) -> Result<Self, DomainError> {
        validate_percentage(cpu_usage_percent, "agent cpu usage")?;
        Ok(Self {
            cpu_usage_percent,
            memory_resident_bytes,
        })
    }

    #[must_use]
    pub const fn cpu_usage_percent(&self) -> f64 {
        self.cpu_usage_percent
    }

    #[must_use]
    pub const fn memory_resident_bytes(&self) -> u64 {
        self.memory_resident_bytes
    }

    fn validate(&self) -> Result<(), DomainError> {
        validate_percentage(self.cpu_usage_percent, "agent cpu usage")
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAgentCpuMemory {
    cpu_usage_percent: f64,
    memory_resident_bytes: u64,
}

impl TryFrom<RawAgentCpuMemory> for AgentCpuMemory {
    type Error = DomainError;

    fn try_from(raw: RawAgentCpuMemory) -> Result<Self, Self::Error> {
        Self::try_new(raw.cpu_usage_percent, raw.memory_resident_bytes)
    }
}

/// A checked CPU and memory metric sample.
///
/// Construct samples through [`CpuMemorySample::try_new`]:
///
/// ```compile_fail
/// use pca_domain::{AgentCpuMemory, CpuMemorySample, HostCpuMemory};
///
/// let host = HostCpuMemory::try_new(42.5, 16, 8).unwrap();
/// let agent = AgentCpuMemory::try_new(2.5, 4).unwrap();
/// let _unchecked = CpuMemorySample {
///     sample_window_ms: 0,
///     logical_cpu_count: 8,
///     host,
///     agent,
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawCpuMemorySample")]
pub struct CpuMemorySample {
    sample_window_ms: u64,
    logical_cpu_count: u32,
    host: HostCpuMemory,
    agent: AgentCpuMemory,
}

impl CpuMemorySample {
    /// Creates a checked CPU and memory sample.
    ///
    /// # Errors
    ///
    /// Returns a domain error when the sampling window or CPU count is zero, or when
    /// either nested sample violates its percentage or byte-total constraints.
    pub fn try_new(
        sample_window_ms: u64,
        logical_cpu_count: u32,
        host: HostCpuMemory,
        agent: AgentCpuMemory,
    ) -> Result<Self, DomainError> {
        if sample_window_ms == 0 {
            return Err(invalid_system_sample(
                "sample window milliseconds must be greater than zero",
            ));
        }
        if logical_cpu_count == 0 {
            return Err(invalid_system_sample(
                "logical CPU count must be greater than zero",
            ));
        }
        host.validate()?;
        agent.validate()?;

        Ok(Self {
            sample_window_ms,
            logical_cpu_count,
            host,
            agent,
        })
    }

    #[must_use]
    pub const fn sample_window_ms(&self) -> u64 {
        self.sample_window_ms
    }

    #[must_use]
    pub const fn logical_cpu_count(&self) -> u32 {
        self.logical_cpu_count
    }

    #[must_use]
    pub const fn host(&self) -> &HostCpuMemory {
        &self.host
    }

    #[must_use]
    pub const fn agent(&self) -> &AgentCpuMemory {
        &self.agent
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCpuMemorySample {
    sample_window_ms: u64,
    logical_cpu_count: u32,
    host: HostCpuMemory,
    agent: AgentCpuMemory,
}

impl TryFrom<RawCpuMemorySample> for CpuMemorySample {
    type Error = DomainError;

    fn try_from(raw: RawCpuMemorySample) -> Result<Self, Self::Error> {
        Self::try_new(
            raw.sample_window_ms,
            raw.logical_cpu_count,
            raw.host,
            raw.agent,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskScope {
    PcaDataVolume,
}

/// A checked PCA data-volume disk sample.
///
/// Construct samples through [`DiskSample::try_new`]:
///
/// ```compile_fail
/// use pca_domain::{DiskSample, DiskScope};
///
/// let _unchecked = DiskSample {
///     scope: DiskScope::PcaDataVolume,
///     total_bytes: 100,
///     available_bytes: 101,
///     used_percent: 0.0,
///     low_space: true,
///     low_space_threshold_bytes: 2_147_483_648,
///     warning_code: Some("DISK_SPACE_LOW".to_owned()),
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawDiskSample")]
pub struct DiskSample {
    scope: DiskScope,
    total_bytes: u64,
    available_bytes: u64,
    used_percent: f64,
    low_space: bool,
    low_space_threshold_bytes: u64,
    warning_code: Option<String>,
}

impl DiskSample {
    /// Creates a checked PCA data-volume disk sample.
    ///
    /// # Errors
    ///
    /// Returns a domain error when totals, derived percentage, low-space state,
    /// threshold, or warning code disagree with the canonical contract.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        scope: DiskScope,
        total_bytes: u64,
        available_bytes: u64,
        used_percent: f64,
        low_space: bool,
        low_space_threshold_bytes: u64,
        warning_code: Option<String>,
    ) -> Result<Self, DomainError> {
        const LOW_SPACE_THRESHOLD_BYTES: u64 = 2_147_483_648;

        if total_bytes == 0 {
            return Err(invalid_system_sample(
                "disk total bytes must be greater than zero",
            ));
        }
        if available_bytes > total_bytes {
            return Err(invalid_system_sample(
                "disk available bytes must not exceed total bytes",
            ));
        }
        validate_percentage(used_percent, "disk used percentage")?;
        if low_space_threshold_bytes != LOW_SPACE_THRESHOLD_BYTES {
            return Err(invalid_system_sample(
                "disk low-space threshold must be 2147483648 bytes",
            ));
        }

        let expected_used_percent = disk_used_percentage(total_bytes, available_bytes);
        if (used_percent - expected_used_percent).abs() > 0.01 {
            return Err(invalid_system_sample(
                "disk used percentage must match total and available bytes",
            ));
        }

        let expected_low_space = available_bytes < low_space_threshold_bytes;
        if low_space != expected_low_space {
            return Err(invalid_system_sample(
                "disk low-space state must match the available-byte threshold",
            ));
        }

        let expected_warning_code = low_space.then_some("DISK_SPACE_LOW");
        if warning_code.as_deref() != expected_warning_code {
            return Err(invalid_system_sample(
                "disk warning code must match the low-space state",
            ));
        }

        Ok(Self {
            scope,
            total_bytes,
            available_bytes,
            used_percent,
            low_space,
            low_space_threshold_bytes,
            warning_code,
        })
    }

    #[must_use]
    pub const fn scope(&self) -> DiskScope {
        self.scope
    }

    #[must_use]
    pub const fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    #[must_use]
    pub const fn available_bytes(&self) -> u64 {
        self.available_bytes
    }

    #[must_use]
    pub const fn used_percent(&self) -> f64 {
        self.used_percent
    }

    #[must_use]
    pub const fn low_space(&self) -> bool {
        self.low_space
    }

    #[must_use]
    pub const fn low_space_threshold_bytes(&self) -> u64 {
        self.low_space_threshold_bytes
    }

    #[must_use]
    pub fn warning_code(&self) -> Option<&str> {
        self.warning_code.as_deref()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDiskSample {
    scope: DiskScope,
    total_bytes: u64,
    available_bytes: u64,
    used_percent: f64,
    low_space: bool,
    low_space_threshold_bytes: u64,
    warning_code: Option<String>,
}

impl TryFrom<RawDiskSample> for DiskSample {
    type Error = DomainError;

    fn try_from(raw: RawDiskSample) -> Result<Self, Self::Error> {
        Self::try_new(
            raw.scope,
            raw.total_bytes,
            raw.available_bytes,
            raw.used_percent,
            raw.low_space,
            raw.low_space_threshold_bytes,
            raw.warning_code,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "metric_group", rename_all = "snake_case")]
pub enum SystemMetricSample {
    CpuMemory(CpuMemorySample),
    Disk(DiskSample),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    Public,
    Normal,
    Medium,
    High,
    Secret,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub event_id: String,
    pub workspace_id: String,
    pub device_id: String,
    pub event_type: String,
    pub source: String,
    pub schema_version: u32,
    pub occurred_at: String,
    pub created_at: String,
    pub sensitivity: Sensitivity,
    pub payload: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachment_refs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BridgeMessageKind {
    Request,
    Response,
    Event,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BridgeStatus {
    Disconnected,
    Handshaking,
    Ready,
    Degraded,
    Incompatible,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeStatusEnvelope {
    pub agent_status: AgentStatus,
    pub bridge_status: BridgeStatus,
    pub local_healthy: bool,
    pub heartbeat_at: String,
    pub process_id: u32,
    pub app_version: String,
    pub schema_version: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandshakeChallengePhase {
    Challenge,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandshakeChallenge {
    pub phase: HandshakeChallengePhase,
    pub nonce: String,
    pub agent_version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandshakeResponsePhase {
    Response,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandshakeResponse {
    pub phase: HandshakeResponsePhase,
    pub nonce: String,
    pub proof: String,
    pub bridge_version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorEnvelope {
    pub error_code: String,
    pub message: String,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BridgeEnvelope {
    pub protocol_version: u32,
    pub request_id: Uuid,
    pub message_kind: BridgeMessageKind,
    pub capability: String,
    pub deadline_ms: u64,
    pub payload: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorEnvelope>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

impl DomainError {
    #[must_use]
    pub fn new(code: &str, message: &str, retryable: bool) -> Self {
        Self {
            code: code.to_owned(),
            message: message.to_owned(),
            retryable,
        }
    }
}

impl fmt::Display for DomainError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for DomainError {}

fn invalid_system_sample(message: &str) -> DomainError {
    DomainError::new("COLLECTOR_DEGRADED", message, false)
}

fn validate_percentage(value: f64, field: &str) -> Result<(), DomainError> {
    if !value.is_finite() || !(0.0..=100.0).contains(&value) {
        return Err(invalid_system_sample(&format!(
            "{field} must be finite and between zero and 100"
        )));
    }
    Ok(())
}

#[allow(clippy::cast_precision_loss)]
fn disk_used_percentage(total_bytes: u64, available_bytes: u64) -> f64 {
    ((total_bytes - available_bytes) as f64 / total_bytes as f64) * 100.0
}

pub const MAX_EVENTS_PER_COMMIT: usize = 4;

#[derive(Debug, Clone, PartialEq)]
pub struct EventCommit {
    events: Vec<EventEnvelope>,
    collector_state: Option<CollectorState>,
}

impl EventCommit {
    /// Creates a commit that atomically persists one through four events and optional state.
    ///
    /// # Errors
    ///
    /// Returns a domain error when the commit contains no events or more than four events.
    pub fn try_new(
        events: Vec<EventEnvelope>,
        collector_state: Option<CollectorState>,
    ) -> Result<Self, DomainError> {
        if !(1..=MAX_EVENTS_PER_COMMIT).contains(&events.len()) {
            return Err(DomainError::new(
                "COLLECTOR_DEGRADED",
                "event commit must contain one through four events",
                false,
            ));
        }
        Ok(Self {
            events,
            collector_state,
        })
    }

    #[must_use]
    pub fn events(&self) -> &[EventEnvelope] {
        &self.events
    }

    #[must_use]
    pub fn collector_state(&self) -> Option<&CollectorState> {
        self.collector_state.as_ref()
    }
}

pub type EventSinkFuture<'a> = Pin<Box<dyn Future<Output = Result<(), DomainError>> + Send + 'a>>;

pub trait EventSink: Send + Sync {
    /// Persists a bounded event commit through the configured sink.
    ///
    /// # Errors
    ///
    /// Returns a domain error when the sink rejects or cannot persist the commit.
    #[allow(clippy::elidable_lifetime_names)]
    fn commit<'a>(&'a self, commit: EventCommit) -> EventSinkFuture<'a>;
}

pub trait Collector: Send {
    fn key(&self) -> &'static str;
    fn status(&self) -> CollectorStatus;

    /// Prepares the collector without starting event production.
    ///
    /// # Errors
    ///
    /// Returns a domain error when required configuration or capabilities are unavailable.
    fn initialize(&mut self) -> Result<(), DomainError>;

    /// Starts producing events through the supplied sink.
    ///
    /// # Errors
    ///
    /// Returns a domain error when startup or event delivery cannot proceed.
    fn start(&mut self, sink: &dyn EventSink) -> Result<(), DomainError>;

    /// Pauses event production while retaining collector state.
    ///
    /// # Errors
    ///
    /// Returns a domain error when the collector cannot enter the paused state.
    fn pause(&mut self) -> Result<(), DomainError>;

    /// Resumes event production from a paused state.
    ///
    /// # Errors
    ///
    /// Returns a domain error when the collector cannot resume safely.
    fn resume(&mut self) -> Result<(), DomainError>;

    /// Stops event production and releases collector resources.
    ///
    /// # Errors
    ///
    /// Returns a domain error when shutdown cannot complete cleanly.
    fn stop(&mut self) -> Result<(), DomainError>;
}
