-- no-transaction
-- Candidate for the 0110 lineage. The disposable plan fixture applies this
-- file directly. Keep it outside SQLx discovery while shared-schema fixtures
-- can deadlock with concurrent builds.
CREATE INDEX CONCURRENTLY idx_moderation_warnings_history_order
  ON moderation_warnings (guild_id, user_id, created_at DESC, id COLLATE "C" DESC);
