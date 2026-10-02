-- Low-XP, constraint-valid ledger. Member 001 updates, 002 is unchanged,
-- 003 refuses lowering, 004 is absent from the export, and 005 is inserted.
-- The foreign guild shares BOTH an updated and a newly imported member ID.
INSERT INTO member_levels
    (guild_id, member_id, xp, message_xp, voice_xp, imported_xp, updated_at)
VALUES
    ('111111111111111111', '100000000000000001', 60, 30, 10, 20, '2026-09-30T00:00:00Z'),
    ('111111111111111111', '100000000000000002', 60, 15, 5, 40, '2026-09-30T00:00:00Z'),
    ('111111111111111111', '100000000000000003', 95, 9, 6, 80, '2026-09-30T00:00:00Z'),
    ('111111111111111111', '100000000000000004', 15, 6, 4, 5, '2026-09-30T00:00:00Z'),
    ('222222222222222222', '100000000000000001', 103, 60, 40, 3, '2026-09-29T00:00:00Z'),
    ('222222222222222222', '100000000000000005', 209, 100, 100, 9, '2026-09-29T00:00:00Z');

INSERT INTO xp_awards (guild_id, member_id, source, xp, occurred_at)
VALUES
    ('111111111111111111', '100000000000000001', 'message', 15, '2026-09-30T00:00:00Z'),
    ('111111111111111111', '100000000000000001', 'voice', 5, '2026-09-30T00:00:00Z'),
    ('222222222222222222', '100000000000000001', 'voice', 5, '2026-09-29T00:00:00Z');

INSERT INTO level_import_runs
    (guild_id, source, source_rows, unique_members, inserted, updated, unchanged,
     duplicate_rows, total_imported_xp, imported_at)
VALUES
    ('111111111111111111', 'mee6', 4, 4, 4, 0, 0, 0, 145, '2026-09-30T00:00:00Z'),
    ('222222222222222222', 'mee6', 2, 2, 2, 0, 0, 0, 12, '2026-09-29T00:00:00Z');
