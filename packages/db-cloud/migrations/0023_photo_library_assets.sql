CREATE TABLE IF NOT EXISTS photo_library_assets (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    device_id uuid NOT NULL,
    event_id uuid NOT NULL UNIQUE REFERENCES system_events(event_id) ON DELETE CASCADE,
    asset_id text NOT NULL,
    captured_at timestamptz NOT NULL,
    media_type text NOT NULL CHECK (media_type IN ('image', 'video')),
    original_filename text NOT NULL,
    mime_type text NOT NULL,
    pixel_width integer NOT NULL CHECK (pixel_width > 0),
    pixel_height integer NOT NULL CHECK (pixel_height > 0),
    duration_seconds double precision NOT NULL CHECK (duration_seconds >= 0),
    album_names jsonb NOT NULL,
    object_key text NOT NULL UNIQUE,
    expected_sha256 char(64) NOT NULL CHECK (expected_sha256 ~ '^[a-f0-9]{64}$'),
    expected_size_bytes bigint NOT NULL CHECK (expected_size_bytes > 0),
    state text NOT NULL CHECK (state IN ('prepared', 'completed')),
    prepared_at timestamptz NOT NULL,
    completed_at timestamptz,
    FOREIGN KEY (workspace_id, device_id) REFERENCES devices(workspace_id, id) ON DELETE CASCADE,
    UNIQUE (workspace_id, device_id, asset_id),
    CHECK ((state = 'prepared' AND completed_at IS NULL) OR (state = 'completed' AND completed_at IS NOT NULL))
);

CREATE INDEX IF NOT EXISTS idx_photo_library_assets_owner_chronology
    ON photo_library_assets(workspace_id, device_id, captured_at DESC)
    WHERE state = 'completed';
