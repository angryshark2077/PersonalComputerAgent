CREATE UNIQUE INDEX IF NOT EXISTS communication_messages_workspace_device_event_unique
    ON communication_messages(workspace_id, device_id, event_id);

CREATE TABLE IF NOT EXISTS communication_objects (
    object_id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    device_id uuid NOT NULL,
    event_id uuid NOT NULL,
    attachment_id text NOT NULL,
    object_key text NOT NULL UNIQUE CHECK (object_key ~ '^communication/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'),
    expected_sha256 char(64) NOT NULL CHECK (expected_sha256 ~ '^[a-f0-9]{64}$'),
    expected_size_bytes bigint NOT NULL CHECK (expected_size_bytes > 0),
    expected_mime_type text NOT NULL,
    state text NOT NULL CHECK (state IN ('prepared', 'completed')),
    prepared_at timestamptz NOT NULL,
    completed_at timestamptz,
    FOREIGN KEY (workspace_id, device_id, event_id)
        REFERENCES communication_messages(workspace_id, device_id, event_id) ON DELETE CASCADE,
    FOREIGN KEY (event_id, attachment_id)
        REFERENCES communication_message_attachments(event_id, attachment_id) ON DELETE CASCADE,
    UNIQUE (event_id, attachment_id),
    CHECK (
        (state = 'prepared' AND completed_at IS NULL)
        OR (state = 'completed' AND completed_at IS NOT NULL)
    )
);

CREATE INDEX IF NOT EXISTS idx_communication_objects_owner
    ON communication_objects(workspace_id, device_id, object_id)
    WHERE state = 'completed';
