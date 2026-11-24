-- Disposable rehearsal only. One statement keeps the guard and inverse atomic.
-- Retain 0475 and its fence. Refuse to discard any post-expansion evidence.
SET LOCAL statement_timeout = '15s';
DO $undo$
BEGIN
    IF current_database() !~ '^two_bot_test_[a-z0-9_]+$'
       OR current_user::text <> current_database() || '_m'
    THEN
        RAISE EXCEPTION 'invalid rehearsal binding' USING ERRCODE = '23514';
    END IF;
    PERFORM set_config('lock_timeout', '2s', true);
    LOCK TABLE public._sqlx_migrations, public._two_bot_migrations,
        public.event_instance_windows, public.internal_action_log, public.web_contract_meta,
        public.voice_heat_hours, public.event_attendance_windows, public.event_voice_sessions,
        public.live_sequence_watermarks IN ACCESS EXCLUSIVE MODE NOWAIT;

    -- Match the reviewed migration descriptions and SQLx SHA-384 bytes.
    IF (SELECT array_agg(version || ':' || description || ':' || encode(checksum, 'hex') ORDER BY version)
        FROM public._sqlx_migrations WHERE version > 473) IS DISTINCT FROM ARRAY[
        '475:attendance recovery boundary:845e9144067a0104232a6c75b4bcfe9066ff8b18bac69e050334f53deea44e6a4d3e12e8185938acbeaf496b7f3ed486',
        '479:voice heat hours:e54de3741cecaae86a01a0e8cc52fd0f2e42b7519e670263b5dc66f87b86bbea8d552e1fb367289ced247eb96629db97',
        '480:event attendance:fbc662442db73e44fbddd42a470a5795f48c88316cae3512db01c51d6b2fba04dbbbf1206ceb0c5cb1c702c10dc87ffc',
        '481:internal voice move audit:bf82ecee86a8d0d2508cf36a2419f745b6a3a95508c4dce136f893c593e208fccc96f18dd12d3bcee48dd5c1a86e4b7b',
        '482:live sequence watermarks:010c50f08c4f5657ab6386e4c991b702962fe61d0c4a1c4bd3abaf2fa6f4c3c01dea087ff50039974c46e63e1bb87b81'
    ] OR (SELECT array_agg(version || ':' || description || ':' || encode(checksum, 'hex') ORDER BY version)
          FROM public._two_bot_migrations WHERE version > 465) IS DISTINCT FROM ARRAY[
        '478:voice heat hours:e54de3741cecaae86a01a0e8cc52fd0f2e42b7519e670263b5dc66f87b86bbea8d552e1fb367289ced247eb96629db97'
    ]
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
END;
$undo$;
