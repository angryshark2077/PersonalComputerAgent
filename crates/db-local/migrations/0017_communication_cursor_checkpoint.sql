ALTER TABLE communication_messages
ADD COLUMN cursor_sequence INTEGER;

UPDATE communication_messages
SET cursor_sequence = source_sequence
WHERE cursor_sequence IS NULL;
