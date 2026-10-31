-- Support globally bounded oldest-first event-store retention without sorting
-- the full append-only table. Keep all capture classes in the index because
-- the runtime query binds its event names and must not depend on a partial-index
-- predicate being inferred from bind parameters.
--
-- Built inside the migration transaction, like the other event_store indexes:
-- CREATE INDEX CONCURRENTLY waits on every open snapshot in the database, and
-- the parallel disposable-database test lanes hold such snapshots while queued
-- on the migrator's advisory lock, which deadlocks (40P01). event_store is the
-- capture table added in the same release window, so the brief build lock is
-- bounded by its current size.
CREATE INDEX IF NOT EXISTS idx_event_store_expiry_event_time_id
    ON event_store (event_time, id);
