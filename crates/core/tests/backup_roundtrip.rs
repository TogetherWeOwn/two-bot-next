//! Backup/restore round trip against a scratch database (TOG-9881).
//!
//! An untested backup is not a backup, and "the command exited 0" is not a
//! test of a backup. What is asserted here is the property that matters
//! during a recovery: dump a database, wipe it, restore it, and the contents
//! are the same — including the things that are easy to lose and hard to
//! notice: the `events` id sequence, timestamptz instants, jsonb payloads,
//! bytea blobs, and NULLs.
//!
//! Requires `TWO_BOT_TEST_DATABASE_URL` pointing at an isolated Postgres
//! database. Only agent-testdb scratch databases may be used, never staging
//! or production (owner directive 2026-09-29). Without the variable the test
//! compiles and skips, so `cargo test` stays green on machines without one.
//!
//! Needs the crate `db` feature: `cargo test -p two-bot-core --features db`.

#![cfg(feature = "db")]

use sqlx::{PgPool, Row};
use std::path::PathBuf;

/// All 22 bot-owned tables, with the real legacy type surface represented:
/// bigserial ids, text, timestamptz, booleans, integers, jsonb, bytea, and
/// nullable columns. Column names per table match the legacy dump's stable
/// read order (`orderFor`), so the test exercises the real ORDER BY paths.
const SCHEMA: &[(&str, &str)] = &[
    ("events", "id BIGSERIAL PRIMARY KEY, guild_id TEXT NOT NULL, member_id TEXT NOT NULL, event_type TEXT NOT NULL, occurred_at TIMESTAMPTZ NOT NULL, source TEXT, payload JSONB, seen BOOLEAN NOT NULL DEFAULT TRUE"),
    ("members", "guild_id TEXT NOT NULL, member_id TEXT NOT NULL, first_seen TIMESTAMPTZ, message_count INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (guild_id, member_id)"),
    ("invite_snapshots", "guild_id TEXT NOT NULL, code TEXT NOT NULL, uses INTEGER NOT NULL DEFAULT 0, captured_at TIMESTAMPTZ NOT NULL, PRIMARY KEY (guild_id, code)"),
    ("operational_audit_log", "entry_id TEXT PRIMARY KEY, created_at TIMESTAMPTZ NOT NULL, delivered BOOLEAN NOT NULL DEFAULT FALSE, detail JSONB"),
    ("moderation_warnings", "id BIGSERIAL PRIMARY KEY, guild_id TEXT NOT NULL, user_id TEXT NOT NULL, reason TEXT NOT NULL, created_at TIMESTAMPTZ NOT NULL"),
    ("moderation_scheduled_unbans", "request_id TEXT PRIMARY KEY, guild_id TEXT NOT NULL, user_id TEXT NOT NULL, execute_at TIMESTAMPTZ NOT NULL"),
    ("moderation_audit", "request_id TEXT PRIMARY KEY, guild_id TEXT NOT NULL, action TEXT NOT NULL, created_at TIMESTAMPTZ NOT NULL"),
    ("moderation_lockdowns", "guild_id TEXT NOT NULL, channel_id TEXT NOT NULL, locked_at TIMESTAMPTZ NOT NULL, PRIMARY KEY (guild_id, channel_id)"),
    ("moderation_idempotency", "guild_id TEXT NOT NULL, idempotency_key TEXT NOT NULL, created_at TIMESTAMPTZ NOT NULL, PRIMARY KEY (guild_id, idempotency_key)"),
    ("containment_events", "audit_entry_id TEXT PRIMARY KEY, guild_id TEXT NOT NULL, occurred_at TIMESTAMPTZ NOT NULL, weight INTEGER NOT NULL"),
    ("containment_incidents", "id BIGSERIAL PRIMARY KEY, guild_id TEXT NOT NULL, started_at TIMESTAMPTZ NOT NULL, closed BOOLEAN NOT NULL DEFAULT FALSE"),
    ("join_risk_flags", "event_id TEXT PRIMARY KEY, guild_id TEXT NOT NULL, joined_at TIMESTAMPTZ NOT NULL, score INTEGER NOT NULL"),
    ("automation_commands", "guild_id TEXT NOT NULL, name TEXT NOT NULL, template TEXT NOT NULL, enabled BOOLEAN NOT NULL DEFAULT TRUE, PRIMARY KEY (guild_id, name)"),
    ("scheduled_messages", "id TEXT PRIMARY KEY, guild_id TEXT NOT NULL, body TEXT NOT NULL, next_run_at TIMESTAMPTZ"),
    ("sticky_messages", "guild_id TEXT NOT NULL, channel_id TEXT NOT NULL, body TEXT NOT NULL, PRIMARY KEY (guild_id, channel_id)"),
    ("automation_audit_log", "id BIGSERIAL PRIMARY KEY, guild_id TEXT NOT NULL, created_at TIMESTAMPTZ NOT NULL, detail TEXT"),
    ("tickets", "id TEXT PRIMARY KEY, guild_id TEXT NOT NULL, created_at TIMESTAMPTZ NOT NULL, status TEXT NOT NULL"),
    ("ticket_transcripts", "ticket_id TEXT PRIMARY KEY, guild_id TEXT NOT NULL, created_at TIMESTAMPTZ NOT NULL, body TEXT, blob BYTEA"),
    ("automod_violations", "guild_id TEXT NOT NULL, user_id TEXT NOT NULL, count INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (guild_id, user_id)"),
    ("automod_processed_messages", "guild_id TEXT NOT NULL, message_id TEXT NOT NULL, processed_at TIMESTAMPTZ NOT NULL, PRIMARY KEY (guild_id, message_id)"),
    ("self_role_audit", "event_id TEXT PRIMARY KEY, guild_id TEXT NOT NULL, created_at TIMESTAMPTZ NOT NULL"),
    ("self_role_panel_claims", "guild_id TEXT NOT NULL, member_id TEXT NOT NULL, panel_id TEXT NOT NULL, claimed_at TIMESTAMPTZ NOT NULL, PRIMARY KEY (guild_id, member_id, panel_id)"),
];

