CREATE INDEX IF NOT EXISTS idx_device_heartbeats_privacy_retention
    ON device_heartbeats(received_at)
    WHERE network_ssid IS NOT NULL
       OR network_bssid IS NOT NULL
       OR network_local_ipv4 IS NOT NULL
       OR network_local_ipv6 IS NOT NULL
       OR network_public_ip IS NOT NULL
       OR network_ip_country IS NOT NULL
       OR network_ip_region IS NOT NULL
       OR network_ip_city IS NOT NULL
       OR network_ip_accuracy IS NOT NULL
       OR network_location_latitude IS NOT NULL
       OR network_location_longitude IS NOT NULL
       OR network_location_horizontal_accuracy_meters IS NOT NULL
       OR network_location_observed_at IS NOT NULL;
