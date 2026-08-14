-- Keeps a photo upload task in the same durable transaction as its Event and Outbox row.
CREATE TABLE IF NOT EXISTS photo_upload_spool (
    photo_id TEXT PRIMARY KEY NOT NULL CHECK (
        length(photo_id) = 36
        AND substr(photo_id, 9, 1) = '-'
        AND substr(photo_id, 14, 1) = '-'
        AND substr(photo_id, 19, 1) = '-'
        AND substr(photo_id, 24, 1) = '-'
        AND replace(photo_id, '-', '') NOT GLOB '*[^0-9a-fA-F]*'
    ),
    event_id TEXT NOT NULL UNIQUE,
    manifest_json TEXT NOT NULL CHECK (json_valid(manifest_json)),
    transfer_state TEXT NOT NULL DEFAULT 'pending' CHECK (
        transfer_state IN ('pending', 'completed')
    ),
    created_at_ms INTEGER NOT NULL,
    completed_at_ms INTEGER,
    FOREIGN KEY (event_id) REFERENCES events_local(event_id) ON DELETE RESTRICT
);

CREATE INDEX IF NOT EXISTS idx_photo_upload_spool_pending
    ON photo_upload_spool(transfer_state, created_at_ms, photo_id);