async fn test_url() -> Option<String> {
    let url = std::env::var("TWO_BOT_TEST_DATABASE_URL").ok()?;
    let url = url.trim().to_owned();
    if url.is_empty() {
        return None;
    }
    let parsed = url::Url::parse(&url).expect("test database URL");
    assert_eq!(
        parsed.host_str(),
        Some("agent-testdb"),
        "tests only use the agent-testdb container"
    );
    assert_eq!(
        parsed.username(),
        "agent_test",
        "tests only use the test principal"
    );
    assert_eq!(parsed.port().unwrap_or(5432), 5432, "test DB port only");
    assert!(
        parsed.password().unwrap_or("").is_empty(),
        "empty test password only"
    );
    assert!(
        parsed.query().is_none(),
        "no query-string credential or host overrides"
    );
    assert!(
        parsed
            .path()
            .trim_start_matches('/')
            .starts_with("two_next_backup"),
        "use a dedicated two_next_backup scratch database"
    );
    Some(url)
}

async fn build_schema(pool: &PgPool) {
    // Audit for the AssertSqlSafe wraps below: every interpolated identifier
    // comes from the SCHEMA const in this file — no file, env or network
    // input reaches SQL text. Row values travel only as bound parameters.
    for (table, _ddl) in SCHEMA {
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP TABLE IF EXISTS {table}")))
            .execute(pool)
            .await
            .unwrap();
    }
    sqlx::query("DROP TABLE IF EXISTS schema_migrations")
        .execute(pool)
        .await
        .unwrap();
    for (table, ddl) in SCHEMA {
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE TABLE {table} ({ddl})")))
            .execute(pool)
            .await
            .unwrap();
    }
    // This fixture intentionally exercises the legacy type surface rather than
    // migrations. Complete-schema/FK coverage lives in backup_schema_roundtrip.
    for table in two_bot_core::backup::dump_file::DUMP_TABLES {
        if !SCHEMA.iter().any(|(name, _)| name == table) {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "CREATE TABLE IF NOT EXISTS {table} (id BIGINT PRIMARY KEY)"
            )))
            .execute(pool)
            .await
            .unwrap();
        }
    }
    sqlx::query("CREATE TABLE schema_migrations (id TEXT PRIMARY KEY)")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO schema_migrations (id) VALUES ('0001_initial'), ('0010_moderation')")
        .execute(pool)
        .await
        .unwrap();
}

