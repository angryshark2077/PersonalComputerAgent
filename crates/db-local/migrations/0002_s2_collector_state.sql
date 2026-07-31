CREATE TABLE IF NOT EXISTS collector_states (
    collector_key TEXT PRIMARY KEY NOT NULL,
    status TEXT NOT NULL CHECK (
        status IN (
            'disabled', 'permission_required', 'initializing', 'running',
            'paused', 'degraded', 'unsupported', 'error'
        )
    ),
    version TEXT NOT NULL,
    desired_revision INTEGER NOT NULL DEFAULT 0 CHECK (desired_revision >= 0),
    applied_revision INTEGER NOT NULL DEFAULT 0 CHECK (applied_revision >= 0),
    last_event_at_ms INTEGER,
    last_health_at_ms INTEGER,
    last_error_code TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);
