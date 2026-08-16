-- Preserves locally invalid media tasks for diagnosis without retrying them forever.
ALTER TABLE attachment_spool ADD COLUMN terminal_failure_code TEXT CHECK (
    terminal_failure_code IS NULL
    OR terminal_failure_code IN ('MEDIA_LOCAL_BODY_INVALID', 'MEDIA_SOURCE_UNSUPPORTED')
);

ALTER TABLE photo_upload_spool ADD COLUMN terminal_failure_code TEXT CHECK (
    terminal_failure_code IS NULL
    OR terminal_failure_code = 'PHOTOS_LOCAL_MANIFEST_INVALID'
);

CREATE INDEX idx_attachment_spool_upload_queue
    ON attachment_spool(terminal_failure_code, transfer_state, created_at_ms, attachment_id);

CREATE INDEX idx_photo_upload_spool_upload_queue
    ON photo_upload_spool(terminal_failure_code, transfer_state, created_at_ms, photo_id);
