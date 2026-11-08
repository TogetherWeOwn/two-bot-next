-- Self-only voice moves (`voice.move_member`) audit through the
-- internal-action log without claiming an idempotency key: repeats
-- re-evaluate live voice state and converge on the `already_in_room` no-op,
-- so a key would add nothing but replay surface. The claim and audit rows
-- gain the source/target channel witness and the request id; the verb CHECK
-- gains the new verb; the outcome CHECKs gain its nine audit outcomes; and
-- the completed-state CHECK gains its two refusal combos.
-- Existing rows stay NULL: a digest cannot reconstruct channels the move
-- columns never recorded.
-- The CHECKs below add as NOT VALID and validate separately so the
-- widened constraints never scan and lock live tables on add.
-- Backout: drop the six columns (voice-move audits lose their channel and
-- request witness; the rows keep caller, verb, subject and outcome).
ALTER TABLE internal_idempotency DROP CONSTRAINT internal_idempotency_action_check;
ALTER TABLE internal_idempotency ADD CONSTRAINT internal_idempotency_action_check
    CHECK (action IN (
        'role.assign', 'guild.add_member', 'announcement.post',
        'event.upsert', 'event.cancel', 'event.read',
        'automations.import', 'automations.export', 'settings.get', 'settings.set',
        'moderation.ban', 'moderation.tempban', 'moderation.kick',
        'moderation.timeout', 'moderation.warn', 'moderation.purge',
        'moderation.slowmode', 'moderation.lockdown', 'moderation.unlock',
        'voice.move_member'
    )) NOT VALID;

ALTER TABLE internal_idempotency
    ADD COLUMN from_channel_id TEXT CHECK (from_channel_id ~ '^[0-9]{17,20}$'),
    ADD COLUMN to_channel_id TEXT CHECK (to_channel_id ~ '^[0-9]{17,20}$'),
    ADD COLUMN request_id TEXT CHECK (request_id ~ '^[0-9A-HJKMNP-TV-Z]{26}$');
ALTER TABLE internal_action_log
    ADD COLUMN from_channel_id TEXT CHECK (from_channel_id ~ '^[0-9]{17,20}$'),
    ADD COLUMN to_channel_id TEXT CHECK (to_channel_id ~ '^[0-9]{17,20}$'),
    ADD COLUMN request_id TEXT CHECK (request_id ~ '^[0-9A-HJKMNP-TV-Z]{26}$');

ALTER TABLE internal_idempotency DROP CONSTRAINT internal_idempotency_outcome_check;
ALTER TABLE internal_idempotency ADD CONSTRAINT internal_idempotency_outcome_check
    CHECK (outcome IN (
        'created', 'updated', 'cancelled',
        'moved', 'already_in_room', 'not_connected', 'not_permitted',
        'room_full', 'room_gone', 'bot_not_permitted',
        'rate_limited', 'discord_unavailable'
    )) NOT VALID;
ALTER TABLE internal_action_log DROP CONSTRAINT internal_action_log_outcome_check;
ALTER TABLE internal_action_log ADD CONSTRAINT internal_action_log_outcome_check
    CHECK (outcome IN (
        'created', 'updated', 'cancelled',
        'moved', 'already_in_room', 'not_connected', 'not_permitted',
        'room_full', 'room_gone', 'bot_not_permitted',
        'rate_limited', 'discord_unavailable'
    )) NOT VALID;

ALTER TABLE internal_idempotency DROP CONSTRAINT IF EXISTS internal_idempotency_response_code_check;
ALTER TABLE internal_idempotency ADD CONSTRAINT internal_idempotency_response_code_check
    CHECK (response_code IN (
        'success', 'malformed', 'action_not_allowed', 'discord_rejected', 'no_effect',
        'version_conflict', 'rate_limited', 'discord_unavailable'
    )) NOT VALID;

ALTER TABLE internal_idempotency DROP CONSTRAINT IF EXISTS internal_idempotency_check;
ALTER TABLE internal_idempotency DROP CONSTRAINT IF EXISTS internal_idempotency_check1;
ALTER TABLE internal_idempotency ADD CONSTRAINT internal_idempotency_check
    CHECK (
        (state <> 'completed' AND response_code IS NULL AND http_status IS NULL
            AND resource_id IS NULL AND affected IS NULL)
        OR (state = 'completed' AND response_code IS NOT NULL AND http_status IS NOT NULL
            AND (
                (response_code = 'success' AND http_status = 200 AND affected IS NOT NULL)
                OR (response_code = 'malformed' AND http_status = 400 AND resource_id IS NULL AND affected IS NULL)
                OR (response_code = 'action_not_allowed' AND http_status = 403 AND resource_id IS NULL AND affected IS NULL)
                OR (response_code = 'discord_rejected' AND http_status = 422 AND resource_id IS NULL AND affected IS NULL)
                OR (response_code = 'no_effect' AND http_status = 502 AND resource_id IS NULL AND affected IS NULL)
                OR (response_code = 'version_conflict' AND http_status = 409 AND resource_id IS NULL AND affected IS NULL)
                OR (response_code = 'rate_limited' AND http_status = 429 AND resource_id IS NULL AND affected IS NULL)
                OR (response_code = 'discord_unavailable' AND http_status = 502 AND resource_id IS NULL AND affected IS NULL)
            ))
    ) NOT VALID;

ALTER TABLE internal_idempotency VALIDATE CONSTRAINT internal_idempotency_action_check;
ALTER TABLE internal_idempotency VALIDATE CONSTRAINT internal_idempotency_outcome_check;
ALTER TABLE internal_action_log VALIDATE CONSTRAINT internal_action_log_outcome_check;
ALTER TABLE internal_idempotency VALIDATE CONSTRAINT internal_idempotency_response_code_check;
ALTER TABLE internal_idempotency VALIDATE CONSTRAINT internal_idempotency_check;
