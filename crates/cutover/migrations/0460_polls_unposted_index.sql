-- no-transaction
-- One concurrent-index statement per migration avoids an implicit transaction.
CREATE INDEX CONCURRENTLY IF NOT EXISTS polls_unposted ON polls (guild_id, closed_at)
    WHERE status = 'closed' AND results_posted_at IS NULL;
