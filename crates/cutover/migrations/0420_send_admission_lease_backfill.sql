-- 0420_send_admission_lease_backfill: fence the production 0419 apply.
--
-- 0419 added `in_flight_since_ms DEFAULT 0`. A lane held through the legacy
-- (pre-lease) fallback path keeps stamp 0, which reads as older than the 60 s
-- lease, so the next admit after 0419 lands would reclaim a live sender: two
-- senders on one Discord token. Stamp every legacy-held row with the
-- apply-time database clock so it owns a full fresh lease; genuinely stale
-- holders (real stamps past the lease) are untouched and still self-heal, and
-- free lanes need no stamp because the next take overwrites it. Same
-- `clock_timestamp()` clock the admit path uses. No PII: a timestamp only, so
-- the member erasure plan, the backup dump exclusion and the table-wide
-- admission role grants are unchanged. Re-runnable: only rows still at the
-- legacy default move.

UPDATE public.discord_send_admission
SET in_flight_since_ms = (extract(epoch FROM clock_timestamp()) * 1000)::bigint
WHERE in_flight AND in_flight_since_ms = 0;
