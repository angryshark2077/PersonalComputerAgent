use std::{future::Future, pin::Pin};

use pca_domain::{CommunicationAttachment, DomainError};

pub type SourceProbeFuture<'a> =
    Pin<Box<dyn Future<Output = Result<SourceCapabilities, DomainError>> + Send + 'a>>;
pub type SourceReadFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<SourceRecord>, DomainError>> + Send + 'a>>;

/// The only source capability data this boundary needs before Task 3 adds production probing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SourceCapabilities {
    pub schema_version: u32,
}

/// An opaque position owned by later local persistence work.
#[derive(Clone, PartialEq, Eq)]
pub struct SourceCursor;

/// An untrusted record returned by a source adapter.
#[derive(Clone, PartialEq, Eq)]
pub enum SourceRecord {
    Message(SourceMessageRecord),
    Unknown,
}

/// Evidence required to normalize one source message without interpreting source-specific values.
#[derive(Clone, PartialEq, Eq)]
pub struct SourceMessageRecord {
    pub message_id: String,
    pub conversation_id: String,
    pub source_key: String,
    pub occurred_at: String,
    pub local_account: LocalAccountProof,
    pub direction: SourceDirection,
    pub kind: SourceMessageKind,
    pub conversation: SourceConversation,
    pub finality: SourceFinality,
    pub payload: SourcePayload,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LocalAccountProof {
    Verified,
    Missing,
    Unknown,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SourceDirection {
    Incoming,
    Outgoing,
    Unknown,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SourceMessageKind {
    Text,
    Audio,
    Image,
    Video,
    Unsupported,
    Unknown,
}

#[derive(Clone, PartialEq, Eq)]
pub enum SourceConversation {
    Direct,
    Group { membership: GroupMembershipEvidence },
    Unknown,
}

/// Evidence for a source-reported group size.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GroupMembershipEvidence {
    Verified(u8),
    Unverified(u8),
    Missing,
    Unknown,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SourceFinality {
    IncomingPersisted,
    OutgoingSent,
    OutgoingDraft,
    OutgoingFailed,
    Unknown,
}

#[derive(Clone, PartialEq, Eq)]
pub enum SourcePayload {
    Text {
        body: String,
    },
    Media {
        attachment: Option<CommunicationAttachment>,
    },
    Unknown,
}

/// Read-only source port. Implementations must not mutate or launch `WeChat`.
pub trait WechatSource: Send + Sync {
    fn probe(&self) -> SourceProbeFuture<'_>;
    fn read_after(&self, cursor: &SourceCursor) -> SourceReadFuture<'_>;
}
