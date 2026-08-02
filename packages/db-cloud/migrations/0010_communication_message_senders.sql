ALTER TABLE communication_events
    DROP CONSTRAINT IF EXISTS communication_events_event_type_check;
ALTER TABLE communication_events
    ADD CONSTRAINT communication_events_event_type_check
    CHECK (event_type IN (
        'communication.message_recorded',
        'communication.conversation_observed',
        'communication.message_sender_observed'
    ));

ALTER TABLE communication_messages
    ADD COLUMN sender_id text NOT NULL DEFAULT '';
ALTER TABLE communication_messages
    ADD COLUMN sender_display_name text NOT NULL DEFAULT '';
