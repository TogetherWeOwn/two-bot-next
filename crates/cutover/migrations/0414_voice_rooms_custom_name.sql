-- Voice V3 `/name`: a per-room custom-name override. `custom_name` holds the
-- owner's (or an admin's) text exactly as typed, template tokens intact, so
-- the runtime can re-render it as the room changes; NULL means the room uses
-- its template name. `name_touched_at` records the last set or restore so
-- the rollback delta keeps measuring a row whose name changes in place (see
-- `rollback_delta.rs`); it is NULL until the first change.
--
-- No identity column: the member erasure plan deletes whole `voice_rooms`
-- rows by owner/original creator (its predicate is unchanged), which drops
-- the override with the row. The table-level backup dump list is unchanged.

ALTER TABLE voice_rooms
  ADD COLUMN IF NOT EXISTS custom_name text
    CHECK (custom_name IS NULL OR char_length(custom_name) BETWEEN 1 AND 100),
  ADD COLUMN IF NOT EXISTS name_touched_at timestamptz;
