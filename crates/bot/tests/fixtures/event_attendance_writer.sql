-- The primary window has no key yet. Its opening-grace boundary is also the
-- expired window's exclusive end and precedes the other window's recovery.
INSERT INTO event_instance_windows
    (guild_id, event_id, instance_starts_at, starts_at, ends_at,
     status, observed_at, attendance_recovered_at)
SELECT guild_id, event_id, starts_at, starts_at, ends_at,
       'scheduled', '2026-01-01T00:00:00Z', recovered_at
FROM (VALUES
    ('100000000000000001', '200000000000000001', '15 minutes', '75 minutes', NULL),
    ('100000000000000002', '200000000000000002', '15 minutes', '75 minutes', NULL),
    ('100000000000000001', '200000000000000003', '-1 hour', '-15 minutes', NULL),
    ('100000000000000001', '200000000000000004', '15 minutes', '75 minutes', '2 seconds')
) AS ids(guild_id, event_id, starts_offset, ends_offset, recovery_offset)
CROSS JOIN (
    SELECT statement_timestamp() - INTERVAL '3 seconds' AS observed_at
) AS clock
CROSS JOIN LATERAL (
    SELECT observed_at + starts_offset::interval AS starts_at,
           observed_at + ends_offset::interval AS ends_at,
           observed_at + recovery_offset::interval AS recovered_at
) AS windows;

INSERT INTO internal_event_keys (guild_id, event_key, event_id)
VALUES ('100000000000000002', 'show', '200000000000000002'),
       ('100000000000000001', 'expired', '200000000000000003'),
       ('100000000000000001', 'recovering', '200000000000000004');
