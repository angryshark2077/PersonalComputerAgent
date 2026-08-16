CREATE TABLE applied_collector_control (
    singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 1),
    device_id TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    -- Revision 0 is reserved for the safe, incomplete legacy bootstrap below. Runtime writes
    -- still reject revision 0 and replace it with the first complete Cloud snapshot.
    configuration_revision INTEGER NOT NULL CHECK (configuration_revision >= 0),
    communication_wechat_enabled INTEGER NOT NULL
        CHECK (communication_wechat_enabled IN (0, 1)),
    screen_capture_enabled INTEGER NOT NULL
        CHECK (screen_capture_enabled IN (0, 1)),
    screen_capture_scheduled_enabled INTEGER NOT NULL
        CHECK (screen_capture_scheduled_enabled IN (0, 1)),
    screen_capture_interval_seconds INTEGER NOT NULL
        CHECK (screen_capture_interval_seconds BETWEEN 60 AND 86400),
    screen_capture_activity_enabled INTEGER NOT NULL
        CHECK (screen_capture_activity_enabled IN (0, 1)),
    screen_capture_activity_min_interval_seconds INTEGER NOT NULL
        CHECK (screen_capture_activity_min_interval_seconds BETWEEN 10 AND 3600),
    screen_capture_excluded_bundle_ids_json TEXT NOT NULL,
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= 0)
);

INSERT INTO applied_collector_control (
    singleton_id,
    device_id,
    workspace_id,
    configuration_revision,
    communication_wechat_enabled,
    screen_capture_enabled,
    screen_capture_scheduled_enabled,
    screen_capture_interval_seconds,
    screen_capture_activity_enabled,
    screen_capture_activity_min_interval_seconds,
    screen_capture_excluded_bundle_ids_json,
    updated_at_ms
)
SELECT
    1,
    pairing.device_id,
    pairing.workspace_id,
    0,
    CASE WHEN EXISTS (
        SELECT 1 FROM collector_states AS state
        WHERE state.collector_key = 'communication.wechat'
          AND state.applied_revision = pairing.applied_control_revision
          AND state.status <> 'disabled'
    ) THEN 1 ELSE 0 END,
    CASE WHEN EXISTS (
        SELECT 1 FROM collector_states AS state
        WHERE state.collector_key = 'screen.capture'
          AND state.applied_revision = pairing.applied_control_revision
          AND state.status <> 'disabled'
    ) THEN 1 ELSE 0 END,
    CASE WHEN EXISTS (
        SELECT 1 FROM collector_states AS state
        WHERE state.collector_key = 'screen.capture'
          AND state.applied_revision = pairing.applied_control_revision
          AND state.status <> 'disabled'
    ) THEN 1 ELSE 0 END,
    300,
    0,
    30,
    '[]',
    pairing.paired_at_ms
FROM pairing_state AS pairing
WHERE pairing.singleton_id = 1
  AND pairing.manually_unpaired = 0
  AND pairing.applied_control_revision > 0;
