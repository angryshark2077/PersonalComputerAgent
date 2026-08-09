ALTER TABLE device_credential_generations
  ADD COLUMN rotation_replay_payload text,
  ADD COLUMN rotation_replay_expires_at timestamptz,
  ADD CONSTRAINT device_credentials_rotation_replay_pair
    CHECK ((rotation_replay_payload IS NULL) = (rotation_replay_expires_at IS NULL)),
  ADD CONSTRAINT device_credentials_rotation_replay_nonempty
    CHECK (rotation_replay_payload IS NULL OR length(rotation_replay_payload) > 0);
