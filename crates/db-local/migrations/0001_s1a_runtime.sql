CREATE TABLE IF NOT EXISTS local_meta (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS agent_state (
    singleton_id INTEGER PRIMARY KEY NOT NULL CHECK (singleton_id = 1),
    agent_status TEXT NOT NULL CHECK (
        agent_status IN (
            'unpaired',
            'initializing',
            'waiting_permission',
            'running',
            'degraded',
            'sleeping',
            'updating',
            'repair',
            'stopped'
        )
    ),
    bridge_status TEXT NOT NULL CHECK (
        bridge_status IN (
            'disconnected',
            'handshaking',
            'ready',
            'degraded',
            'incompatible',
            'stopped'
        )
    ),
    local_healthy INTEGER NOT NULL CHECK (local_healthy IN (0, 1)),
    updated_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS events_local (
    event_id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    device_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    source TEXT NOT NULL,
    schema_version INTEGER NOT NULL CHECK (schema_version > 0),
    occurred_at_ms INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    sensitivity TEXT NOT NULL CHECK (
        sensitivity IN ('public', 'normal', 'medium', 'high', 'secret')
    ),
    payload_json TEXT NOT NULL,
    attachment_refs_json TEXT NOT NULL DEFAULT '[]',
    idempotency_key TEXT
);

CREATE TABLE IF NOT EXISTS sync_outbox (
    outbox_id TEXT PRIMARY KEY NOT NULL,
    event_id TEXT NOT NULL UNIQUE,
    state TEXT NOT NULL DEFAULT 'pending' CHECK (
        state IN ('pending', 'sending', 'acked', 'conflict', 'dead_letter')
    ),
    created_at_ms INTEGER NOT NULL,
    FOREIGN KEY (event_id) REFERENCES events_local(event_id) ON DELETE RESTRICT
);

CREATE TABLE IF NOT EXISTS diagnostic_events (
    diagnostic_id TEXT PRIMARY KEY NOT NULL,
    occurred_at_ms INTEGER NOT NULL,
    level TEXT NOT NULL,
    code TEXT NOT NULL,
    redacted_json TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_events_local_occurred_at
    ON events_local(occurred_at_ms);
CREATE INDEX IF NOT EXISTS idx_sync_outbox_state_created
    ON sync_outbox(state, created_at_ms);
