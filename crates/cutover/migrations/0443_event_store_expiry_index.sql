-- no-transaction
-- Support globally bounded oldest-first event-store retention without sorting
-- the full append-only table. Keep all capture classes in the index because
-- the runtime query binds its event names and must not depend on a partial-index
-- predicate being inferred from bind parameters.
CREATE INDEX CONCURRENTLY idx_event_store_expiry_event_time_id
    ON event_store (event_time, id);
