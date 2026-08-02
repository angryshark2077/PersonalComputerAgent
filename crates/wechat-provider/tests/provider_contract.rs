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
        incoming_small_group_video(15),
        missing_local_account_proof(),
        unknown_direction(),
        outgoing_draft(),
        outgoing_failed(),
        unsupported_type(),
        group_with_unknown_member_count(),
        group_with_member_count(16),
        incomplete_video(),
        unknown_source_record(),
    ]);

    let emitted = provider
        .poll_once()
        .await
        .expect("fixture source must read");

    assert_eq!(emitted.len(), 2);
    assert_eq!(emitted[0].account_id(), "wechat-account-1");
    assert_eq!(emitted[0].source_sequence(), 1);
    assert_eq!(emitted[0].message().direction(), Direction::Outgoing);
    assert_eq!(emitted[0].message().kind(), MessageKind::Text);
    assert_eq!(
        emitted[0].message().conversation(),
        &ConversationScope::Direct
    );
    assert!(emitted[0].completed_media().is_empty());
    assert_eq!(emitted[1].message().direction(), Direction::Incoming);
    assert_eq!(emitted[1].message().kind(), MessageKind::Video);
    assert_eq!(
        emitted[1].message().conversation(),
        &ConversationScope::Group { member_count: 15 }
    );
    assert_eq!(emitted[1].completed_media().len(), 1);
    assert!(emitted
        .iter()
        .all(|record| record.message().conversation().is_allowed()));
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