async fn seed(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO events (guild_id, member_id, event_type, occurred_at, source, payload, seen) VALUES \
         ('g1', 'm0', 'member_join', '2026-08-01T10:00:00Z', 'invite:abc', '{\"via\": \"invite\"}', TRUE), \
         ('g1', 'm0', 'first_message', '2026-08-01T10:00:30Z', 'channel:general', NULL, TRUE), \
         ('g1', 'm1', 'member_join', '2026-08-02T10:00:00+02:00', NULL, '{}', FALSE)",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO members (guild_id, member_id, first_seen, message_count) VALUES \
         ('g1', 'm0', '2026-08-01T10:00:00Z', 41), ('g1', 'm1', NULL, 0)",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO invite_snapshots (guild_id, code, uses, captured_at) VALUES ('g1', 'abc', 7, '2026-08-01T09:00:00Z')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO operational_audit_log (entry_id, created_at, delivered, detail) VALUES ('e1', '2026-08-01T10:01:00Z', TRUE, '{\"k\": 1}')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO moderation_warnings (guild_id, user_id, reason, created_at) VALUES ('g1', 'm9', 'spam', '2026-08-03T12:00:00Z')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO moderation_scheduled_unbans (request_id, guild_id, user_id, execute_at) VALUES ('r1', 'g1', 'm9', '2026-08-10T12:00:00Z')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO tickets (id, guild_id, created_at, status) VALUES ('t1', 'g1', '2026-08-04T12:00:00Z', 'open')",
    )
    .execute(pool)
    .await
    .unwrap();
    // Bytea blob + NULL body: the binary and the absent value must both survive.
    sqlx::query("INSERT INTO ticket_transcripts (ticket_id, guild_id, created_at, body, blob) VALUES ('t1', 'g1', '2026-08-04T13:00:00Z', NULL, '\\xdeadbeef')")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO self_role_panel_claims (guild_id, member_id, panel_id, claimed_at) VALUES ('g1', 'm0', 'p1', '2026-08-05T12:00:00Z')",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// Canonical text snapshot of every table, for before/after comparison.
async fn snapshot_all(pool: &PgPool) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    for (table, _ddl) in SCHEMA {
        let order = match *table {
            "events" => "id",
            "members" => "guild_id, member_id",
            "invite_snapshots" => "guild_id, code",
            "operational_audit_log" => "entry_id",
            "moderation_warnings" => "created_at, id",
            "moderation_scheduled_unbans" => "execute_at, request_id",
            "moderation_audit" => "created_at, request_id",
            "moderation_lockdowns" => "guild_id, channel_id",
            "moderation_idempotency" => "guild_id, idempotency_key",
            "containment_events" => "occurred_at, audit_entry_id",
            "containment_incidents" => "started_at, id",
            "join_risk_flags" => "joined_at, event_id",
            "automation_commands" => "guild_id, name",
            "scheduled_messages" => "guild_id, id",
            "sticky_messages" => "guild_id, channel_id",
            "automation_audit_log" => "created_at, id",
            "tickets" => "created_at, id",
            "ticket_transcripts" => "created_at, ticket_id",
            "automod_violations" => "guild_id, user_id",
            "automod_processed_messages" => "guild_id, message_id",
            "self_role_audit" => "created_at, event_id",
            "self_role_panel_claims" => "guild_id, member_id, panel_id",
            _ => "1",
        };
        let cols: Vec<(String,)> = sqlx::query_as(
            "SELECT column_name FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = $1 ORDER BY ordinal_position",
        )
        .bind(*table)
        .fetch_all(pool)
        .await
        .unwrap();
        let select_list = cols
            .iter()
            .map(|(c,)| format!("\"{c}\"::text"))
            .collect::<Vec<_>>()
            .join(", ");
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {select_list} FROM {table} ORDER BY {order}"
        )))
        .fetch_all(pool)
        .await
        .unwrap();
        let mut lines = Vec::new();
        for row in rows {
            let cells: Vec<String> = (0..cols.len())
                .map(|i| {
                    row.try_get::<Option<String>, _>(i)
                        .unwrap()
                        .unwrap_or_else(|| "NULL".to_owned())
                })
                .collect();
            lines.push(cells.join("|"));
        }
        out.push(((*table).to_owned(), lines));
    }
    out
}

