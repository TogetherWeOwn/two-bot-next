-- Bound reconciliation to the message accepted by Discord.
--
-- A failed database acknowledgement can be retried after arbitrarily many
-- newer channel messages. Starting the history scan immediately after the
-- accepted snowflake avoids a fixed recent-history window duplicating it.
ALTER TABLE operational_audit_log
  ADD COLUMN IF NOT EXISTS delivery_search_before TEXT;
