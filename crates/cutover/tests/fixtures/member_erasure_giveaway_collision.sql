-- Giveaway erasure collision seed: the same giveaway id in two guilds, with
-- distinct creator, entrant and winner members, and a reroll round naming
-- its replaced winner only in reroll_kind. Every child row has a live
-- parent: the schema binds entries and draws to `giveaways` with composite
-- foreign keys, so parentless orphan rows are impossible.
-- Fixed fake snowflakes only. Never load this fixture into a runtime database.
INSERT INTO giveaways
  (guild_id, id, prize, channel_id, message_id, created_by, winners_count, ends_at,
   status, requirements_json, max_entries, draw_nonce, created_at, updated_at)
VALUES
  ('1545644954272137297', 'gw-collide', 'fixture', '999999999999999999', 'm-1', '123456789012345680', 2, '2026-10-01T00:00:00Z',
   'open', '{}'::jsonb, 1, 'nonce-1', '2026-10-01T00:00:00Z', '2026-10-01T00:00:00Z'),
  ('1545644954272137298', 'gw-collide', 'fixture', '999999999999999999', 'm-2', '123456789012345680', 1, '2026-10-01T00:00:00Z',
   'open', '{}'::jsonb, 1, 'nonce-2', '2026-10-01T00:00:00Z', '2026-10-01T00:00:00Z'),
  ('1545644954272137297', 'gw-reroll', 'fixture', '999999999999999999', 'm-3', '123456789012345680', 1, '2026-10-01T00:00:00Z',
   'drawn', '{}'::jsonb, 1, 'nonce-3', '2026-10-01T00:00:00Z', '2026-10-01T00:00:00Z'),
  ('1545644954272137297', 'gw-u', 'fixture', '999999999999999999', 'm-4', '123456789012345678', 1, '2026-10-01T00:00:00Z',
   'open', '{}'::jsonb, 1, 'nonce-4', '2026-10-01T00:00:00Z', '2026-10-01T00:00:00Z'),
  ('1545644954272137297', 'gw-v', 'fixture', '999999999999999999', 'm-5', '123456789012345680', 1, '2026-10-01T00:00:00Z',
   'open', '{}'::jsonb, 1, 'nonce-5', '2026-10-01T00:00:00Z', '2026-10-01T00:00:00Z');
INSERT INTO giveaway_entries (guild_id, giveaway_id, user_id, weight, entered_at)
VALUES
  ('1545644954272137297', 'gw-collide', '123456789012345678', 1, '2026-10-01T00:00:00Z'),
  ('1545644954272137298', 'gw-collide', '123456789012345678', 1, '2026-10-01T00:00:00Z'),
  ('1545644954272137297', 'gw-u', '123456789012345679', 1, '2026-10-01T00:00:00Z'),
  ('1545644954272137297', 'gw-v', '123456789012345679', 1, '2026-10-01T00:00:00Z');
INSERT INTO giveaway_draws
  (guild_id, giveaway_id, draw_no, drawn_at, eligible_count, eligible_hash, seed,
   winner_ids, reroll_kind)
VALUES
  ('1545644954272137297', 'gw-collide', 0, '2026-10-01T00:00:00Z', 2, 'fixture', 'fixture',
   '["123456789012345678", "123456789012345679"]'::jsonb, NULL),
  ('1545644954272137298', 'gw-collide', 0, '2026-10-01T00:00:00Z', 1, 'fixture', 'fixture',
   '["123456789012345678"]'::jsonb, NULL),
  ('1545644954272137297', 'gw-reroll', 1, '2026-10-01T00:00:00Z', 2, 'fixture', 'fixture',
   '["123456789012345679"]'::jsonb, 'single:123456789012345678'),
  ('1545644954272137297', 'gw-u', 0, '2026-10-01T00:00:00Z', 1, 'fixture', 'fixture',
   '["123456789012345679"]'::jsonb, NULL),
  ('1545644954272137297', 'gw-v', 0, '2026-10-01T00:00:00Z', 1, 'fixture', 'fixture',
   '["123456789012345679"]'::jsonb, NULL);
