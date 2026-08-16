ALTER TABLE pairing_sessions
ADD COLUMN requested_device_id uuid;

CREATE INDEX idx_pairing_sessions_requested_device
    ON pairing_sessions(requested_device_id)
    WHERE requested_device_id IS NOT NULL;
