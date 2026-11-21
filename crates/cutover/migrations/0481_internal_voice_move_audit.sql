-- Keep keyless terminal receipts in the private ledger without dropping the keyed FK.
ALTER TABLE internal_action_log
    ALTER COLUMN intent_id DROP NOT NULL,
    ADD COLUMN delivery_hash TEXT CHECK (delivery_hash ~ '^[0-9a-f]{64}$'),
    ADD COLUMN from_channel_id TEXT CHECK (from_channel_id ~ '^[0-9]{17,20}$'),
    ADD COLUMN request_id TEXT CHECK (request_id ~ '^[0-9A-HJKMNP-TV-Z]{26}$'),
    ADD CHECK (CASE WHEN action = 'voice.move_member' THEN intent_id IS NULL AND phase = 'terminal'
        AND delivery_hash IS NOT NULL AND request_id IS NOT NULL
        AND guild_id IS NOT NULL AND target_id IS NOT NULL AND resource_id IS NOT NULL
        AND outcome IS NOT NULL AND (outcome <> 'moved' OR from_channel_id IS NOT NULL)
        ELSE intent_id IS NOT NULL AND delivery_hash IS NULL END) NOT VALID,
    DROP CONSTRAINT internal_action_log_outcome_check,
    ADD CHECK (CASE WHEN action = 'voice.move_member' THEN outcome IN ('moved', 'already_in_room', 'not_connected',
        'not_permitted', 'room_full', 'room_gone', 'bot_not_permitted', 'rate_limited', 'discord_unavailable')
        ELSE outcome IN ('created', 'updated', 'cancelled') END) NOT VALID;
