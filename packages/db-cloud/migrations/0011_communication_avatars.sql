ALTER TABLE communication_conversations
    ADD COLUMN IF NOT EXISTS avatar_url text;

ALTER TABLE communication_messages
    ADD COLUMN IF NOT EXISTS sender_avatar_url text;
