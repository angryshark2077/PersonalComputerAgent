#![forbid(unsafe_code)]

use pca_domain::{DomainError, EventSink};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderStatus {
    Disabled,
    WaitingSource,
    CheckingStoredKey,
    PassiveScanning,
    VerifyingDatabase,
    Active,
    Degraded,
    CapabilityUnavailable,
    Unsupported,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WechatKeyType {
    RawKey,
    EncKeyPairSet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommunicationSyncMode {
    Full,
    MetadataOnly,
    LocalOnly,
}

pub trait CommunicationProvider: Send {
    fn key(&self) -> &'static str;
    fn status(&self) -> ProviderStatus;
    fn discover(&mut self) -> Result<(), DomainError>;
    fn start(&mut self, sink: &dyn EventSink) -> Result<(), DomainError>;
    fn stop(&mut self) -> Result<(), DomainError>;
}
