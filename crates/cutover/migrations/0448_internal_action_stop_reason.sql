-- Durable stop reason for website channel-moderation purge receipts. A purge
-- that Discord cut short after at least one deletion records why (`refused`
-- or `rate_limited`), so an outer replay returns the first result
-- byte-identically. A completed purge, every other receipt and every
-- existing row stay NULL. The terminal audit copies the scalar from the
-- completed receipt; intent audits keep it NULL. Closed set, never free text.
ALTER TABLE internal_idempotency
    ADD COLUMN stop_reason TEXT CHECK (stop_reason IN ('refused', 'rate_limited'));
ALTER TABLE internal_action_log
    ADD COLUMN stop_reason TEXT CHECK (stop_reason IN ('refused', 'rate_limited'));
