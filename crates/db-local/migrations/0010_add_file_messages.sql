CREATE TABLE communication_messages_new (
    local_message_id INTEGER PRIMARY KEY NOT NULL,
    event_id TEXT NOT NULL UNIQUE,
    account_id TEXT NOT NULL,
    external_conversation_id TEXT NOT NULL,
    source_sequence INTEGER NOT NULL CHECK (source_sequence >= 0),
    source_key TEXT NOT NULL,
    direction TEXT NOT NULL CHECK (direction IN ('incoming', 'outgoing')),
    kind TEXT NOT NULL CHECK (kind IN ('text', 'audio', 'image', 'video', 'file')),
    occurred_at_ms INTEGER NOT NULL,
    text_body TEXT,
    created_at_ms INTEGER NOT NULL,
    UNIQUE (account_id, source_key),
    FOREIGN KEY (event_id) REFERENCES events_local(event_id) ON DELETE RESTRICT,
    FOREIGN KEY (account_id, external_conversation_id)
        REFERENCES communication_conversations(account_id, external_conversation_id)
        ON DELETE RESTRICT,
    CHECK (
        (kind = 'text' AND text_body IS NOT NULL AND length(trim(text_body)) > 0)
        OR (kind IN ('audio', 'image', 'video', 'file') AND text_body IS NULL)
    )
);

CREATE TABLE attachment_spool_new (
    attachment_id TEXT PRIMARY KEY NOT NULL,
    local_message_id INTEGER NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('audio', 'image', 'video', 'file')),
    sha256 TEXT NOT NULL CHECK (
        length(sha256) = 64 AND sha256 NOT GLOB '*[^0-9a-f]*'
    ),
    size_bytes INTEGER NOT NULL CHECK (size_bytes > 0),
    mime_type TEXT NOT NULL CHECK (length(trim(mime_type)) > 0),
    spool_relative_path TEXT NOT NULL CHECK (
        length(spool_relative_path) > 0
        AND spool_relative_path NOT LIKE '/%'
        AND spool_relative_path NOT GLOB '*..*'
    ),
    transfer_state TEXT NOT NULL DEFAULT 'pending' CHECK (
        transfer_state IN ('pending', 'prepared', 'uploading', 'completed', 'failed')
    ),
    created_at_ms INTEGER NOT NULL,
    completed_at_ms INTEGER,
    FOREIGN KEY (local_message_id) REFERENCES communication_messages_new(local_message_id)
        ON DELETE RESTRICT
);

INSERT INTO communication_messages_new SELECT * FROM communication_messages;
INSERT INTO attachment_spool_new SELECT * FROM attachment_spool;

DROP TRIGGER IF EXISTS attachment_spool_requires_deterministic_file_name;
DROP TABLE attachment_spool;
DROP TABLE communication_messages;

ALTER TABLE communication_messages_new RENAME TO communication_messages;
ALTER TABLE attachment_spool_new RENAME TO attachment_spool;

CREATE INDEX idx_communication_messages_event ON communication_messages(event_id);
CREATE INDEX idx_attachment_spool_message ON attachment_spool(local_message_id);
CREATE INDEX idx_attachment_spool_retention
    ON attachment_spool(transfer_state, completed_at_ms);

CREATE TRIGGER attachment_spool_requires_deterministic_file_name
BEFORE INSERT ON attachment_spool
FOR EACH ROW
WHEN NEW.spool_relative_path <> NEW.sha256
  OR length(NEW.spool_relative_path) <> 64
  OR NEW.spool_relative_path GLOB '*[^0-9a-f]*'
BEGIN
    SELECT RAISE(ABORT, 'attachment spool path must equal its lowercase SHA-256 filename');
END;
