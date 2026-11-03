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
