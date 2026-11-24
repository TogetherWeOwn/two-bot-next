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
END;
$undo$;
