ALTER TABLE auth_sessions
    ADD COLUMN IF NOT EXISTS session_token text;
ALTER TABLE auth_sessions
    ALTER COLUMN session_token_hash DROP NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS auth_sessions_session_token_unique
    ON auth_sessions(session_token)
    WHERE session_token IS NOT NULL;

ALTER TABLE pairing_sessions
    ADD COLUMN IF NOT EXISTS callback_state_hash character(64)
    CHECK (callback_state_hash IS NULL OR callback_state_hash ~ '^[0-9a-f]{64}$');
