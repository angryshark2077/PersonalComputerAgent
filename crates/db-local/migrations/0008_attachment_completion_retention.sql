ALTER TABLE attachment_spool ADD COLUMN completed_at_ms INTEGER;

CREATE INDEX idx_attachment_spool_retention
    ON attachment_spool(transfer_state, completed_at_ms);
