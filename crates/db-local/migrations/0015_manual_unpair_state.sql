ALTER TABLE pairing_state
ADD COLUMN manually_unpaired INTEGER NOT NULL DEFAULT 0
CHECK (manually_unpaired IN (0, 1));
