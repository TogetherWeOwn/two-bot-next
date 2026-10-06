-- 0419_send_admission_lease: the 2026-10-05 staging crash loop showed a permit
-- whose completion failed during a storage outage leaves `in_flight` held
-- forever, so every later boot's admission is refused and the container never
-- serves. `in_flight_since_ms` (database clock, ms) stamps each take, letting
-- `admit` reclaim a lane whose holder is dead past the lease while fresh
-- holders still block. Existing rows default to 0 (ancient), so a
-- pre-migration stuck lane is reclaimable on the next admit. No PII: a
-- timestamp only, so the member erasure plan, the backup dump exclusion and
-- the table-wide admission role grants are unchanged.

ALTER TABLE public.discord_send_admission
  ADD COLUMN IF NOT EXISTS in_flight_since_ms BIGINT NOT NULL DEFAULT 0 CHECK (in_flight_since_ms >= 0);
