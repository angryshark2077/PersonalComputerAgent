ALTER TABLE collector_configs
    ADD COLUMN IF NOT EXISTS messages_enabled boolean NOT NULL DEFAULT false,
    ADD COLUMN IF NOT EXISTS photos_enabled boolean NOT NULL DEFAULT false;

ALTER TABLE communication_events
    DROP CONSTRAINT IF EXISTS communication_events_source_check;
ALTER TABLE communication_events
    ADD CONSTRAINT communication_events_source_check
    CHECK (source IN ('communication.wechat', 'communication.messages'));

ALTER TABLE communication_conversations
    DROP CONSTRAINT IF EXISTS communication_conversations_scope_members;
ALTER TABLE communication_conversations
    ADD CONSTRAINT communication_conversations_scope_members
    CHECK (
        (scope = 'direct' AND member_count IS NULL)
        OR (scope = 'group' AND member_count BETWEEN 1 AND 255)
    );
