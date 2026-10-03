-- Voice V2 ownership handoffs: caretaker succession and /reclaim +
-- /transfer update `owner_id`/`original_creator_id` in place, breaking the
-- 0224 insert-once shape. `owner_touched_at` records the last handoff so the
-- rollback delta keeps measuring the row (see `rollback_delta.rs`); existing
-- rows inherit their creation stamp. No PII: a timestamp only, so the member
-- erasure plan (owner columns) and table-level dump/seed lists are unchanged.

ALTER TABLE voice_rooms
  ADD COLUMN IF NOT EXISTS owner_touched_at timestamptz NOT NULL DEFAULT now();

UPDATE voice_rooms SET owner_touched_at = created_at;
