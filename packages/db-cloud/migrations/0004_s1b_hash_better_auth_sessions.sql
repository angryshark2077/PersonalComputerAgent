-- 0003 temporarily introduced a raw Better Auth session-token column. Existing
-- sessions are deliberately invalidated before removing it: a raw token cannot
-- be safely transformed in place without retaining an active bearer credential.
DELETE FROM auth_sessions;

DROP INDEX IF EXISTS auth_sessions_session_token_unique;
ALTER TABLE auth_sessions DROP COLUMN IF EXISTS session_token;
ALTER TABLE auth_sessions
    ALTER COLUMN session_token_hash SET NOT NULL;
