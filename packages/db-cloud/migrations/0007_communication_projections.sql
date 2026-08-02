CREATE TABLE IF NOT EXISTS communication_conversations (
    workspace_id uuid NOT NULL,
    device_id uuid NOT NULL,
    conversation_id text NOT NULL,
    scope text NOT NULL CHECK (scope IN ('direct', 'group')),
    member_count integer,
    last_message_at timestamptz NOT NULL,
    PRIMARY KEY (workspace_id, device_id, conversation_id),
    FOREIGN KEY (workspace_id, device_id)
        REFERENCES devices(workspace_id, id) ON DELETE CASCADE,
    CHECK (
        (scope = 'direct' AND member_count IS NULL)
        OR (scope = 'group' AND member_count BETWEEN 1 AND 8)
    )
);

CREATE TABLE IF NOT EXISTS communication_messages (
    event_id uuid PRIMARY KEY REFERENCES communication_events(event_id) ON DELETE CASCADE,
    workspace_id uuid NOT NULL,
    device_id uuid NOT NULL,
    conversation_id text NOT NULL,
    message_id text NOT NULL,
    source_key text NOT NULL,
    occurred_at timestamptz NOT NULL,
    direction text NOT NULL CHECK (direction IN ('incoming', 'outgoing')),
    kind text NOT NULL CHECK (kind IN ('text', 'audio', 'image', 'video')),
    text_body text,
    FOREIGN KEY (workspace_id, device_id, conversation_id)
        REFERENCES communication_conversations(workspace_id, device_id, conversation_id) ON DELETE CASCADE,
    UNIQUE (workspace_id, device_id, source_key),
    UNIQUE (workspace_id, device_id, message_id),
    CHECK (
        (kind = 'text' AND text_body IS NOT NULL)
        OR (kind <> 'text' AND text_body IS NULL)
    )
);

CREATE INDEX IF NOT EXISTS idx_communication_messages_device_conversation_chronology
    ON communication_messages(workspace_id, device_id, conversation_id, occurred_at DESC);

CREATE TABLE IF NOT EXISTS communication_message_attachments (
    event_id uuid NOT NULL REFERENCES communication_messages(event_id) ON DELETE CASCADE,
    attachment_id text NOT NULL,
    kind text NOT NULL CHECK (kind IN ('audio', 'image', 'video')),
    sha256 char(64) NOT NULL CHECK (sha256 ~ '^[a-f0-9]{64}$'),
    size_bytes bigint NOT NULL CHECK (size_bytes > 0),
    mime_type text NOT NULL,
    PRIMARY KEY (event_id, attachment_id)
);

INSERT INTO communication_conversations (
    workspace_id,
    device_id,
    conversation_id,
    scope,
    member_count,
    last_message_at
)
SELECT DISTINCT ON (workspace_id, device_id, payload ->> 'conversation_id')
    workspace_id,
    device_id,
    payload ->> 'conversation_id',
    payload -> 'conversation' ->> 'scope',
    CASE
        WHEN payload -> 'conversation' ->> 'scope' = 'group'
            THEN (payload -> 'conversation' ->> 'member_count')::integer
        ELSE NULL
    END,
    occurred_at
FROM communication_events
ORDER BY
    workspace_id,
    device_id,
    payload ->> 'conversation_id',
    occurred_at DESC,
    event_id DESC
ON CONFLICT (workspace_id, device_id, conversation_id) DO UPDATE
SET
    member_count = CASE
        WHEN EXCLUDED.last_message_at >= communication_conversations.last_message_at
            THEN EXCLUDED.member_count
        ELSE communication_conversations.member_count
    END,
    last_message_at = GREATEST(
        communication_conversations.last_message_at,
        EXCLUDED.last_message_at
    );

INSERT INTO communication_messages (
    event_id,
    workspace_id,
    device_id,
    conversation_id,
    message_id,
    source_key,
    occurred_at,
    direction,
    kind,
    text_body
)
SELECT
    event_id,
    workspace_id,
    device_id,
    payload ->> 'conversation_id',
    payload ->> 'message_id',
    payload ->> 'source_key',
    occurred_at,
    payload ->> 'direction',
    payload ->> 'kind',
    CASE WHEN payload ->> 'kind' = 'text' THEN payload ->> 'text' ELSE NULL END
FROM communication_events
ON CONFLICT DO NOTHING;

INSERT INTO communication_message_attachments (
    event_id,
    attachment_id,
    kind,
    sha256,
    size_bytes,
    mime_type
)
SELECT
    event.event_id,
    attachment.value ->> 'attachment_id',
    attachment.value ->> 'kind',
    attachment.value ->> 'sha256',
    (attachment.value ->> 'size_bytes')::bigint,
    attachment.value ->> 'mime_type'
FROM communication_events AS event
INNER JOIN communication_messages AS message ON message.event_id = event.event_id
CROSS JOIN LATERAL jsonb_array_elements(COALESCE(event.payload -> 'attachments', '[]'::jsonb)) AS attachment(value)
ON CONFLICT DO NOTHING;
