ALTER TABLE communication_messages
    DROP CONSTRAINT IF EXISTS communication_messages_kind,
    DROP CONSTRAINT IF EXISTS communication_messages_text_body;

ALTER TABLE communication_messages
    ADD CONSTRAINT communication_messages_kind
        CHECK (kind IN ('text', 'audio', 'image', 'video', 'file')),
    ADD CONSTRAINT communication_messages_text_body
        CHECK (
            (kind = 'text' AND text_body IS NOT NULL)
            OR (kind <> 'text' AND text_body IS NULL)
        );

ALTER TABLE communication_message_attachments
    DROP CONSTRAINT IF EXISTS communication_message_attachments_kind;

ALTER TABLE communication_message_attachments
    ADD CONSTRAINT communication_message_attachments_kind
        CHECK (kind IN ('audio', 'image', 'video', 'file'));

ALTER TABLE communication_message_attachments
    ADD COLUMN file_name text;
