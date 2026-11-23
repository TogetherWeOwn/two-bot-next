-- Synthetic accepted windows with the observed join exactly at opening grace.
-- Both windows are live during the fixture. Only the second guild has a key.
INSERT INTO event_instance_windows
    (guild_id, event_id, instance_starts_at, starts_at, ends_at,
     status, observed_at, attendance_recovered_at)
SELECT guild_id, event_id, starts_at, starts_at, starts_at + INTERVAL '1 hour',
       'scheduled', '2026-01-01T00:00:00Z', NULL
FROM (VALUES
    ('100000000000000001', '200000000000000001'),
    ('100000000000000002', '200000000000000002')
) AS ids(guild_id, event_id)
CROSS JOIN (
    SELECT statement_timestamp() + INTERVAL '14 minutes 57 seconds' AS starts_at
) AS times;

INSERT INTO internal_event_keys (guild_id, event_key, event_id)
VALUES ('100000000000000002', 'show', '200000000000000002');
