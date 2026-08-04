ALTER TABLE device_heartbeats
    ADD COLUMN network_interface_type text,
    ADD COLUMN network_wifi_identity_available boolean,
    ADD COLUMN network_ssid text,
    ADD COLUMN network_bssid text,
    ADD COLUMN network_local_ipv4 inet,
    ADD COLUMN network_local_ipv6 inet,
    ADD COLUMN network_public_ip inet,
    ADD COLUMN network_ip_country text,
    ADD COLUMN network_ip_region text,
    ADD COLUMN network_ip_city text,
    ADD COLUMN network_ip_accuracy text;

ALTER TABLE device_heartbeats
    ADD CONSTRAINT device_heartbeats_network_shape CHECK (
        (
            network_interface_type IS NULL
            AND network_wifi_identity_available IS NULL
            AND network_ssid IS NULL
            AND network_bssid IS NULL
            AND network_local_ipv4 IS NULL
            AND network_local_ipv6 IS NULL
        ) OR (
            network_interface_type IN ('wifi', 'wired', 'other', 'none')
            AND network_wifi_identity_available IS NOT NULL
            AND (network_interface_type = 'wifi' OR (network_ssid IS NULL AND network_bssid IS NULL))
            AND (network_bssid IS NULL OR network_bssid ~ '^[0-9A-F]{2}(:[0-9A-F]{2}){5}$')
        )
    ),
    ADD CONSTRAINT device_heartbeats_network_geo_shape CHECK (
        network_ip_accuracy IS NULL OR network_ip_accuracy = 'ip_city'
    );

CREATE TABLE network_location_library (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    actor_user_id uuid NOT NULL,
    name text NOT NULL CHECK (length(btrim(name)) BETWEEN 1 AND 100),
    match_ssid text,
    match_bssid text,
    country text,
    region text,
    city text,
    created_at timestamptz NOT NULL,
    updated_at timestamptz NOT NULL,
    FOREIGN KEY (workspace_id, actor_user_id)
        REFERENCES workspace_members(workspace_id, user_id) ON DELETE CASCADE,
    CHECK (match_ssid IS NOT NULL OR match_bssid IS NOT NULL),
    CHECK (match_ssid IS NULL OR length(match_ssid) BETWEEN 1 AND 128),
    CHECK (match_bssid IS NULL OR match_bssid ~ '^[0-9A-F]{2}(:[0-9A-F]{2}){5}$')
);

CREATE UNIQUE INDEX network_location_library_workspace_bssid_unique
    ON network_location_library(workspace_id, match_bssid)
    WHERE match_bssid IS NOT NULL;

CREATE INDEX idx_network_location_library_workspace_ssid
    ON network_location_library(workspace_id, match_ssid)
    WHERE match_ssid IS NOT NULL;
