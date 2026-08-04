CREATE TEMP TABLE pca_media_projection_upgrades AS
WITH candidates AS (
    SELECT
        event.event_id,
        event.workspace_id,
        event.device_id,
        event.occurred_at,
        event.created_at,
        event.payload,
        CASE
            WHEN event.payload ->> 'source_key' LIKE '%:full' THEN 2
            WHEN event.payload ->> 'source_key' LIKE '%-pending' THEN 0
            ELSE 1
        END AS fidelity
    FROM communication_events AS event
    WHERE event.event_type = 'communication.message_recorded'
), ranked AS (
    SELECT
        candidate.*,
        ROW_NUMBER() OVER (
            PARTITION BY
                candidate.workspace_id,
                candidate.device_id,
                candidate.payload ->> 'message_id'
            ORDER BY candidate.fidelity DESC, candidate.created_at DESC, candidate.event_id DESC
        ) AS preference
    FROM candidates AS candidate
)
SELECT
    desired.event_id AS desired_event_id,
    current.event_id AS current_event_id,
    desired.workspace_id,
    desired.device_id,
    desired.occurred_at,
    desired.payload,
    current.sender_id,
    current.sender_display_name,
    current.sender_avatar_url
FROM ranked AS desired
INNER JOIN communication_messages AS current
    ON current.workspace_id = desired.workspace_id
    AND current.device_id = desired.device_id
    AND current.message_id = desired.payload ->> 'message_id'
INNER JOIN candidates AS projected
    ON projected.event_id = current.event_id
WHERE desired.preference = 1
  AND desired.event_id <> current.event_id
  AND desired.fidelity > projected.fidelity;

DELETE FROM communication_messages AS message
USING pca_media_projection_upgrades AS upgrade
WHERE message.event_id = upgrade.current_event_id;

INSERT INTO communication_messages (
    event_id,
    workspace_id,
    device_id,
    conversation_id,
    message_id,
    sender_id,
    sender_display_name,
    sender_avatar_url,
    source_key,
    occurred_at,
    direction,
    kind,
    text_body
)
SELECT
    desired_event_id,
    workspace_id,
    device_id,
    payload ->> 'conversation_id',
    payload ->> 'message_id',
    COALESCE(NULLIF(payload ->> 'sender_id', ''), sender_id),
    COALESCE(NULLIF(payload ->> 'sender_display_name', ''), sender_display_name),
    sender_avatar_url,
    payload ->> 'source_key',
    occurred_at,
    payload ->> 'direction',
    payload ->> 'kind',
    CASE WHEN payload ->> 'kind' = 'text' THEN payload ->> 'text' ELSE NULL END
FROM pca_media_projection_upgrades;

INSERT INTO communication_message_attachments (
    event_id,
    attachment_id,
    kind,
    sha256,
    size_bytes,
    mime_type,
    file_name
)
SELECT
    upgrade.desired_event_id,
    attachment.value ->> 'attachment_id',
    attachment.value ->> 'kind',
    attachment.value ->> 'sha256',
    (attachment.value ->> 'size_bytes')::bigint,
    attachment.value ->> 'mime_type',
    NULLIF(attachment.value ->> 'file_name', '')
FROM pca_media_projection_upgrades AS upgrade
CROSS JOIN LATERAL jsonb_array_elements(
    COALESCE(upgrade.payload -> 'attachments', '[]'::jsonb)
) AS attachment(value);
