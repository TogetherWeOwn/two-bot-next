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
    ));

-- Contract v2 §7 consent backstop: a suppressed member's refusal finishes the
-- same `dm_unavailable` receipt, but the terminal audit records *why* it never
-- sent. Widen the audit evidence allowlist with `suppressed` (existing rows
-- keep `executor`; backward-compatible, same rollback order as above).
ALTER TABLE internal_action_log DROP CONSTRAINT IF EXISTS internal_action_log_evidence_code_check;
ALTER TABLE internal_action_log ADD CONSTRAINT internal_action_log_evidence_code_check
    CHECK (evidence_code IN (
        'executor', 'discord_confirmed_effect', 'discord_confirmed_no_effect', 'proven_not_sent',
        'suppressed'
    ));
