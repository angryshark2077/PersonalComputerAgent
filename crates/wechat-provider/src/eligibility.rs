use pca_domain::{
    CommunicationMessageRecorded, CommunicationMessageRecordedInput, ConversationScope, Direction,
    MessageKind,
};

use crate::source::{
    LocalAccountProof, SourceConversation, SourceDirection, SourceFinality, SourceMessageKind,
    SourcePayload, SourceRecord,
};

pub(crate) fn eligible_message(record: SourceRecord) -> Option<CommunicationMessageRecorded> {
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
            member_count: Some(member_count),
        } if (1..=8).contains(&member_count) => ConversationScope::Group { member_count },
        _ => return None,
    };

    let kind = match record.kind {
        SourceMessageKind::Text => MessageKind::Text,
        SourceMessageKind::Audio => MessageKind::Audio,
        SourceMessageKind::Image => MessageKind::Image,
        SourceMessageKind::Video => MessageKind::Video,
        SourceMessageKind::Unsupported | SourceMessageKind::Unknown => return None,
    };

    let (text, attachments) = match (kind, record.payload) {
        (MessageKind::Text, SourcePayload::Text { body }) => (Some(body), Vec::new()),
        (
            media_kind,
            SourcePayload::Media {
                attachment: Some(attachment),
            },
        ) if attachment.kind() == media_kind => (None, vec![attachment]),
        _ => return None,
    };

    CommunicationMessageRecorded::try_new(CommunicationMessageRecordedInput {
        message_id: record.message_id,
        conversation_id: record.conversation_id,
        source_key: record.source_key,
        occurred_at: record.occurred_at,
        direction,
        kind,
        conversation,
        text,
        attachments,
    })
    .ok()
}
