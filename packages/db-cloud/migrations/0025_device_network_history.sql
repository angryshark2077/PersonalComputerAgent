CREATE TABLE device_network_history (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL,
    device_id uuid NOT NULL,
    observed_at timestamptz NOT NULL,
    interface_type text NOT NULL,
    wifi_identity_available boolean NOT NULL,
    ssid text,
    bssid text,
    local_ipv4 inet,
    local_ipv6 inet,
    public_ip inet,
    ip_country text,
    ip_region text,
    ip_city text,
    ip_accuracy text,
    location_latitude double precision,
    location_longitude double precision,
    location_horizontal_accuracy_meters double precision,
    location_observed_at timestamptz,
    CONSTRAINT device_network_history_device_fk
        FOREIGN KEY (workspace_id, device_id) REFERENCES devices(workspace_id, id) ON DELETE CASCADE
);

CREATE INDEX idx_device_network_history_recent
    ON device_network_history (workspace_id, device_id, observed_at DESC);
