-- Fence completion/release to the winning claim generation. PostgreSQL 13+
-- provides gen_random_uuid() without an extension. Existing shared ledger rows
-- receive a token too; their request identity and completion state are unchanged.
ALTER TABLE moderation_idempotency
  ADD COLUMN claim_token TEXT NOT NULL DEFAULT pg_catalog.gen_random_uuid()::TEXT;
