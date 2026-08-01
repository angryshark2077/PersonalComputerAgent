CREATE TABLE IF NOT EXISTS communication_conversations (
    account_id TEXT NOT NULL,
    external_conversation_id TEXT NOT NULL,
    scope TEXT NOT NULL CHECK (scope IN ('direct', 'group')),
    member_count INTEGER CHECK (
        (scope = 'direct' AND member_count IS NULL)
        OR (scope = 'group' AND member_count BETWEEN 1 AND 8)
    ),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (account_id, external_conversation_id)
);

CREATE TABLE IF NOT EXISTS communication_messages (
    local_message_id INTEGER PRIMARY KEY NOT NULL,
    event_id TEXT NOT NULL UNIQUE,
    account_id TEXT NOT NULL,
    external_conversation_id TEXT NOT NULL,
    source_sequence INTEGER NOT NULL CHECK (source_sequence >= 0),
    source_key TEXT NOT NULL,
    direction TEXT NOT NULL CHECK (direction IN ('incoming', 'outgoing')),
    kind TEXT NOT NULL CHECK (kind IN ('text', 'audio', 'image', 'video')),
    occurred_at_ms INTEGER NOT NULL,
    text_body TEXT,
    created_at_ms INTEGER NOT NULL,
    UNIQUE (account_id, external_conversation_id, source_sequence),
    UNIQUE (account_id, source_key),
    FOREIGN KEY (event_id) REFERENCES events_local(event_id) ON DELETE RESTRICT,
    FOREIGN KEY (account_id, external_conversation_id)
        REFERENCES communication_conversations(account_id, external_conversation_id)
        ON DELETE RESTRICT,
    CHECK (
        (kind = 'text' AND text_body IS NOT NULL AND length(trim(text_body)) > 0)
        OR (kind IN ('audio', 'image', 'video') AND text_body IS NULL)
    )
);

CREATE TABLE IF NOT EXISTS communication_cursors (
    account_id TEXT NOT NULL,
    external_conversation_id TEXT NOT NULL,
    last_source_sequence INTEGER NOT NULL CHECK (last_source_sequence >= 0),
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (account_id, external_conversation_id),
    FOREIGN KEY (account_id, external_conversation_id)
        REFERENCES communication_conversations(account_id, external_conversation_id)
        ON DELETE RESTRICT
);

CREATE TABLE IF NOT EXISTS attachment_spool (
    attachment_id TEXT PRIMARY KEY NOT NULL,
    local_message_id INTEGER NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('audio', 'image', 'video')),
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
    FOREIGN KEY (local_message_id) REFERENCES communication_messages(local_message_id)
        ON DELETE RESTRICT
);

CREATE TABLE IF NOT EXISTS local_tombstones (
    account_id TEXT NOT NULL,
    source_key TEXT NOT NULL,
    tombstoned_at_ms INTEGER NOT NULL,
    PRIMARY KEY (account_id, source_key)
);

CREATE INDEX IF NOT EXISTS idx_communication_messages_event
    ON communication_messages(event_id);
CREATE INDEX IF NOT EXISTS idx_attachment_spool_message
    ON attachment_spool(local_message_id);
