-- Restrict each expiry class to a bounded index range scan instead of walking
-- non-expiring event-store families at the oldest end of the time index.
--
-- Keep this transactional: the concurrent build deadlocks disposable test
-- migrators that hold snapshots while queued on SQLx's advisory lock. The
-- event_store table is introduced and indexed within the same release window.
CREATE INDEX IF NOT EXISTS idx_event_store_expiry_class_time_id
    ON event_store (event_type, event_time, id);

DROP INDEX IF EXISTS idx_event_store_expiry_event_time_id;
