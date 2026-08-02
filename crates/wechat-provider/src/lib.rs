#![deny(unsafe_code)]

mod eligibility;
pub mod fixtures;
#[cfg(target_os = "macos")]
mod production;
pub mod source;
pub mod sqlcipher_source;

#[cfg(target_os = "macos")]
pub use production::MacOSWechatProviderFactory;

use pca_domain::DomainError;
use pca_provider_contracts::{
    CommunicationPollFuture, CommunicationProvider, CommunicationProviderFuture,
    NormalizedCommunicationRecord, ProviderStatus,
};

use crate::{
    eligibility::eligible_message,
    source::{SourceCursor, WechatSource},
};

/// A read-only, source-agnostic `WeChat` Provider.
///
/// It only normalizes records proven eligible by its source. Persistence, attachment spooling,
/// Cloud synchronization, and source probing implementations are deliberately outside this crate.
pub struct WechatProvider<S> {
    source: S,
    cursor: SourceCursor,
    status: ProviderStatus,
}

impl<S> WechatProvider<S>
where
    S: WechatSource,
{
    #[must_use]
    pub fn new(source: S) -> Self {
        Self {
            source,
            cursor: SourceCursor,
            status: ProviderStatus::WaitingSource,
        }
    }

    /// Reads one source batch and returns only communication records with complete evidence.
    ///
    /// # Errors
    ///
    /// Returns a domain error when the source cannot be read. Ineligible source records are
    /// omitted without retaining their body, path, or media data.
    pub async fn poll_once(&mut self) -> Result<Vec<NormalizedCommunicationRecord>, DomainError> {
        let records = self.source.read_after(&self.cursor).await?;
        self.status = ProviderStatus::Active;
        Ok(records.into_iter().filter_map(eligible_message).collect())
    }
}

impl<S> CommunicationProvider for WechatProvider<S>
where
    S: WechatSource,
{
    fn key(&self) -> &'static str {
        "communication.wechat"
    }

    fn status(&self) -> ProviderStatus {
        self.status
    }

    fn discover(&mut self) -> CommunicationProviderFuture<'_> {
        Box::pin(async move {
            self.status = ProviderStatus::VerifyingDatabase;
            self.source.probe().await?;
            self.status = ProviderStatus::PassiveScanning;
            Ok(())
        })
    }

    fn poll_once(&mut self) -> CommunicationPollFuture<'_> {
        Box::pin(WechatProvider::poll_once(self))
    }

    fn stop(&mut self) -> Result<(), DomainError> {
        self.status = ProviderStatus::Disabled;
        Ok(())
    }
}