#[tokio::test]
async fn dump_inspect_restore_round_trip() {
    let Some(url) = test_url().await else {
        eprintln!("SKIP backup_roundtrip: TWO_BOT_TEST_DATABASE_URL is not set");
        return;
    };
    let pool = PgPool::connect(&url).await.expect("connect test db");
    build_schema(&pool).await;
    seed(&pool).await;
    let before = snapshot_all(&pool).await;

    let scratch = PathBuf::from(
        std::env::var_os("PAPERCLIP_RUN_SCRATCH_DIR")
            .expect("database tests require run-owned PAPERCLIP_RUN_SCRATCH_DIR"),
    );
    let dir = scratch.join(format!("two-bot-backup-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dump_path: PathBuf = dir.join("two-funnel-test.ndjson.gz");

    // Dump.
    let manifest = two_bot_core::backup::dump::dump(&pool, &dump_path)
        .await
        .expect("dump");
    assert_eq!(
        manifest.tables.len(),
        two_bot_core::backup::dump_file::DUMP_TABLES.len(),
        "all covered tables dumped"
    );
    let events = manifest.tables.iter().find(|t| t.name == "events").unwrap();
    assert_eq!(events.count, 3);
    assert!(dump_path.exists());

    // Inspect (file-only validation, no database).
    let contents = two_bot_core::backup::dump_file::inspect(&dump_path).expect("inspect");
    assert_eq!(
        contents.rows,
        contents
            .manifest
            .tables
            .iter()
            .map(|t| t.count)
            .sum::<u64>()
    );

    // Wipe everything, then restore.
    for (table, _ddl) in SCHEMA {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "TRUNCATE {table} RESTART IDENTITY"
        )))
        .execute(&pool)
        .await
        .unwrap();
    }
    let report = two_bot_core::backup::dump::restore(&pool, &dump_path)
        .await
        .expect("restore");
    assert!(
        report.ok,
        "every table count matches the manifest: {:?}",
        report.restored
    );
    assert!(
        report.dropped_columns.is_empty(),
        "no dropped columns on identical schema"
    );

    // Contents identical.
    let after = snapshot_all(&pool).await;
    assert_eq!(before, after, "restored contents equal the dumped contents");

    // The events id sequence is past the restored high-water mark: the next
    // write must not collide with a row we just put back.
    let next: (i64,) = sqlx::query_as(
        "INSERT INTO events (guild_id, member_id, event_type, occurred_at) \
         VALUES ('g1', 'm2', 'member_join', now()) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(next.0 > 3, "sequence resumed past the restored max id");

    // TOG-9970: the live writer must refuse a source transcript just past its
    // reader's real line budget, without publishing or replacing any archive.
    use two_bot_core::backup::dump_file::MAX_DUMP_LINE_BYTES;
    let valid_bytes = std::fs::read(&dump_path).unwrap();
    // Derive the real row envelope from PostgreSQL text output, including JSON
    // escaping and the newline. A source body alone is not the line length.
    let (ticket_id, guild_id, created_at, blob): (String, String, String, String) =
        sqlx::query_as("SELECT ticket_id, guild_id, created_at::text, blob::text FROM ticket_transcripts WHERE ticket_id = 't1'")
            .fetch_one(&pool).await.unwrap();
    let empty_row = serde_json::json!({"kind":"row", "table":"ticket_transcripts", "data": {
        "ticket_id":ticket_id, "guild_id":guild_id, "created_at":created_at, "body":"", "blob":blob
    }});
    let body_at_cap =
        MAX_DUMP_LINE_BYTES as usize - serde_json::to_vec(&empty_row).unwrap().len() - 1;
    sqlx::query("UPDATE ticket_transcripts SET body = $1 WHERE ticket_id = 't1'")
        .bind("x".repeat(body_at_cap))
        .execute(&pool)
        .await
        .unwrap();
    let boundary_path = dir.join("two-funnel-line-boundary.ndjson.gz");
    two_bot_core::backup::dump::dump(&pool, &boundary_path)
        .await
        .expect("exact real line cap");
    let boundary = two_bot_core::backup::dump_file::inspect(&boundary_path).unwrap();
    assert_eq!(
        boundary.buffers["ticket_transcripts"][0]["body"]
            .as_str()
            .unwrap()
            .len(),
        body_at_cap
    );
    // One byte beyond the complete-row cap must be refused, not merely the
    // review's much larger transcript beyond the standalone-body cap.
    sqlx::query("UPDATE ticket_transcripts SET body = $1 WHERE ticket_id = 't1'")
        .bind("x".repeat(body_at_cap + 1))
        .execute(&pool)
        .await
        .unwrap();
    let oversize_path = dir.join("two-funnel-oversize.ndjson.gz");
    for destination in [&oversize_path, &dump_path] {
        let err = two_bot_core::backup::dump::dump(&pool, destination)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("decoded line exceeds"), "{err}");
        assert!(
            !oversize_path.exists(),
            "oversized source must not be published"
        );
        assert_eq!(
            std::fs::read(&dump_path).unwrap(),
            valid_bytes,
            "prior recovery point survives"
        );
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            2,
            "failed temporary is removed"
        );
    }
    two_bot_core::backup::dump_file::inspect(&dump_path).unwrap();
    sqlx::query("UPDATE ticket_transcripts SET body = NULL WHERE ticket_id = 't1'")
        .execute(&pool)
        .await
        .unwrap();

    // Optional migration ledger must not abort the repeatable-read transaction.
    sqlx::query("DROP TABLE schema_migrations")
        .execute(&pool)
        .await
        .unwrap();
    let missing_ledger_path = dir.join("no-ledger.ndjson.gz");
    let no_ledger = two_bot_core::backup::dump::dump(&pool, &missing_ledger_path)
        .await
        .unwrap();
    assert!(no_ledger.schema_migrations.is_empty());
    let inspected = two_bot_core::backup::dump_file::inspect(&missing_ledger_path).unwrap();
    assert!(
        inspected.rows > 0,
        "table pages still readable without ledger"
    );
    sqlx::query("CREATE TABLE schema_migrations (wrong_column TEXT)")
        .execute(&pool)
        .await
        .unwrap();
    let err = two_bot_core::backup::dump::dump(&pool, &dir.join("bad-ledger.gz"))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("cannot read schema_migrations"),
        "{err}"
    );

    // The actual frozen writer fixture uses the legacy events/risk-flag columns.
    // Other tables stay present but empty, as declared in this minimal fixture.
    sqlx::query("DROP TABLE events")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE events (id BIGSERIAL PRIMARY KEY, event_type TEXT NOT NULL, member_id TEXT, guild_id TEXT, occurred_at TIMESTAMPTZ NOT NULL, recorded_at TIMESTAMPTZ NOT NULL, source TEXT NOT NULL, metadata TEXT, idempotency_key TEXT)")
        .execute(&pool).await.unwrap();
    sqlx::query("DROP TABLE join_risk_flags")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE join_risk_flags (event_id TEXT PRIMARY KEY, guild_id TEXT NOT NULL, member_id TEXT NOT NULL, account_created_at TIMESTAMPTZ, joined_at TIMESTAMPTZ NOT NULL, source TEXT, score INTEGER NOT NULL, reasons_json TEXT NOT NULL, bulk_join_window BOOLEAN NOT NULL, flagged BOOLEAN NOT NULL, created_at TIMESTAMPTZ NOT NULL)")
        .execute(&pool).await.unwrap();
    let legacy_path = dir.join("legacy-v3-native.gz");
    let mut enc = two_bot_core::backup::dump_file::new_encoder();
    std::io::Write::write_all(&mut enc, include_bytes!("fixtures/legacy-v3-native.ndjson"))
        .unwrap();
    std::fs::write(
        &legacy_path,
        two_bot_core::backup::dump_file::finish_gzip(enc).unwrap(),
    )
    .unwrap();
    let legacy = two_bot_core::backup::dump::restore(&pool, &legacy_path)
        .await
        .unwrap();
    assert!(legacy.ok);
    assert!(legacy.dropped_columns.is_empty());
    let event: (i64, Option<String>, String, String) = sqlx::query_as("SELECT id, member_id, metadata, to_char(occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') FROM events")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(
        event,
        (
            1,
            None,
            "{\"channelId\":\"c1\"}".to_owned(),
            "2026-08-01 12:00:00".to_owned()
        )
    );
    let risk: (i32, bool, bool, String) = sqlx::query_as(
        "SELECT score, bulk_join_window, flagged, reasons_json FROM join_risk_flags",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(risk, (3, false, true, "[\"new account\"]".to_owned()));
    let (next_legacy_id,): (i64,) = sqlx::query_as("INSERT INTO events (event_type, occurred_at, recorded_at, source) VALUES ('member_join', now(), now(), 'fixture') RETURNING id")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(next_legacy_id, 2);

    std::fs::remove_dir_all(&dir).ok();
}
