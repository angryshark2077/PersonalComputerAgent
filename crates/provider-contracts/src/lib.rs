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

    /// Discovers the configured communication source without producing events.
    ///
    /// # Errors
    ///
    /// Returns a domain error when capability or source discovery fails.
    fn discover(&mut self) -> Result<(), DomainError>;

    /// Starts producing normalized communication events through the supplied sink.
    ///
    /// # Errors
    ///
    /// Returns a domain error when provider startup or event delivery fails.
    fn start(&mut self, sink: &dyn EventSink) -> Result<(), DomainError>;

    /// Stops the provider and releases its resources.
    ///
    /// # Errors
    ///
    /// Returns a domain error when shutdown cannot complete cleanly.
    fn stop(&mut self) -> Result<(), DomainError>;
}
