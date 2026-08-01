#![forbid(unsafe_code)]

use std::{future::Future, pin::Pin};

use pca_domain::{CommunicationMessageRecorded, DomainError};

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

pub type CommunicationProviderFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), DomainError>> + Send + 'a>>;

pub type CommunicationPollFuture<'a> = Pin<
    Box<dyn Future<Output = Result<Vec<CommunicationMessageRecorded>, DomainError>> + Send + 'a>,
>;

pub trait CommunicationProvider: Send {
    fn key(&self) -> &'static str;
    fn status(&self) -> ProviderStatus;

    /// Discovers the configured communication source without producing events.
    ///
    /// # Errors
    ///
    /// Returns a domain error when capability or source discovery fails.
    fn discover(&mut self) -> CommunicationProviderFuture<'_>;

    /// Reads one bounded batch of normalized communication records from the source.
    ///
    /// # Errors
    ///
    /// Returns a domain error when the source cannot be read. The provider does not persist or
    /// upload returned records.
    fn poll_once(&mut self) -> CommunicationPollFuture<'_>;

    /// Stops the provider and releases its resources.
    ///
    /// # Errors
    ///
    /// Returns a domain error when shutdown cannot complete cleanly.
    fn stop(&mut self) -> Result<(), DomainError>;
}
