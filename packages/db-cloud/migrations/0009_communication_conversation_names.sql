ALTER TABLE communication_events
    DROP CONSTRAINT IF EXISTS communication_events_event_type_check;
ALTER TABLE communication_events
    ADD CONSTRAINT communication_events_event_type_check
    CHECK (event_type IN (
        'communication.message_recorded',
        'communication.conversation_observed'
    ));

ALTER TABLE communication_conversations
    ADD COLUMN display_name text NOT NULL DEFAULT '';
UPDATE communication_conversations
SET display_name = conversation_id
WHERE display_name = '';

ALTER TABLE communication_conversations
    DROP CONSTRAINT IF EXISTS communication_conversations_check;
ALTER TABLE communication_conversations
    DROP CONSTRAINT IF EXISTS communication_conversations_scope_members;
ALTER TABLE communication_conversations
    ADD CONSTRAINT communication_conversations_scope_members
    CHECK (
        (scope = 'direct' AND member_count IS NULL)
        OR (scope = 'group' AND member_count BETWEEN 1 AND 15)
    );
