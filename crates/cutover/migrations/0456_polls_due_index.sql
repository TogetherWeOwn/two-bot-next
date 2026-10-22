-- no-transaction
-- IF NOT EXISTS tables may already contain rows: avoid blocking their writes.
CREATE INDEX CONCURRENTLY IF NOT EXISTS polls_due ON polls (guild_id, ends_at)
    WHERE status = 'open';
