use pca_domain::{ConversationScope, Direction, MessageKind};
use pca_wechat_provider::fixtures::{
    fixture_provider, group_with_member_count, group_with_unknown_member_count,
    group_with_unverified_member_count, incoming_small_group_video, incomplete_video,
    missing_local_account_proof, outgoing_direct_text, outgoing_draft, outgoing_failed,
    unknown_direction, unknown_source_record, unsupported_type,
};

#[tokio::test]
async fn emits_only_confirmed_direct_or_small_group_records() {
    let mut provider = fixture_provider([
        outgoing_direct_text(),
        incoming_small_group_video(8),
        missing_local_account_proof(),
        unknown_direction(),
        outgoing_draft(),
        outgoing_failed(),
        unsupported_type(),
        group_with_unknown_member_count(),
        group_with_member_count(9),
        incomplete_video(),
        unknown_source_record(),
    ]);

    let emitted = provider
        .poll_once()
        .await
        .expect("fixture source must read");

    assert_eq!(emitted.len(), 2);
    assert_eq!(emitted[0].direction(), Direction::Outgoing);
    assert_eq!(emitted[0].kind(), MessageKind::Text);
    assert_eq!(emitted[0].conversation(), &ConversationScope::Direct);
    assert_eq!(emitted[1].direction(), Direction::Incoming);
    assert_eq!(emitted[1].kind(), MessageKind::Video);
    assert_eq!(
        emitted[1].conversation(),
        &ConversationScope::Group { member_count: 8 }
    );
    assert!(emitted
        .iter()
        .all(|message| message.conversation().is_allowed()));
}

#[tokio::test]
async fn rejects_an_unverified_present_group_member_count() {
    let mut provider = fixture_provider([group_with_unverified_member_count(4)]);

    let emitted = provider
        .poll_once()
        .await
        .expect("fixture source must read");

    assert!(emitted.is_empty());
}
