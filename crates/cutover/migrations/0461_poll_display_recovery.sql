-- Commit votes before their live edit; retry only the idempotent display PATCH.
ALTER TABLE polls ADD COLUMN IF NOT EXISTS display_dirty BOOLEAN NOT NULL DEFAULT FALSE;
