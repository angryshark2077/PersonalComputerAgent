CREATE TABLE device_collector_health (
    workspace_id uuid NOT NULL,
    device_id uuid NOT NULL,
    collector_key text NOT NULL,
    collector_version text NOT NULL,
    status text NOT NULL,
    desired_config_revision bigint NOT NULL,
    applied_config_revision bigint NOT NULL,
    last_event_at timestamptz,
    last_health_at timestamptz,
    error_code text,
    reported_at timestamptz NOT NULL,
    agent_version text NOT NULL,
    PRIMARY KEY (workspace_id, device_id, collector_key),
    CONSTRAINT device_collector_health_device_fk
      FOREIGN KEY (workspace_id, device_id)
      REFERENCES devices (workspace_id, id)
      ON DELETE CASCADE,
    CONSTRAINT device_collector_health_key_valid
      CHECK (collector_key ~ '^[a-z][a-z0-9.-]{0,63}$'),
    CONSTRAINT device_collector_health_status_valid
      CHECK (status IN ('disabled', 'permission_required', 'initializing', 'running', 'paused', 'degraded', 'unsupported', 'error')),
    CONSTRAINT device_collector_health_revisions_nonnegative
      CHECK (desired_config_revision >= 0 AND applied_config_revision >= 0),
    CONSTRAINT device_collector_health_error_code_valid
      CHECK (error_code IS NULL OR error_code ~ '^[A-Z][A-Z0-9_]{0,127}$')
);

CREATE INDEX idx_device_collector_health_reported
  ON device_collector_health (workspace_id, device_id, reported_at DESC);
