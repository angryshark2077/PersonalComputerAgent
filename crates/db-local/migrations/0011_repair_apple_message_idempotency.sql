UPDATE events_local
SET idempotency_key = json_extract(payload_json, '$.source_key')
WHERE source = 'communication.messages'
  AND event_type = 'communication.message_recorded'
  AND json_type(payload_json, '$.source_key') = 'text'
  AND EXISTS (
      SELECT 1
      FROM sync_outbox
      WHERE sync_outbox.event_id = events_local.event_id
        AND sync_outbox.state <> 'acked'
  );
