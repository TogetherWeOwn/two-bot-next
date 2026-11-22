-- Keep the keyed FK and defer validation so the ledger is not scanned under the DDL lock.
ALTER TABLE internal_action_log
    ALTER COLUMN intent_id DROP NOT NULL,
    ADD COLUMN delivery_hash TEXT, ADD COLUMN from_channel_id TEXT, ADD COLUMN request_id TEXT,
    ADD CONSTRAINT internal_action_log_receipt_check CHECK (delivery_hash ~ '^[0-9a-f]{64}$'
        AND from_channel_id ~ '^[0-9]{17,20}$' AND request_id ~ '^[0-9A-HJKMNP-TV-Z]{26}$'
        AND CASE WHEN action = 'voice.move_member' THEN intent_id IS NULL AND phase = 'terminal'
        AND delivery_hash IS NOT NULL AND request_id IS NOT NULL
        AND guild_id IS NOT NULL AND target_id IS NOT NULL AND resource_id IS NOT NULL
        AND outcome IS NOT NULL AND (outcome <> 'moved' OR from_channel_id IS NOT NULL)
        ELSE intent_id IS NOT NULL AND delivery_hash IS NULL END) NOT VALID,
    DROP CONSTRAINT internal_action_log_outcome_check,
    ADD CONSTRAINT internal_action_log_outcome_check CHECK (
        CASE WHEN action = 'voice.move_member' THEN outcome IN ('moved', 'already_in_room', 'not_connected',
        'not_permitted', 'room_full', 'room_gone', 'bot_not_permitted', 'rate_limited', 'discord_unavailable')
        ELSE outcome IN ('created', 'updated', 'cancelled') END) NOT VALID;
