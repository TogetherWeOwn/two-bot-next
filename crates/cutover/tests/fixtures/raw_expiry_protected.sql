-- Synthetic durable state. No role/grant changes or guard bypasses.
INSERT INTO member_levels (guild_id, member_id, xp, message_xp, updated_at)
VALUES ('111111111111111111', '333333333333333333', 150, 150, '2026-01-01T00:00:00Z'),
       ('222222222222222222', '333333333333333333', 250, 250, '2026-01-01T00:00:00Z');
INSERT INTO member_ranks (guild_id, member_id, updated_at)
VALUES ('111111111111111111', '333333333333333333', '2026-01-01T00:00:00Z'),
       ('222222222222222222', '333333333333333333', '2026-01-01T00:00:00Z');

INSERT INTO events
    (event_type, member_id, guild_id, occurred_at, source, metadata, idempotency_key)
VALUES ('first_message', '333333333333333333', '111111111111111111',
        '2026-01-01T00:00:00Z', 'gateway', '{}', 'raw-retained-first-message');

INSERT INTO guild_settings_audit (guild_id, key, old_value, new_value, actor, at)
VALUES ('111111111111111111', 'WELCOME_CHANNEL_ID', 'null', '"444444444444444444"',
        'retention-fixture', '2026-01-01T00:00:00Z');
INSERT INTO privacy_opt_outs (guild_id, member_id, opted_out_at)
VALUES ('111111111111111111', '333333333333333333', '2026-01-01T00:00:00Z');
INSERT INTO privacy_opt_out_audit (guild_id, member_id, actor, opted_out_at)
VALUES ('111111111111111111', '333333333333333333', 'retention-fixture', '2026-01-01T00:00:00Z');
INSERT INTO guild_analytics_settings
    (guild_id, presence_enabled, activity_enabled, voice_enabled, updated_at)
VALUES ('111111111111111111', FALSE, FALSE, FALSE, '2026-01-01T00:00:00Z');

INSERT INTO moderation_idempotency
    (guild_id, idempotency_key, action, request_hash, state, outcome, claimed_at, result_json)
VALUES ('111111111111111111', 'raw-retained-release', 'warn', 'operator_released',
        'done', 'operator_released', '2026-01-01T00:00:00Z', '{}');
INSERT INTO moderation_channel_executions
    (channel_id, guild_id, idempotency_key, claim_token)
VALUES ('444444444444444444', '111111111111111111', 'raw-retained-release', 'operator_released');
INSERT INTO internal_idempotency
    (caller_hash, key_hash, action, payload_hash, state, guild_id, actor_id, target_id)
VALUES (repeat('a', 64), repeat('b', 64), 'role.assign', repeat('c', 64),
        'unknown', '111111111111111111', '333333333333333333', '333333333333333333');

INSERT INTO retention_deletion_requests
    (request_id, guild_id, user_id, requested_at, status, completed_at, receipt_id)
VALUES ('raw-retained-receipt', '222222222222222222', '555555555555555555',
        '2026-01-01T00:00:00Z', 'completed', '2026-01-02T00:00:00Z', 'raw-retained-receipt-artifact');
