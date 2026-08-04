ALTER TABLE device_heartbeats
    ADD COLUMN network_location_latitude double precision,
    ADD COLUMN network_location_longitude double precision,
    ADD COLUMN network_location_horizontal_accuracy_meters double precision,
    ADD COLUMN network_location_observed_at timestamptz;

ALTER TABLE device_heartbeats
    ADD CONSTRAINT device_heartbeats_device_location_shape CHECK (
        (
            network_location_latitude IS NULL
            AND network_location_longitude IS NULL
            AND network_location_horizontal_accuracy_meters IS NULL
            AND network_location_observed_at IS NULL
        ) OR (
            network_location_latitude BETWEEN -90 AND 90
            AND network_location_longitude BETWEEN -180 AND 180
            AND network_location_horizontal_accuracy_meters BETWEEN 0 AND 100000
            AND network_location_observed_at IS NOT NULL
        )
    );
