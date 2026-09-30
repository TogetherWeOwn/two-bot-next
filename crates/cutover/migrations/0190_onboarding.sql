-- Onboarding funnel support (TOG-10086, S4 slice of TOG-9809).
--
-- The funnel `events` / `members` tables come from 0001 (legacy
-- `migrations/0001_initial.sql` + 0007/0008/0009); this slice adds no columns
-- and no tables. Onboarding writes three `event_type` values into the existing
-- log — `onboarding_prompted` (once-per-member, idempotency key
-- `guild:member:onboarding_prompted`), `game_roles_selected` and
-- `channel_routed` (repeatable by design, keyed `guild:member:type:time`) —
-- same spellings and key formats as legacy `idempotencyKey()` in
-- `src/core/events.ts`, so rows written by either implementation dedupe.
--
-- What this migration adds: the covering index the prompt gate reads on every
-- join (`has_onboarding_prompt`: guild + member + once-per-member type), and
-- the CHECK documenting the vocabulary this slice owns. `CREATE INDEX IF NOT
-- EXISTS` + transactional guard: one-shots run against staging and
-- agent-testdb scratch schemas that may already carry the S6-ported tables.
--
-- Reserved block for this card: 0190–0199 (parallel S4 slices never collide).

-- Prompt-gate read path: one indexed lookup per join / gate-clear.
CREATE INDEX IF NOT EXISTS idx_events_onboarding_prompt
  ON events (guild_id, member_id, event_type)
  WHERE event_type = 'onboarding_prompted';

-- Time-to-route reads (member_join → channel_routed per member, TWO-7).
CREATE INDEX IF NOT EXISTS idx_events_route_time
  ON events (guild_id, member_id, occurred_at)
  WHERE event_type IN ('member_join', 'channel_routed');

-- Vocabulary this slice writes. A boolean CHECK over the three literals
-- documents the contract without constraining the other slices' types.
DO $$
BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint WHERE conname = 'chk_events_onboarding_vocabulary'
  ) THEN
    ALTER TABLE events ADD CONSTRAINT chk_events_onboarding_vocabulary CHECK (
      event_type NOT IN ('onboarding_prompted', 'game_roles_selected', 'channel_routed')
      OR (
        -- once-per-member prompt vs repeatable selection/routing rows
        (event_type = 'onboarding_prompted' AND member_id IS NOT NULL)
        OR (event_type IN ('game_roles_selected', 'channel_routed') AND member_id IS NOT NULL)
      )
    );
  END IF;
END
$$;
