-- Member-notify DM verbs: `reminder.send` and `place_offer.send` need durable
-- idempotency claims like every other key-requiring verb, so the action CHECK
-- admits them. Backward compatible: older binaries never send the new verbs,
-- and no existing row changes meaning.
ALTER TABLE internal_idempotency DROP CONSTRAINT IF EXISTS internal_idempotency_action_check;
ALTER TABLE internal_idempotency ADD CONSTRAINT internal_idempotency_action_check
    CHECK (action IN (
        'role.assign', 'guild.add_member', 'announcement.post',
        'event.upsert', 'event.cancel', 'event.read',
        'automations.import', 'automations.export', 'settings.get', 'settings.set',
        'moderation.ban', 'moderation.tempban', 'moderation.kick',
        'moderation.timeout', 'moderation.warn', 'moderation.purge',
        'moderation.slowmode', 'moderation.lockdown', 'moderation.unlock',
        'reminder.send', 'place_offer.send'
    )) NOT VALID;

ALTER TABLE internal_idempotency VALIDATE CONSTRAINT internal_idempotency_action_check;

-- Pin the site's bounded event key with the member and receipt. Both intent
-- and audit retain correlation without retaining event names or DM text.
ALTER TABLE internal_idempotency ADD COLUMN event_key TEXT
    CHECK (event_key ~ '^[A-Za-z0-9._:-]{1,64}$');
ALTER TABLE internal_action_log ADD COLUMN event_key TEXT
    CHECK (event_key ~ '^[A-Za-z0-9._:-]{1,64}$');

-- Contract v2 §7 consent backstop: a suppressed member's refusal finishes the
-- same `dm_unavailable` receipt, but the terminal audit records *why* it never
-- sent. Widen the audit evidence allowlist with `suppressed` (existing rows
-- keep `executor`; backward-compatible, same rollback order as above).
ALTER TABLE internal_action_log DROP CONSTRAINT IF EXISTS internal_action_log_evidence_code_check;
ALTER TABLE internal_action_log ADD CONSTRAINT internal_action_log_evidence_code_check
    CHECK (evidence_code IN (
        'executor', 'discord_confirmed_effect', 'discord_confirmed_no_effect', 'proven_not_sent',
        'suppressed'
    )) NOT VALID;
ALTER TABLE internal_action_log VALIDATE CONSTRAINT internal_action_log_evidence_code_check;

-- Hash the notify:<canonical discord_id> key; retain only numeric limiter
-- state, not member/event names or message text. One locked row shares both
-- verbs and all callers across receiver processes and restarts. The bounded
-- rolling history and integer token credits are committed with each new claim.
CREATE TABLE internal_notify_budgets (
    bucket_key TEXT PRIMARY KEY CHECK (bucket_key ~ '^[a-f0-9]{64}$'),
    credit_ms BIGINT NOT NULL DEFAULT 25920000 CHECK (credit_ms BETWEEN 0 AND 25920000),
    updated_ms BIGINT NOT NULL DEFAULT 0 CHECK (updated_ms >= 0),
    admissions BIGINT[] NOT NULL DEFAULT '{}' CHECK (cardinality(admissions) <= 10)
);
