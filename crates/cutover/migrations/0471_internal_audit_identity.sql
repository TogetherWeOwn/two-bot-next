-- rollback-compatible: prior-image writes omit nullable audit keys and preserve pinned identity; only the new private invoker helper loses PUBLIC EXECUTE, while normal intent transitions and audit erasure remain valid.
-- Whole-file argument and target-specific release proof: docs/migrations.md.
-- Freeze publication identity at the action boundary, not through mutable maps.
-- NULL supports old images and unprovable history; never backfill guessed names.
ALTER TABLE internal_idempotency
    ADD COLUMN audit_event_key TEXT,
    ADD COLUMN audit_rank_key TEXT;
ALTER TABLE internal_action_log
    ADD COLUMN audit_event_key TEXT,
    ADD COLUMN audit_rank_key TEXT;

ALTER TABLE internal_idempotency ADD CONSTRAINT internal_intent_event_key
    CHECK (audit_event_key IS NULL OR
        (action IN ('event.upsert', 'event.cancel') AND octet_length(audit_event_key) BETWEEN 1 AND 200
         AND audit_event_key COLLATE "C" ~ '^[A-Za-z0-9._:-]+$')) NOT VALID;
ALTER TABLE internal_idempotency VALIDATE CONSTRAINT internal_intent_event_key;
ALTER TABLE internal_idempotency ADD CONSTRAINT internal_intent_rank_key
    CHECK (audit_rank_key IS NULL OR
        (action = 'role.assign' AND resolved_role_id IS NOT NULL
         AND audit_rank_key IN ('prospect', 'member', 'soldier', 'veteran', 'legend'))) NOT VALID;
ALTER TABLE internal_idempotency VALIDATE CONSTRAINT internal_intent_rank_key;
ALTER TABLE internal_action_log ADD CONSTRAINT internal_log_event_key
    CHECK (audit_event_key IS NULL OR
        (action IN ('event.upsert', 'event.cancel') AND octet_length(audit_event_key) BETWEEN 1 AND 200
         AND audit_event_key COLLATE "C" ~ '^[A-Za-z0-9._:-]+$')) NOT VALID;
ALTER TABLE internal_action_log VALIDATE CONSTRAINT internal_log_event_key;
ALTER TABLE internal_action_log ADD CONSTRAINT internal_log_rank_key
    CHECK (audit_rank_key IS NULL OR
        (action = 'role.assign' AND resolved_role_id IS NOT NULL
         AND audit_rank_key IN ('prospect', 'member', 'soldier', 'veteran', 'legend'))) NOT VALID;
ALTER TABLE internal_action_log VALIDATE CONSTRAINT internal_log_rank_key;

-- Replays and reconciliation may not enrich a previously consumed audit row.
-- Authorized erasure still DELETEs rows; this is not a blanket append-only gate.
CREATE OR REPLACE FUNCTION public.internal_audit_identity_immutable() RETURNS trigger
LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, public AS $body$
BEGIN
    IF TG_TABLE_NAME = 'internal_action_log' THEN
        IF NEW IS DISTINCT FROM OLD THEN
            RAISE EXCEPTION 'internal audit rows are immutable';
        END IF;
    ELSE
        IF NEW.audit_event_key IS DISTINCT FROM OLD.audit_event_key
           OR ((OLD.state = 'completed' OR OLD.audit_rank_key IS NOT NULL)
               AND NEW.audit_rank_key IS DISTINCT FROM OLD.audit_rank_key) THEN
            RAISE EXCEPTION 'internal audit identity is immutable';
        END IF;
    END IF;
    RETURN NEW;
END;
$body$;
REVOKE ALL ON FUNCTION public.internal_audit_identity_immutable() FROM PUBLIC;
CREATE TRIGGER internal_log_identity_immutable BEFORE UPDATE ON internal_action_log
    FOR EACH ROW EXECUTE FUNCTION internal_audit_identity_immutable();
CREATE TRIGGER internal_intent_identity_immutable BEFORE UPDATE ON internal_idempotency
    FOR EACH ROW EXECUTE FUNCTION internal_audit_identity_immutable();
