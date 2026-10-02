-- Record the counting phase on the winning delivery claim atomically with
-- the once-per-message ledger commit. A counted claim survives safe release
-- so reconciliation, not a silent AlreadyProcessed, settles the delivery.
ALTER TABLE automod_delivery_claims
  ADD COLUMN counted BOOLEAN NOT NULL DEFAULT FALSE;
