-- TOG-1649 review hardening: make feed-delivery ownership exclusive across replicas.

ALTER TABLE feed_deliveries ADD COLUMN IF NOT EXISTS claim_token TEXT;
ALTER TABLE feed_deliveries ADD COLUMN IF NOT EXISTS claimed_at TIMESTAMPTZ;
