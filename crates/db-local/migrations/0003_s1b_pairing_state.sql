CREATE TABLE IF NOT EXISTS pairing_state (
    singleton_id INTEGER PRIMARY KEY NOT NULL CHECK (singleton_id = 1),
    device_id TEXT NOT NULL CHECK (
        length(device_id) = 36
        AND substr(device_id, 9, 1) = '-'
        AND substr(device_id, 14, 1) = '-'
        AND substr(device_id, 19, 1) = '-'
        AND substr(device_id, 24, 1) = '-'
        AND device_id NOT GLOB '*[^0-9A-Fa-f-]*'
        AND length(replace(device_id, '-', '')) = 32
    ),
    workspace_id TEXT NOT NULL CHECK (
        length(workspace_id) = 36
        AND substr(workspace_id, 9, 1) = '-'
        AND substr(workspace_id, 14, 1) = '-'
        AND substr(workspace_id, 19, 1) = '-'
        AND substr(workspace_id, 24, 1) = '-'
        AND workspace_id NOT GLOB '*[^0-9A-Fa-f-]*'
        AND length(replace(workspace_id, '-', '')) = 32
    ),
    credential_ref TEXT NOT NULL CHECK (
        credential_ref LIKE 'keychain://%'
    ),
    credential_generation INTEGER NOT NULL CHECK (credential_generation >= 0),
    applied_control_revision INTEGER NOT NULL DEFAULT 0 CHECK (
        applied_control_revision >= 0
    ),
    paired_at_ms INTEGER NOT NULL CHECK (paired_at_ms >= 0)
);
