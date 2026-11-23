-- no-transaction
-- Extend the 0110 lookup order without replacing its existing index.
CREATE INDEX CONCURRENTLY idx_moderation_warnings_history_order
  ON moderation_warnings (guild_id, user_id, created_at DESC, id COLLATE "C" DESC);
