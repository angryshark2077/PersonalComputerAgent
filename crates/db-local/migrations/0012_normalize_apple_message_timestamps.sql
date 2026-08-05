UPDATE events_local
SET payload_json = CASE event_type
    WHEN 'communication.conversation_observed' THEN
        json_set(payload_json, '$.observed_at', strftime('%Y-%m-%dT%H:%M:%fZ', occurred_at_ms / 1000.0, 'unixepoch'))
    WHEN 'communication.message_sender_observed' THEN
        json_set(payload_json, '$.observed_at', strftime('%Y-%m-%dT%H:%M:%fZ', occurred_at_ms / 1000.0, 'unixepoch'))
    WHEN 'communication.message_recorded' THEN
        json_set(payload_json, '$.occurred_at', strftime('%Y-%m-%dT%H:%M:%fZ', occurred_at_ms / 1000.0, 'unixepoch'))
    ELSE payload_json
END
WHERE source = 'communication.messages'
  AND EXISTS (
      SELECT 1
      FROM sync_outbox
      WHERE sync_outbox.event_id = events_local.event_id
        AND sync_outbox.state <> 'acked'
  );
