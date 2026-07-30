CREATE TABLE IF NOT EXISTS _pca_migrations (
    id text PRIMARY KEY,
    checksum text NOT NULL,
    app_version text NOT NULL,
    started_at timestamptz NOT NULL,
    completed_at timestamptz,
    status text NOT NULL CHECK (status IN ('started', 'completed', 'failed'))
);
