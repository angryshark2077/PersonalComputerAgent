CREATE TABLE IF NOT EXISTS device_revocation_audit (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    device_id uuid NOT NULL,
    actor_user_id uuid NOT NULL,
    revoked_at timestamptz NOT NULL,
    FOREIGN KEY (workspace_id, device_id)
        REFERENCES devices(workspace_id, id) ON DELETE CASCADE,
    CONSTRAINT device_revocation_audit_actor_membership_fk
        FOREIGN KEY (workspace_id, actor_user_id)
        REFERENCES workspace_members(workspace_id, user_id) ON DELETE RESTRICT
);

CREATE INDEX IF NOT EXISTS idx_device_revocation_audit_chronology
    ON device_revocation_audit(workspace_id, device_id, revoked_at DESC);
