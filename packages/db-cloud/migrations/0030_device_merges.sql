ALTER TABLE devices
  ADD COLUMN merged_into_device_id uuid,
  ADD COLUMN merged_at timestamptz,
  ADD COLUMN merged_by_user_id uuid;

ALTER TABLE devices
  ADD CONSTRAINT devices_merge_target_fk
    FOREIGN KEY (workspace_id, merged_into_device_id)
    REFERENCES devices (workspace_id, id)
    ON DELETE RESTRICT,
  ADD CONSTRAINT devices_merge_actor_fk
    FOREIGN KEY (merged_by_user_id)
    REFERENCES auth_users (id)
    ON DELETE RESTRICT,
  ADD CONSTRAINT devices_not_merged_into_self
    CHECK (merged_into_device_id IS NULL OR merged_into_device_id <> id),
  ADD CONSTRAINT devices_merge_fields_complete
    CHECK (
      (merged_into_device_id IS NULL AND merged_at IS NULL AND merged_by_user_id IS NULL)
      OR
      (merged_into_device_id IS NOT NULL AND merged_at IS NOT NULL AND merged_by_user_id IS NOT NULL)
    );

CREATE INDEX idx_devices_merge_target
  ON devices (workspace_id, merged_into_device_id)
  WHERE merged_into_device_id IS NOT NULL;
