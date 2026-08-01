CREATE TABLE IF NOT EXISTS system_events (
    event_id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    device_id uuid NOT NULL,
    event_type text NOT NULL CHECK (
        event_type IN (
            'system.metric_sampled',
            'system.health_changed',
            'collector.status_changed'
        )
    ),
    source text NOT NULL CHECK (
        (event_type LIKE 'system.%' AND source = 'system')
        OR (event_type = 'collector.status_changed' AND source = 'collector.registry')
    ),
    schema_version integer NOT NULL CHECK (schema_version = 1),
    occurred_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL,
    sensitivity text NOT NULL CHECK (sensitivity = 'normal'),
    payload jsonb NOT NULL,
    idempotency_key text,
    FOREIGN KEY (workspace_id, device_id)
        REFERENCES devices(workspace_id, id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_system_events_device_chronology
    ON system_events(workspace_id, device_id, occurred_at DESC);
