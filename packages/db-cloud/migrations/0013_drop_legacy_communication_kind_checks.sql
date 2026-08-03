ALTER TABLE communication_messages
    DROP CONSTRAINT IF EXISTS communication_messages_kind_check;

ALTER TABLE communication_message_attachments
    DROP CONSTRAINT IF EXISTS communication_message_attachments_kind_check;
