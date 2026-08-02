CREATE TABLE IF NOT EXISTS communication_events (
    event_id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    device_id uuid NOT NULL,
    event_type text NOT NULL CHECK (event_type = 'communication.message_recorded'),
    source text NOT NULL CHECK (source = 'communication.wechat'),
    schema_version integer NOT NULL CHECK (schema_version = 1),
    occurred_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL,
    sensitivity text NOT NULL CHECK (sensitivity = 'high'),
    payload jsonb NOT NULL,
    attachment_refs jsonb NOT NULL,
    idempotency_key text,
    FOREIGN KEY (workspace_id, device_id)
        REFERENCES devices(workspace_id, id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_communication_events_device_chronology
    ON communication_events(workspace_id, device_id, occurred_at DESC);

CREATE UNIQUE INDEX IF NOT EXISTS communication_events_idempotency_unique
    ON communication_events(workspace_id, device_id, idempotency_key)
    WHERE idempotency_key IS NOT NULL;
