ALTER TABLE collector_configs
    ADD COLUMN screen_capture_enabled boolean NOT NULL DEFAULT false,
    ADD COLUMN screen_capture_scheduled_enabled boolean NOT NULL DEFAULT true,
    ADD COLUMN screen_capture_interval_seconds integer NOT NULL DEFAULT 300,
    ADD COLUMN screen_capture_activity_enabled boolean NOT NULL DEFAULT true,
    ADD COLUMN screen_capture_activity_min_interval_seconds integer NOT NULL DEFAULT 30,
    ADD COLUMN screen_capture_excluded_bundle_ids jsonb NOT NULL DEFAULT '[]'::jsonb,
    ADD CONSTRAINT collector_configs_screen_interval
        CHECK (screen_capture_interval_seconds BETWEEN 60 AND 86400),
    ADD CONSTRAINT collector_configs_screen_activity_interval
        CHECK (screen_capture_activity_min_interval_seconds BETWEEN 10 AND 3600);

CREATE TABLE device_screenshot_requests (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    device_id uuid NOT NULL,
    actor_user_id uuid NOT NULL REFERENCES auth_users(id) ON DELETE RESTRICT,
    status text NOT NULL DEFAULT 'queued'
        CHECK (status IN ('queued', 'succeeded', 'failed')),
    requested_at timestamptz NOT NULL,
    completed_at timestamptz,
    screenshot_id uuid,
    error_code text,
    FOREIGN KEY (workspace_id, device_id)
        REFERENCES devices(workspace_id, id) ON DELETE CASCADE
);

CREATE UNIQUE INDEX device_screenshot_requests_one_queued
    ON device_screenshot_requests(workspace_id, device_id)
    WHERE status = 'queued';

CREATE INDEX device_screenshot_requests_latest
    ON device_screenshot_requests(workspace_id, device_id, requested_at DESC);

CREATE TABLE device_screenshots (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    device_id uuid NOT NULL,
    request_id uuid REFERENCES device_screenshot_requests(id) ON DELETE SET NULL,
    trigger text NOT NULL CHECK (trigger IN ('manual', 'scheduled', 'activity')),
    captured_at timestamptz NOT NULL,
    app_bundle_id text,
    pixel_width integer NOT NULL CHECK (pixel_width > 0),
    pixel_height integer NOT NULL CHECK (pixel_height > 0),
    object_key text NOT NULL,
    expected_sha256 char(64) NOT NULL CHECK (expected_sha256 ~ '^[0-9a-f]{64}$'),
    expected_size_bytes bigint NOT NULL CHECK (expected_size_bytes > 0),
    expected_mime_type text NOT NULL CHECK (expected_mime_type = 'image/jpeg'),
    state text NOT NULL DEFAULT 'prepared' CHECK (state IN ('prepared', 'completed')),
    prepared_at timestamptz NOT NULL,
    completed_at timestamptz,
    FOREIGN KEY (workspace_id, device_id)
        REFERENCES devices(workspace_id, id) ON DELETE CASCADE
);

CREATE UNIQUE INDEX device_screenshots_request_unique
    ON device_screenshots(request_id);

CREATE INDEX device_screenshots_chronology
    ON device_screenshots(workspace_id, device_id, captured_at DESC);

ALTER TABLE device_screenshot_requests
    ADD CONSTRAINT device_screenshot_requests_screenshot_fk
    FOREIGN KEY (screenshot_id) REFERENCES device_screenshots(id) ON DELETE SET NULL;
