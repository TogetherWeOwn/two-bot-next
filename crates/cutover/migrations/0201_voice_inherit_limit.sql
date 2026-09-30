-- NULL inherits the live creator's limit; 0 is an explicit unlimited override.
-- Preserve earlier stored overrides and the shared migration checksum history.
ALTER TABLE voice_creators ALTER COLUMN default_limit DROP NOT NULL;
ALTER TABLE voice_creators ALTER COLUMN default_limit DROP DEFAULT;
