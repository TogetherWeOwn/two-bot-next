-- Disposable rehearsal only. Run as the scoped migrator inside a transaction.
-- Retain 0475 and its fence. Refuse to discard any post-expansion evidence.
SET LOCAL lock_timeout = '2s';
SET LOCAL statement_timeout = '15s';
LOCK TABLE public._sqlx_migrations, public._two_bot_migrations,
    public.event_instance_windows, public.internal_action_log, public.web_contract_meta,
    public.voice_heat_hours, public.event_attendance_windows, public.event_voice_sessions,
    public.live_sequence_watermarks IN ACCESS EXCLUSIVE MODE NOWAIT;

DO $undo$
BEGIN
    IF (SELECT array_agg(version ORDER BY version) FROM public._sqlx_migrations
        WHERE version > 473) IS DISTINCT FROM ARRAY[475, 479, 480, 481, 482]::bigint[]
       OR (SELECT array_agg(version ORDER BY version) FROM public._two_bot_migrations
           WHERE version > 465) IS DISTINCT FROM ARRAY[478]::bigint[]
       OR EXISTS (SELECT FROM public._sqlx_migrations WHERE NOT success)
       OR EXISTS (SELECT FROM public._two_bot_migrations WHERE NOT success)
       OR (SELECT contract_version FROM public.web_contract_meta WHERE singleton) IS DISTINCT FROM '1.1'
    THEN
        RAISE EXCEPTION 'unexpected rehearsal schema or ledgers' USING ERRCODE = '23514';
    END IF;
    IF EXISTS (SELECT FROM public.voice_heat_hours)
       OR EXISTS (SELECT FROM public.event_attendance_windows)
       OR EXISTS (SELECT FROM public.event_voice_sessions)
       OR (SELECT is_called FROM public.event_voice_sessions_id_seq)
       OR EXISTS (SELECT FROM public.live_sequence_watermarks)
       OR EXISTS (
           SELECT FROM public.internal_action_log
           WHERE intent_id IS NULL OR action = 'voice.move_member'
              OR delivery_hash IS NOT NULL OR from_channel_id IS NOT NULL OR request_id IS NOT NULL
       )
    THEN
        RAISE EXCEPTION 'new schema contains evidence' USING ERRCODE = '23514';
    END IF;
END
$undo$;

ALTER TABLE public.internal_action_log
    DROP CONSTRAINT internal_action_log_receipt_check,
    DROP CONSTRAINT internal_action_log_outcome_check,
    DROP COLUMN delivery_hash, DROP COLUMN from_channel_id, DROP COLUMN request_id,
    ALTER COLUMN intent_id SET NOT NULL,
    ADD CONSTRAINT internal_action_log_outcome_check CHECK (outcome IN ('created', 'updated', 'cancelled'));
DROP VIEW web_v1.voice_heat;
UPDATE public.web_contract_meta SET contract_version = '1.0' WHERE singleton;
DROP TABLE public.event_voice_sessions, public.event_attendance_windows,
    public.voice_heat_hours, public.live_sequence_watermarks;
DELETE FROM public._sqlx_migrations WHERE version IN (479, 480, 481, 482);
DELETE FROM public._two_bot_migrations WHERE version = 478;
