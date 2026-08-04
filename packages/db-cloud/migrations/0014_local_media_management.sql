ALTER TABLE device_heartbeats
    ADD COLUMN completed_media_file_count bigint NOT NULL DEFAULT 0,
    ADD COLUMN completed_media_bytes bigint NOT NULL DEFAULT 0,
    ADD COLUMN protected_media_file_count bigint NOT NULL DEFAULT 0,
    ADD COLUMN protected_media_bytes bigint NOT NULL DEFAULT 0;

ALTER TABLE device_heartbeats
    ADD CONSTRAINT device_heartbeats_media_counts_nonnegative CHECK (
        completed_media_file_count >= 0
        AND completed_media_bytes >= 0
        AND protected_media_file_count >= 0
        AND protected_media_bytes >= 0
    );

CREATE TABLE device_media_cleanup_requests (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    device_id uuid NOT NULL,
    actor_user_id uuid NOT NULL REFERENCES auth_users(id) ON DELETE RESTRICT,
    status text NOT NULL DEFAULT 'queued' CHECK (status IN ('queued', 'succeeded', 'failed')),
    requested_at timestamptz NOT NULL,
    completed_at timestamptz,
    deleted_file_count bigint,
    freed_bytes bigint,
    error_code text,
    FOREIGN KEY (workspace_id, device_id)
        REFERENCES devices(workspace_id, id) ON DELETE CASCADE,
    CHECK (
        (status = 'queued' AND completed_at IS NULL AND deleted_file_count IS NULL
            AND freed_bytes IS NULL AND error_code IS NULL)
        OR (status = 'succeeded' AND completed_at IS NOT NULL
            AND deleted_file_count IS NOT NULL AND deleted_file_count >= 0
            AND freed_bytes IS NOT NULL AND freed_bytes >= 0 AND error_code IS NULL)
        OR (status = 'failed' AND completed_at IS NOT NULL
            AND deleted_file_count IS NOT NULL AND deleted_file_count >= 0
            AND freed_bytes IS NOT NULL AND freed_bytes >= 0
            AND error_code IS NOT NULL AND length(error_code) > 0)
    )
);

CREATE UNIQUE INDEX device_media_cleanup_one_queued
    ON device_media_cleanup_requests(workspace_id, device_id)
    WHERE status = 'queued';

CREATE INDEX device_media_cleanup_latest
    ON device_media_cleanup_requests(workspace_id, device_id, requested_at DESC);
