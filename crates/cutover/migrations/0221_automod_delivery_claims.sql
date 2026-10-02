-- Per-delivery arbitration, separate from the once-per-message violation ledger.
-- The hash includes a stable edit revision: clean creates can be re-inspected on
-- edit, while gateway retries cannot repeat funnel awards or Discord effects.
-- A started mutation is never automatically released or expired. Its uncertain
-- outcome must not turn a gateway retry into a second sanction.
CREATE TABLE IF NOT EXISTS automod_delivery_claims (
  guild_id         TEXT NOT NULL,
  message_id       TEXT NOT NULL,
  delivery_kind    TEXT NOT NULL CHECK (delivery_kind IN ('create', 'update')),
  dry_run          BOOLEAN NOT NULL,
  request_hash     TEXT NOT NULL,
  claim_token      TEXT NOT NULL DEFAULT gen_random_uuid()::text,
  mutation_started BOOLEAN NOT NULL DEFAULT FALSE,
  result_json      JSONB,
  claimed_at       TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  completed_at     TIMESTAMPTZ,
  PRIMARY KEY (guild_id, message_id, delivery_kind, dry_run, request_hash)
);
