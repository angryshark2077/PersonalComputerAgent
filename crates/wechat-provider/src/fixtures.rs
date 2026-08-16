use std::path::PathBuf;

use pca_domain::{CommunicationAttachment, MessageKind};

use crate::{
    source::{
        GroupMembershipEvidence, LocalAccountProof, SourceCapabilities, SourceCompletedMedia,
        SourceConversation, SourceCursor, SourceDirection, SourceFinality, SourceMessageKind,
        SourceMessageRecord, SourcePayload, SourceProbeFuture, SourceReadFuture, SourceRecord,
        WechatSource,
    },
    WechatProvider,
};

pub fn fixture_provider(
    records: impl IntoIterator<Item = SourceRecord>,
) -> WechatProvider<FixtureWechatSource> {
    WechatProvider::new(FixtureWechatSource {
        records: records.into_iter().collect(),
    })
}

pub struct FixtureWechatSource {
    records: Vec<SourceRecord>,
}

impl WechatSource for FixtureWechatSource {
    fn probe(&self) -> SourceProbeFuture<'_> {
        Box::pin(async {
            Ok(SourceCapabilities {
                source_version: "fixture-v1".to_owned(),
                schema_version: 1,
            })
        })
    }

    fn read_after(&self, _: &SourceCursor) -> SourceReadFuture<'_> {
        Box::pin(async move { Ok(self.records.clone()) })
    }
}

#[must_use]
pub fn outgoing_direct_text() -> SourceRecord {
    message(
        SourceDirection::Outgoing,
        SourceMessageKind::Text,
        SourceConversation::Direct,
        SourceFinality::OutgoingSent,
        SourcePayload::Text {
            body: "sent text".to_owned(),
        },
    )
}

#[must_use]
pub fn incoming_small_group_video(member_count: u8) -> SourceRecord {
    message(
        SourceDirection::Incoming,
        SourceMessageKind::Video,
        SourceConversation::Group {
            membership: GroupMembershipEvidence::Verified(member_count),
        },
        SourceFinality::IncomingPersisted,
        SourcePayload::Media {
            attachment: Some(attachment(MessageKind::Video)),
            completed_source: Some(SourceCompletedMedia {
                attachment_id: "attachment-1".to_owned(),
                source_path: PathBuf::from("/fixture/completed-video.mp4"),
            }),
        },
    )
}

#[must_use]
pub fn missing_local_account_proof() -> SourceRecord {
    let mut record = outgoing_direct_text();
    let SourceRecord::Message(message) = &mut record else {
        unreachable!()
    };
    message.local_account = LocalAccountProof::Missing;
    record
}

#[must_use]
pub fn unknown_direction() -> SourceRecord {
    let mut record = outgoing_direct_text();
    let SourceRecord::Message(message) = &mut record else {
        unreachable!()
    };
    message.direction = SourceDirection::Unknown;
    record
}

#[must_use]
pub fn outgoing_draft() -> SourceRecord {
    let mut record = outgoing_direct_text();
    let SourceRecord::Message(message) = &mut record else {
        unreachable!()
    };
    message.finality = SourceFinality::OutgoingDraft;
    record
}

#[must_use]
pub fn outgoing_failed() -> SourceRecord {
    let mut record = outgoing_direct_text();
    let SourceRecord::Message(message) = &mut record else {
        unreachable!()
    };
    message.finality = SourceFinality::OutgoingFailed;
    record
}

#[must_use]
pub fn unsupported_type() -> SourceRecord {
    let mut record = outgoing_direct_text();
    let SourceRecord::Message(message) = &mut record else {
        unreachable!()
    };
    message.kind = SourceMessageKind::Unsupported;
    record
}

#[must_use]
pub fn group_with_unknown_member_count() -> SourceRecord {
    let mut record = outgoing_direct_text();
    let SourceRecord::Message(message) = &mut record else {
        unreachable!()
    };
    message.conversation = SourceConversation::Group {
        membership: GroupMembershipEvidence::Unknown,
    };
    record
}

#[must_use]
pub fn group_with_member_count(member_count: u8) -> SourceRecord {
    let mut record = outgoing_direct_text();
    let SourceRecord::Message(message) = &mut record else {
        unreachable!()
    };
    message.conversation = SourceConversation::Group {
        membership: GroupMembershipEvidence::Verified(member_count),
    };
    record
}

#[must_use]
pub fn group_with_unverified_member_count(member_count: u8) -> SourceRecord {
    let mut record = outgoing_direct_text();
    let SourceRecord::Message(message) = &mut record else {
        unreachable!()
    };
    message.conversation = SourceConversation::Group {
        membership: GroupMembershipEvidence::Unverified(member_count),
    };
    record
}

#[must_use]
pub fn incomplete_video() -> SourceRecord {
    message(
        SourceDirection::Incoming,
        SourceMessageKind::Video,
        SourceConversation::Direct,
        SourceFinality::IncomingPersisted,
        SourcePayload::Media {
            attachment: None,
            completed_source: None,
        },
    )
}

#[must_use]
pub fn unknown_source_record() -> SourceRecord {
    SourceRecord::Unknown
}

fn message(
    direction: SourceDirection,
    kind: SourceMessageKind,
    conversation: SourceConversation,
    finality: SourceFinality,
    payload: SourcePayload,
) -> SourceRecord {
    SourceRecord::Message(Box::new(SourceMessageRecord {
        account_id: "wechat-account-1".to_owned(),
        source_sequence: 1,
        cursor_sequence: 1,
        message_id: "message-1".to_owned(),
        conversation_id: "conversation-1".to_owned(),
        conversation_display_name: "Conversation One".to_owned(),
        conversation_avatar_url: None,
        sender_id: "wxid_sender".to_owned(),
        sender_display_name: "Sender One".to_owned(),
        sender_avatar_url: None,
        source_key: "account-1:conversation-1:1".to_owned(),
        occurred_at: "2026-08-02T00:00:00Z".to_owned(),
        local_account: LocalAccountProof::Verified,
        direction,
        kind,
        conversation,
        finality,
        payload,
    }))
}

fn attachment(kind: MessageKind) -> CommunicationAttachment {
    CommunicationAttachment::try_new(
        "attachment-1".to_owned(),
        kind,
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
        1,
        "video/mp4".to_owned(),
    )
    .expect("fixture attachment must be valid")
}
