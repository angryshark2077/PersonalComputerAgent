CREATE TABLE IF NOT EXISTS auth_users (
    id uuid PRIMARY KEY,
    name text NOT NULL,
    email text NOT NULL UNIQUE,
    email_verified boolean NOT NULL DEFAULT false,
    image_url text,
    created_at timestamptz NOT NULL,
    updated_at timestamptz NOT NULL
);

CREATE TABLE IF NOT EXISTS auth_sessions (
    id uuid PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES auth_users(id) ON DELETE CASCADE,
    session_token_hash character(64) NOT NULL UNIQUE CHECK (
        session_token_hash ~ '^[0-9a-f]{64}$'
    ),
    expires_at timestamptz NOT NULL,
    ip_address inet,
    user_agent text,
    created_at timestamptz NOT NULL,
    updated_at timestamptz NOT NULL
);

CREATE TABLE IF NOT EXISTS auth_accounts (
    id uuid PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES auth_users(id) ON DELETE CASCADE,
    provider_id text NOT NULL,
    account_id text NOT NULL,
    password_hash text,
    created_at timestamptz NOT NULL,
    updated_at timestamptz NOT NULL,
    UNIQUE (provider_id, account_id)
);

CREATE TABLE IF NOT EXISTS workspaces (
    id uuid PRIMARY KEY,
    name text NOT NULL,
    slug text NOT NULL UNIQUE,
    created_at timestamptz NOT NULL,
    updated_at timestamptz NOT NULL
);

CREATE TABLE IF NOT EXISTS workspace_members (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    user_id uuid NOT NULL REFERENCES auth_users(id) ON DELETE CASCADE,
    role text NOT NULL CHECK (role = 'owner'),
    created_at timestamptz NOT NULL,
    PRIMARY KEY (workspace_id, user_id),
    UNIQUE (user_id)
);

CREATE TABLE IF NOT EXISTS devices (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
    owner_user_id uuid NOT NULL REFERENCES auth_users(id) ON DELETE RESTRICT,
    device_public_key_hash character(64) NOT NULL UNIQUE CHECK (
        device_public_key_hash ~ '^[0-9a-f]{64}$'
    ),
    platform text NOT NULL CHECK (platform = 'macos'),
    created_at timestamptz NOT NULL,
    revoked_at timestamptz,
    UNIQUE (workspace_id, id)
);

CREATE TABLE IF NOT EXISTS device_credential_generations (
    workspace_id uuid NOT NULL,
    device_id uuid NOT NULL,
    generation bigint NOT NULL CHECK (generation > 0),
    access_token_hash character(64) NOT NULL UNIQUE CHECK (
        access_token_hash ~ '^[0-9a-f]{64}$'
    ),
    refresh_token_hash character(64) NOT NULL UNIQUE CHECK (
        refresh_token_hash ~ '^[0-9a-f]{64}$'
    ),
    access_expires_at timestamptz NOT NULL,
    refresh_expires_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL,
    revoked_at timestamptz,
    PRIMARY KEY (device_id, generation),
    FOREIGN KEY (workspace_id, device_id)
        REFERENCES devices(workspace_id, id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS pairing_sessions (
    session_id_hash character(64) PRIMARY KEY CHECK (
        session_id_hash ~ '^[0-9a-f]{64}$'
    ),
    device_public_key_hash character(64) NOT NULL CHECK (
        device_public_key_hash ~ '^[0-9a-f]{64}$'
    ),
    code_challenge text NOT NULL,
    callback_uri text NOT NULL,
    expires_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL,
    authorized_at timestamptz
);

CREATE TABLE IF NOT EXISTS pairing_authorization_codes (
    authorization_code_hash character(64) PRIMARY KEY CHECK (
        authorization_code_hash ~ '^[0-9a-f]{64}$'
    ),
    session_id_hash character(64) NOT NULL UNIQUE
        REFERENCES pairing_sessions(session_id_hash) ON DELETE CASCADE,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
    owner_user_id uuid NOT NULL REFERENCES auth_users(id) ON DELETE RESTRICT,
    callback_state_hash character(64) NOT NULL CHECK (
        callback_state_hash ~ '^[0-9a-f]{64}$'
    ),
    expires_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL,
    consumed_at timestamptz
);

CREATE TABLE IF NOT EXISTS collector_configs (
    workspace_id uuid NOT NULL,
    device_id uuid NOT NULL,
    configuration_revision bigint NOT NULL DEFAULT 0 CHECK (
        configuration_revision >= 0
    ),
    network_enabled boolean NOT NULL DEFAULT false,
    wechat_enabled boolean NOT NULL DEFAULT false,
    updated_at timestamptz NOT NULL,
    PRIMARY KEY (workspace_id, device_id),
    FOREIGN KEY (workspace_id, device_id)
        REFERENCES devices(workspace_id, id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS collector_config_audit (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    device_id uuid NOT NULL,
    actor_user_id uuid NOT NULL REFERENCES auth_users(id) ON DELETE RESTRICT,
    configuration_revision bigint NOT NULL CHECK (configuration_revision > 0),
    old_config jsonb NOT NULL,
    new_config jsonb NOT NULL,
    created_at timestamptz NOT NULL,
    FOREIGN KEY (workspace_id, device_id)
        REFERENCES devices(workspace_id, id) ON DELETE CASCADE,
    UNIQUE (device_id, configuration_revision)
);

CREATE TABLE IF NOT EXISTS device_heartbeats (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    device_id uuid NOT NULL,
    received_at timestamptz NOT NULL,
    agent_version text NOT NULL,
    presence text NOT NULL CHECK (
        presence IN ('online', 'stale', 'offline', 'sleeping')
    ),
    outbox_depth bigint NOT NULL CHECK (outbox_depth >= 0),
    FOREIGN KEY (workspace_id, device_id)
        REFERENCES devices(workspace_id, id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_pairing_sessions_active_expiry
    ON pairing_sessions(expires_at)
    WHERE authorized_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_devices_workspace
    ON devices(workspace_id, id);
CREATE INDEX IF NOT EXISTS idx_collector_config_audit_chronology
    ON collector_config_audit(workspace_id, device_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_device_heartbeats_last
    ON device_heartbeats(workspace_id, device_id, received_at DESC);
