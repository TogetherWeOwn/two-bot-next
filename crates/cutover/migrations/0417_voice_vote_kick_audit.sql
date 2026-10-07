-- Voice V4 vote-kick audit trail. The room vote (`/kick` on a member who shares
-- the invoker's temporary room) is a room-scoped decision, not a guild kick:
-- a passed vote denies the target Connect on that room and disconnects them
-- from voice. The moderation `/kick` writes `moderation_audit`; this table is
-- the vote path's own record and must never be read as a member kick.
--
-- One row per (guild, vote, event). `vote_id` is the initiating interaction
-- ID (a snowflake the vote core already binds ballots to), never the
-- interaction token, so a replayed write is a no-op. `event` is one of
-- vote_started, vote_refused, vote_result or enforcement; `outcome` is a fixed
-- lower-case code (see `two_bot_core::voice_vote_kick_audit`), with no free
-- text. The progress columns ride vote_started/vote_result rows only.
--
-- Identity columns are `initiator_id` and `target_id`; the member erasure plan
-- deletes a member's rows by either (both are retained for every event, so a
-- vote's rows leave together).

CREATE TABLE IF NOT EXISTS voice_vote_kick_audit (
  guild_id       TEXT NOT NULL,
  vote_id        TEXT NOT NULL,
  event          TEXT NOT NULL
    CHECK (event IN ('vote_started', 'vote_refused', 'vote_result', 'enforcement')),
  room_id        TEXT NOT NULL,
  initiator_id   TEXT NOT NULL,
  target_id      TEXT NOT NULL,
  outcome        TEXT NOT NULL CHECK (outcome ~ '^[a-z_]{1,40}$'),
  yes_votes      INTEGER CHECK (yes_votes IS NULL OR yes_votes >= 0),
  votes_required INTEGER CHECK (votes_required IS NULL OR votes_required >= 0),
  voters_total   INTEGER CHECK (voters_total IS NULL OR voters_total >= 0),
  occurred_at    TIMESTAMPTZ NOT NULL,
  PRIMARY KEY (guild_id, vote_id, event)
);
CREATE INDEX IF NOT EXISTS idx_voice_vote_kick_audit_time
  ON voice_vote_kick_audit (guild_id, occurred_at);
CREATE INDEX IF NOT EXISTS idx_voice_vote_kick_audit_target
  ON voice_vote_kick_audit (guild_id, target_id, occurred_at);
