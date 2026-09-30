-- Fence lockdown recovery cleanup to the generation that was restored.
-- PostgreSQL 13+ provides gen_random_uuid() without an extension. Existing
-- recovery rows receive a token too; their seed and reason are unchanged.
-- Repeated lockdowns preserve the original generation and seed (the store's
-- ON CONFLICT refreshes only reason/locked_at); cleanup deletes only the
-- matching generation and reports stale otherwise.
ALTER TABLE moderation_lockdowns
  ADD COLUMN recovery_generation TEXT NOT NULL DEFAULT pg_catalog.gen_random_uuid()::TEXT;
