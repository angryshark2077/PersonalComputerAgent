use pca_domain::{
    CommunicationAttachment, CommunicationMessageRecorded, CommunicationMessageRecordedInput,
    ConversationScope, Direction, MessageKind,
};
use serde_json::json;

fn attachment(kind: MessageKind) -> CommunicationAttachment {
    serde_json::from_value(json!({
        "attachment_id": "attachment-1",
        "kind": kind,
        "sha256": "a".repeat(64),
        "size_bytes": 1,
        "mime_type": "image/png"
    }))
    .expect("complete attachment manifest")
}

fn valid_group_message(member_count: u8) -> CommunicationMessageRecordedInput {
    CommunicationMessageRecordedInput {
        message_id: "message-1".to_owned(),
        conversation_id: "conversation-1".to_owned(),
        source_key: "account-1:conversation-1:message-1".to_owned(),
        occurred_at: "2026-08-02T12:00:00Z".to_owned(),
        direction: Direction::Incoming,
        kind: MessageKind::Image,
        conversation: ConversationScope::Group { member_count },
        text: None,
        attachments: vec![attachment(MessageKind::Image)],
    }
}

#[test]
fn accepts_direct_text_and_small_group_media_only() {
    let text = CommunicationMessageRecorded::try_new(CommunicationMessageRecordedInput {
        message_id: "message-1".to_owned(),
        conversation_id: "conversation-1".to_owned(),
        source_key: "account-1:conversation-1:message-1".to_owned(),
        occurred_at: "2026-08-02T12:00:00Z".to_owned(),
        direction: Direction::Outgoing,
        kind: MessageKind::Text,
        conversation: ConversationScope::Direct,
        text: Some("sent text".to_owned()),
        attachments: Vec::new(),
    });

    assert!(text.is_ok());
    assert!(CommunicationMessageRecorded::try_new(valid_group_message(8)).is_ok());
}

#[test]
fn rejects_group_larger_than_eight_and_unknown_attachment_fields() {
    assert!(CommunicationMessageRecorded::try_new(valid_group_message(9)).is_err());
    assert!(serde_json::from_value::<CommunicationAttachment>(json!({
        "attachment_id": "a",
        "kind": "image",
        "sha256": "a".repeat(64),
        "size_bytes": 1,
        "mime_type": "image/png",
        "extra": true
    }))
    .is_err());
}

#[test]
fn rejects_empty_text_incomplete_media_and_mismatched_attachment_kind() {
    let mut empty_text = valid_group_message(8);
    empty_text.kind = MessageKind::Text;
    empty_text.text = Some(" ".to_owned());
    empty_text.attachments = Vec::new();
    assert!(CommunicationMessageRecorded::try_new(empty_text).is_err());

    let mut no_media = valid_group_message(8);
    no_media.attachments = Vec::new();
    assert!(CommunicationMessageRecorded::try_new(no_media).is_err());

    let mut mismatched_media = valid_group_message(8);
    mismatched_media.attachments = vec![attachment(MessageKind::Video)];
    assert!(CommunicationMessageRecorded::try_new(mismatched_media).is_err());
}

#[test]
fn debug_output_redacts_message_bodies_and_source_metadata() {
    let body = "message-body-secret";
    let conversation_display_name = "conversation-display-name-secret";
    let source_path = "/private/wechat/source-path-secret";
    let attachment_payload = "attachment-payload-secret";
    let text_input = CommunicationMessageRecordedInput {
        message_id: "message-1".to_owned(),
        conversation_id: conversation_display_name.to_owned(),
        source_key: source_path.to_owned(),
        occurred_at: "2026-08-02T12:00:00Z".to_owned(),
        direction: Direction::Incoming,
        kind: MessageKind::Text,
        conversation: ConversationScope::Direct,
        text: Some(body.to_owned()),
        attachments: Vec::new(),
    };
    let media_input = CommunicationMessageRecordedInput {
        message_id: "message-2".to_owned(),
        conversation_id: conversation_display_name.to_owned(),
        source_key: source_path.to_owned(),
        occurred_at: "2026-08-02T12:00:00Z".to_owned(),
        direction: Direction::Outgoing,
        kind: MessageKind::Image,
        conversation: ConversationScope::Direct,
        text: None,
        attachments: vec![serde_json::from_value(json!({
            "attachment_id": attachment_payload,
            "kind": "image",
            "sha256": "a".repeat(64),
            "size_bytes": 1,
            "mime_type": "image/payload-secret"
        }))
        .expect("complete attachment manifest")],
    };

    let text_record =
        CommunicationMessageRecorded::try_new(text_input.clone()).expect("valid text record");
    let media_record =
        CommunicationMessageRecorded::try_new(media_input.clone()).expect("valid media record");

    for output in [
        format!("{text_input:?}"),
        format!("{media_input:?}"),
        format!("{text_record:?}"),
        format!("{media_record:?}"),
    ] {
        for sensitive_value in [
            body,
            conversation_display_name,
            source_path,
            attachment_payload,
            "image/payload-secret",
        ] {
            assert!(
                !output.contains(sensitive_value),
                "Debug leaked {sensitive_value}"
            );
        }
    }
}
