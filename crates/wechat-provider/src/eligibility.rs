use pca_domain::{
    CommunicationMessageRecorded, CommunicationMessageRecordedInput, ConversationScope, Direction,
    MessageKind,
};
use pca_provider_contracts::{CompletedMediaSource, NormalizedCommunicationRecord};

use crate::source::{
    GroupMembershipEvidence, LocalAccountProof, SourceConversation, SourceDirection,
    SourceFinality, SourceMessageKind, SourcePayload, SourceRecord,
};

pub(crate) fn eligible_message(record: SourceRecord) -> Option<NormalizedCommunicationRecord> {
    let SourceRecord::Message(record) = record else {
        return None;
    };

    if record.local_account != LocalAccountProof::Verified {
        return None;
    }

    let direction = match record.direction {
        SourceDirection::Incoming if record.finality == SourceFinality::IncomingPersisted => {
            Direction::Incoming
        }
        SourceDirection::Outgoing if record.finality == SourceFinality::OutgoingSent => {
            Direction::Outgoing
        }
        _ => return None,
    };

    let conversation = match record.conversation {
        SourceConversation::Direct => ConversationScope::Direct,
        SourceConversation::Group {
            membership: GroupMembershipEvidence::Verified(member_count),
        } if (1..=15).contains(&member_count) => ConversationScope::Group { member_count },
        _ => return None,
    };

    let kind = match record.kind {
        SourceMessageKind::Text => MessageKind::Text,
        SourceMessageKind::Audio => MessageKind::Audio,
        SourceMessageKind::Image => MessageKind::Image,
        SourceMessageKind::Video => MessageKind::Video,
        SourceMessageKind::Unsupported | SourceMessageKind::Unknown => return None,
    };

    let (text, attachments, completed_media) = match (kind, record.payload) {
        (MessageKind::Text, SourcePayload::Text { body }) => (Some(body), Vec::new(), Vec::new()),
        (
            media_kind,
            SourcePayload::Media {
                attachment: Some(attachment),
                completed_source: Some(completed_source),
            },
        ) if attachment.kind() == media_kind
            && completed_source.attachment_id == attachment.attachment_id() =>
        {
            let source = CompletedMediaSource::try_new(
                completed_source.attachment_id,
                completed_source.source_path,
            )
            .ok()?;
            (None, vec![attachment], vec![source])
        }
        _ => return None,
    };

    let message = CommunicationMessageRecorded::try_new(CommunicationMessageRecordedInput {
        message_id: record.message_id,
        conversation_id: record.conversation_id,
        sender_id: record.sender_id,
        sender_display_name: record.sender_display_name,
        source_key: record.source_key,
        occurred_at: record.occurred_at,
        direction,
        kind,
        conversation,
        text,
        attachments,
    })
    .ok()?;
    NormalizedCommunicationRecord::try_new(
        record.account_id,
        record.source_sequence,
        record.conversation_display_name,
        message,
        completed_media,
    )
    .ok()
}
