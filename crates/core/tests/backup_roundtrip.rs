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
    if url.contains("agent-testdb") || url.contains("127.0.0.1") || url.contains("localhost") {
        Some(url)
    } else {
        panic!(
            "TWO_BOT_TEST_DATABASE_URL must point at agent-testdb or loopback (test-containers rule); refusing {url:?}"
        );
    }
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

    let dir = std::env::temp_dir().join(format!("two-bot-backup-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dump_path: PathBuf = dir.join("two-funnel-test.ndjson.gz");

    // Dump.
    let manifest = two_bot_core::backup::dump::dump(&pool, &dump_path)
        .await
        .expect("dump");
    assert_eq!(manifest.tables.len(), 22, "all bot-owned tables dumped");
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

    std::fs::remove_dir_all(&dir).ok();
}
