#![forbid(unsafe_code)]

use std::{collections::HashSet, future::Future, path::PathBuf, pin::Pin};

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
    Box<dyn Future<Output = Result<Vec<NormalizedCommunicationRecord>, DomainError>> + Send + 'a>,
>;

/// A provider-confirmed immutable source file for one validated media manifest.
///
/// This type deliberately has no `Debug` implementation because the path is private source data.
#[derive(Clone, PartialEq, Eq)]
pub struct CompletedMediaSource {
    attachment_id: String,
    source_path: PathBuf,
}

impl CompletedMediaSource {
    /// Creates a completed-media descriptor without interpreting the source path in Agent Core.
    ///
    /// # Errors
    ///
    /// Returns a redacted record error when the identifier is empty or the path is not absolute.
    pub fn try_new(attachment_id: String, source_path: PathBuf) -> Result<Self, DomainError> {
        if attachment_id.trim().is_empty()
            || attachment_id.len() > 512
            || attachment_id.chars().any(char::is_control)
            || !source_path.is_absolute()
        {
            return Err(invalid_record());
        }
        Ok(Self {
            attachment_id,
            source_path,
        })
    }

    #[must_use]
    pub fn attachment_id(&self) -> &str {
        &self.attachment_id
    }

    #[must_use]
    pub fn source_path(&self) -> &std::path::Path {
        &self.source_path
    }
}

/// One provider-normalized record ready for Agent persistence.
///
/// It contains no provider-private schema types and intentionally has no `Debug` implementation.
#[derive(Clone, PartialEq, Eq)]
pub struct NormalizedCommunicationRecord {
    account_id: String,
    source_sequence: u64,
    conversation_display_name: String,
    conversation_avatar_url: Option<String>,
    sender_avatar_url: Option<String>,
    message: CommunicationMessageRecorded,
    completed_media: Vec<CompletedMediaSource>,
}

impl NormalizedCommunicationRecord {
    /// Validates the opaque source position and exact completed-media mapping.
    ///
    /// # Errors
    ///
    /// Returns a redacted record error for an invalid account, zero sequence, or incomplete or
    /// duplicated media descriptors.
    pub fn try_new(
        account_id: String,
        source_sequence: u64,
        conversation_display_name: String,
        message: CommunicationMessageRecorded,
        completed_media: Vec<CompletedMediaSource>,
    ) -> Result<Self, DomainError> {
        if account_id.trim().is_empty()
            || account_id.len() > 512
            || account_id.chars().any(char::is_control)
            || source_sequence == 0
            || conversation_display_name.trim().is_empty()
            || conversation_display_name.len() > 1024
            || conversation_display_name.chars().any(char::is_control)
        {
            return Err(invalid_record());
        }
        let expected = message
            .attachments()
            .iter()
            .map(pca_domain::CommunicationAttachment::attachment_id)
            .collect::<HashSet<_>>();
        let actual = completed_media
            .iter()
            .map(CompletedMediaSource::attachment_id)
            .collect::<HashSet<_>>();
        if expected.len() != message.attachments().len()
            || actual.len() != completed_media.len()
            || expected != actual
        {
            return Err(invalid_record());
        }
        Ok(Self {
            account_id,
            source_sequence,
            conversation_display_name,
            conversation_avatar_url: None,
            sender_avatar_url: None,
            message,
            completed_media,
        })
    }

    /// Attaches optional presentation metadata without changing the immutable message payload.
    ///
    /// # Errors
    ///
    /// Returns a redacted record error when either URL is not a bounded HTTPS URL.
    pub fn with_avatar_metadata(
        mut self,
        conversation_avatar_url: Option<String>,
        sender_avatar_url: Option<String>,
    ) -> Result<Self, DomainError> {
        if conversation_avatar_url
            .as_deref()
            .is_some_and(|value| !valid_avatar_url(value))
            || sender_avatar_url
                .as_deref()
                .is_some_and(|value| !valid_avatar_url(value))
        {
            return Err(invalid_record());
        }
        self.conversation_avatar_url = conversation_avatar_url;
        self.sender_avatar_url = sender_avatar_url;
        Ok(self)
    }

    #[must_use]
    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    #[must_use]
    pub const fn source_sequence(&self) -> u64 {
        self.source_sequence
    }

    #[must_use]
    pub fn conversation_display_name(&self) -> &str {
        &self.conversation_display_name
    }

    #[must_use]
    pub fn conversation_avatar_url(&self) -> Option<&str> {
        self.conversation_avatar_url.as_deref()
    }

    #[must_use]
    pub fn sender_avatar_url(&self) -> Option<&str> {
        self.sender_avatar_url.as_deref()
    }

    #[must_use]
    pub const fn message(&self) -> &CommunicationMessageRecorded {
        &self.message
    }

    #[must_use]
    pub fn completed_media(&self) -> &[CompletedMediaSource] {
        &self.completed_media
    }

    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        String,
        u64,
        String,
        CommunicationMessageRecorded,
        Vec<CompletedMediaSource>,
    ) {
        (
            self.account_id,
            self.source_sequence,
            self.conversation_display_name,
            self.message,
            self.completed_media,
        )
    }
}

fn valid_avatar_url(value: &str) -> bool {
    value.starts_with("https://") && value.len() <= 4096 && !value.chars().any(char::is_control)
}

/// Creates fresh Provider instances for retries and control revision transitions.
pub trait CommunicationProviderFactory: Send + Sync {
    /// Returns a new provider or a redacted `WECHAT_*` capability error.
    ///
    /// # Errors
    ///
    /// Returns a domain error when no verified provider can be constructed.
    fn create(&self) -> Result<Box<dyn CommunicationProvider>, DomainError>;
}

pub trait CommunicationProvider: Send {
    fn key(&self) -> &'static str;
    fn status(&self) -> ProviderStatus;

    /// Returns a redacted partial-degradation reason from the most recent successful poll.
    ///
    /// A provider may return records and still report that one optional source stage failed.
    fn health_error(&self) -> Option<DomainError> {
        None
    }

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

fn invalid_record() -> DomainError {
    DomainError::new(
        "COMMUNICATION_INVALID_RECORD",
        "communication provider record is invalid",
        false,
    )
}
