-- The lease/nonce alone cannot distinguish an unsent claim from an accepted
-- message whose response or completion was lost. Fence before crossing the wire.
-- Additive for older readers; rolling back to a blind-replay writer is NOT safe.
ALTER TABLE scheduled_messages
  ADD COLUMN IF NOT EXISTS delivery_uncertain BOOLEAN DEFAULT FALSE;

-- An older writer may already have sent any pending occurrence. Never infer
-- absence from lease expiry, restart or Discord's short nonce-deduplication window.
UPDATE scheduled_messages
   SET delivery_uncertain = TRUE
 WHERE occurrence_nonce IS NOT NULL OR claim_token IS NOT NULL;
