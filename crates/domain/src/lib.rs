#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

pub trait EventSink: Send + Sync {
    /// Persists an event through the configured sink.
    ///
    /// # Errors
    ///
    /// Returns a domain error when the sink rejects or cannot persist the event.
    fn emit(&self, event: EventEnvelope) -> Result<(), DomainError>;
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
