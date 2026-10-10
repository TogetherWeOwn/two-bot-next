-- Settings CAS conflicts are definitive terminal failures (TOG-19314).
-- A stale expected_version never silently reverts the save that landed in
-- between; the idempotency claim records version_conflict so replays return the
-- same 409 without a second write, audit row or version bump.
ALTER TABLE internal_idempotency DROP CONSTRAINT IF EXISTS internal_idempotency_response_code_check;
ALTER TABLE internal_idempotency ADD CONSTRAINT internal_idempotency_response_code_check
    CHECK (response_code IN (
        'success', 'malformed', 'action_not_allowed', 'discord_rejected', 'no_effect',
        'version_conflict'
    ));

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
            ))
    );
