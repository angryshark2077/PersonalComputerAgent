#![forbid(unsafe_code)]

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sensitivity {
    Public,
    Normal,
    Medium,
    High,
    Secret,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
    pub payload_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

pub trait EventSink: Send + Sync {
    fn emit(&self, event: EventEnvelope) -> Result<(), DomainError>;
}

pub trait Collector: Send {
    fn key(&self) -> &'static str;
    fn status(&self) -> CollectorStatus;
    fn initialize(&mut self) -> Result<(), DomainError>;
    fn start(&mut self, sink: &dyn EventSink) -> Result<(), DomainError>;
    fn pause(&mut self) -> Result<(), DomainError>;
    fn resume(&mut self) -> Result<(), DomainError>;
    fn stop(&mut self) -> Result<(), DomainError>;
}
