-- Opt-in month/day only; calendar dates below describe delivery, never birth years.
-- No automatic production activation. Apply before deploying the birthday runtime.
-- Additive rollback: stop the job and unpublish /birthday; preserve these tables
-- until all role grants and uncertain sends have been reconciled.
CREATE TABLE birthday_profiles (
    guild_id TEXT NOT NULL,
    member_id TEXT NOT NULL,
    birth_month SMALLINT NOT NULL CHECK (birth_month BETWEEN 1 AND 12),
    birth_day SMALLINT NOT NULL CHECK (
        birth_day BETWEEN 1 AND CASE
            WHEN birth_month = 2 THEN 29
            WHEN birth_month IN (4, 6, 9, 11) THEN 30
            ELSE 31 END),
    PRIMARY KEY (guild_id, member_id)
);

CREATE TABLE birthday_config (
    guild_id TEXT PRIMARY KEY,
    channel_id TEXT,
    role_id TEXT,
    cheer TEXT CHECK (char_length(cheer) BETWEEN 1 AND 200)
);

-- Durable pre-wire fence. An unconfirmed post never becomes a fresh send on
-- restart: Discord's nonce cache is finite. Inspect the channel before recovery.
CREATE TABLE birthday_cheers (
    guild_id TEXT NOT NULL,
    member_id TEXT NOT NULL,
    celebration_date DATE NOT NULL,
    channel_id TEXT NOT NULL,
    nonce TEXT NOT NULL,
    message_id TEXT,
    PRIMARY KEY (guild_id, member_id, celebration_date)
);

-- Written BEFORE PUT. Pending/uncertain grants survive /birthday remove and
-- config replacement, so the next sweep can always revoke the exact old role.
CREATE TABLE birthday_role_grants (
    guild_id TEXT NOT NULL,
    member_id TEXT NOT NULL,
    role_id TEXT NOT NULL,
    celebration_date DATE NOT NULL,
    PRIMARY KEY (guild_id, member_id, role_id)
);
