-- Retain the first positive recovery after an absence independently of later
-- positive refreshes. This is a historical credit fence, not live authority.
-- New accepted instances explicitly insert NULL until their first recovery.
-- Existing evidence and older archives lack a complete recovery history: the
-- additive default fences new capture at migration/restore time, never backfills
-- attendance. Later positive refreshes preserve that conservative boundary.
-- Rollback keeps this nullable column; the previous binary ignores it.
ALTER TABLE event_instance_windows
  ADD COLUMN attendance_recovered_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP;
