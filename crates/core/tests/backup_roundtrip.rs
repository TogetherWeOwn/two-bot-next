//! Backup/restore round trip against a scratch database (TOG-9881, TOG-11878).
//!
//! An untested backup is not a backup, and "the command exited 0" is not a
//! test of a backup. What is asserted here is the property that matters
//! during a recovery: dump a database, change it, restore it, and the dumped
//! tables are the same — including the things that are easy to lose and hard
//! to notice: allocator positions, foreign-key chains, timestamptz instants,
//! booleans and NULLs — while the tables a restore must not touch
//! (`NOT_DUMPED`) are left exactly as they were.
//!
//! The current-format round trip runs against the schema the bot really
//! migrates (`crates/cutover/migrations`), not a hand-written stand-in: a
//! stand-in is how restore came to be tested against tables Next does not
//! have (TOG-11878). The legacy fixture keeps a legacy-shaped schema, the
//! only place a legacy-format dump can restore.
//!
//! Requires `TWO_BOT_TEST_DATABASE_URL` pointing at an isolated Postgres
//! database. Only agent-testdb scratch databases may be used, never staging
//! or production (owner directive 2026-09-29). Each test works in its own
//! schema in that database and drops it at the end. Without the variable
//! the test compiles and skips, so `cargo test` stays green on machines
//! without one.
//!
//! Needs the crate `db` feature: `cargo test -p two-bot-core --features db`.

#![cfg(feature = "db")]

use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use two_bot_core::backup::dump::{dump, restore};
use two_bot_core::backup::dump_file::{inspect, DUMP_TABLES, NOT_DUMPED};

/// Rows in every foreign-key chain the dump owns, the allocator tables, and
/// a `guild_settings` row the restore must leave alone.
const SEED: &str = r#"
INSERT INTO events (event_type, member_id, guild_id, occurred_at, source, metadata, idempotency_key) VALUES
  ('member_join', 'm0', 'g1', '2026-08-01T10:00:00Z', 'invite:abc', '{"via":"invite"}', 'e1'),
  ('first_message', 'm0', 'g1', '2026-08-01T10:00:30Z', 'channel:general', NULL, 'e2'),
  ('member_join', NULL, 'g1', '2026-08-02T10:00:00+02:00', 'invite:abc', NULL, 'e3');
INSERT INTO members (guild_id, member_id, joined_at, join_source, is_bot) VALUES
  ('g1', 'm0', '2026-08-01T10:00:00Z', 'invite:abc', FALSE),
  ('g1', 'm1', NULL, NULL, TRUE);
INSERT INTO moderation_idempotency (guild_id, idempotency_key, action, request_hash, state, claimed_at)
  VALUES ('g1', 'k1', 'lock', 'h1', 'completed', '2026-08-01T10:00:00Z');
INSERT INTO moderation_channel_executions (channel_id, guild_id, idempotency_key, claim_token)
  VALUES ('c1', 'g1', 'k1', 'claim-1');
INSERT INTO internal_idempotency (caller_hash, key_hash, action, payload_hash, state, guild_id)
  VALUES (repeat('a', 64), repeat('b', 64), 'role.assign', repeat('c', 64), 'in_flight', '123456789012345678');
INSERT INTO internal_action_log (intent_id, phase, caller_hash, action, guild_id)
  SELECT intent_id, 'intent', caller_hash, action, guild_id FROM internal_idempotency;
INSERT INTO tickets (id, guild_id, opener_id, status, created_at)
  VALUES ('t1', 'g1', 'u1', 'open', '2026-08-04T12:00:00Z');
INSERT INTO ticket_transcripts (ticket_id, guild_id, channel_id, opener_id, content, message_count, created_at, purge_after)
  VALUES ('t1', 'g1', 'c9', 'u1', '', 0, '2026-08-04T13:00:00Z', '2026-09-04T13:00:00Z');
INSERT INTO feed_relays (id, guild_id, channel_id, kind, source, enabled, created_by, created_at, updated_at)
  VALUES ('f1', 'g1', 'c3', 'rss', 'https://example.invalid/feed', FALSE, 'u1', '2026-08-05T12:00:00Z', '2026-08-05T12:00:00Z');
INSERT INTO feed_deliveries (feed_id, item_key, nonce, state, first_seen_at)
  VALUES ('f1', 'item-1', 'n1', 'pending', '2026-08-05T12:01:00Z');
INSERT INTO xp_awards (guild_id, member_id, source, xp, occurred_at) VALUES
  ('g1', 'm0', 'message', 5, '2026-08-06T12:00:00Z'),
  ('g1', 'm0', 'voice', 7, '2026-08-06T12:05:00Z');
-- IDs 3..5 handed out and gone again before the dump: the allocator is
-- ahead of MAX(id), and no restore may hand them out a second time.
INSERT INTO xp_awards (guild_id, member_id, source, xp, occurred_at)
  SELECT 'g1', 'm9', 'message', 1, now() FROM generate_series(1, 3);
DELETE FROM xp_awards WHERE member_id = 'm9';
INSERT INTO guild_settings (guild_id, key, value, updated_by) VALUES ('g1', 'welcome', '"hi"', 'op');
"#;

/// Writes after the dump, in dumped tables (undone by the restore) and in
/// `guild_settings` (kept by it). xp_awards IDs 6..9 are handed out.
const AFTER_DUMP: &str = r#"
DELETE FROM events WHERE idempotency_key = 'e2';
UPDATE members SET is_bot = NOT is_bot;
INSERT INTO moderation_idempotency (guild_id, idempotency_key, action, request_hash, state, claimed_at)
  VALUES ('g1', 'k2', 'lock', 'h2', 'completed', '2026-08-07T10:00:00Z');
INSERT INTO moderation_channel_executions (channel_id, guild_id, idempotency_key, claim_token)
  VALUES ('c2', 'g1', 'k2', 'claim-2');
INSERT INTO xp_awards (guild_id, member_id, source, xp, occurred_at)
  SELECT 'g1', 'm8', 'message', 1, now() FROM generate_series(1, 4);
UPDATE guild_settings SET value = '"changed"', updated_by = 'op2' WHERE key = 'welcome';
"#;

/// The 22 legacy tables, as far as the frozen legacy fixture needs them:
/// its `events` and `join_risk_flags` columns, the rest present but empty.
const LEGACY_SCHEMA: &[(&str, &str)] = &[
    ("events", "id BIGSERIAL PRIMARY KEY, event_type TEXT NOT NULL, member_id TEXT, guild_id TEXT, occurred_at TIMESTAMPTZ NOT NULL, recorded_at TIMESTAMPTZ NOT NULL, source TEXT NOT NULL, metadata TEXT, idempotency_key TEXT"),
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
    ("join_risk_flags", "event_id TEXT PRIMARY KEY, guild_id TEXT NOT NULL, member_id TEXT NOT NULL, account_created_at TIMESTAMPTZ, joined_at TIMESTAMPTZ NOT NULL, source TEXT, score INTEGER NOT NULL, reasons_json TEXT NOT NULL, bulk_join_window BOOLEAN NOT NULL, flagged BOOLEAN NOT NULL, created_at TIMESTAMPTZ NOT NULL"),
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

/// One test's own schema in the scratch database, with every pooled
/// connection pinned to it.
struct Scratch {
    admin: PgPool,
    pool: PgPool,
    schema: String,
}

impl Scratch {
    async fn new(url: &str, tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // Built here from a fixed tag and numbers only: safe to interpolate.
        let schema = format!("backup_{tag}_{}_{nanos}", std::process::id());
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(url)
            .await
            .expect("connect test db");
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .unwrap();
        let path = schema.clone();
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .after_connect(move |conn, _| {
                let path = path.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path', $1, false)")
                        .bind(path)
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
            .connect(url)
            .await
            .expect("connect scratch schema");
        Self {
            admin,
            pool,
            schema,
        }
    }

    async fn finish(self) {
        self.pool.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA {} CASCADE",
            self.schema
        )))
        .execute(&self.admin)
        .await
        .unwrap();
    }
}

fn scratch_dir(tag: &str) -> PathBuf {
    let scratch = PathBuf::from(
        std::env::var_os("PAPERCLIP_RUN_SCRATCH_DIR")
            .expect("database tests require run-owned PAPERCLIP_RUN_SCRATCH_DIR"),
    );
    let dir = scratch.join(format!("two-bot-backup-test-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_legacy_fixture(dir: &Path) -> PathBuf {
    let path = dir.join("legacy-v3-native.gz");
    let mut enc = two_bot_core::backup::dump_file::new_encoder();
    std::io::Write::write_all(&mut enc, include_bytes!("fixtures/legacy-v3-native.ndjson"))
        .unwrap();
    std::fs::write(
        &path,
        two_bot_core::backup::dump_file::finish_gzip(enc).unwrap(),
    )
    .unwrap();
    path
}

/// Canonical text snapshot of the given tables, for before/after comparison.
async fn snapshot(pool: &PgPool, tables: &[&str]) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    for table in tables {
        let cols: Vec<String> = sqlx::query_scalar(
            "SELECT column_name::text FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = $1 ORDER BY ordinal_position",
        )
        .bind(*table)
        .fetch_all(pool)
        .await
        .unwrap();
        assert!(!cols.is_empty(), "{table} exists");
        let select_list = cols
            .iter()
            .map(|c| format!("\"{c}\"::text"))
            .collect::<Vec<_>>()
            .join(", ");
        // Audit: table names come from the crate's constants, column names
        // from the database itself. No values reach SQL text.
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {select_list} FROM {table}"
        )))
        .fetch_all(pool)
        .await
        .unwrap();
        let mut lines: Vec<String> = rows
            .iter()
            .map(|row| {
                (0..cols.len())
                    .map(|i| {
                        row.try_get::<Option<String>, _>(i)
                            .unwrap()
                            .unwrap_or_else(|| "NULL".to_owned())
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect();
        lines.sort();
        out.push(((*table).to_owned(), lines));
    }
    out
}

/// Settings rows with their trigger-allocated counters, the audit trail's
/// length, and the append-only trigger: what a restore must not touch.
async fn settings_state(pool: &PgPool) -> (Vec<(String, String, i64, i64)>, i64, i64) {
    let rows = sqlx::query_as(
        "SELECT key, value::text, version, cas_version FROM guild_settings ORDER BY guild_id, key",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let (audit,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM guild_settings_audit")
        .fetch_one(pool)
        .await
        .unwrap();
    let (triggers,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM pg_trigger \
         WHERE tgrelid = 'guild_settings_audit'::regclass AND NOT tgisinternal",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    (rows, audit, triggers)
}

async fn next_id(pool: &PgPool, insert: &'static str) -> i64 {
    let (id,): (i64,) = sqlx::query_as(insert).fetch_one(pool).await.unwrap();
    id
}

#[tokio::test]
async fn dump_inspect_restore_round_trip() {
    let Some(url) = test_url().await else {
        eprintln!("SKIP backup_roundtrip: TWO_BOT_TEST_DATABASE_URL is not set");
        return;
    };
    let db = Scratch::new(&url, "current").await;
    let pool = &db.pool;
    sqlx::migrate!("../cutover/migrations")
        .run(pool)
        .await
        .expect("migrate the scratch schema");
    sqlx::raw_sql(SEED).execute(pool).await.unwrap();
    let before = snapshot(pool, DUMP_TABLES).await;

    let dir = scratch_dir("current");
    let dump_path: PathBuf = dir.join("two-funnel-test.ndjson.gz");

    // Dump.
    let manifest = dump(pool, &dump_path).await.expect("dump");
    assert_eq!(
        manifest.tables.len(),
        DUMP_TABLES.len(),
        "every owned table"
    );
    assert!(
        manifest
            .tables
            .iter()
            .all(|t| NOT_DUMPED.iter().all(|(n, _)| *n != t.name)),
        "no excluded table is dumped"
    );
    let events = manifest.tables.iter().find(|t| t.name == "events").unwrap();
    assert_eq!(events.count, 3);
    assert_eq!(manifest.events_sequence, 3);
    assert_eq!(
        manifest.sequences.get("xp_awards"),
        Some(&5),
        "the allocator's mark, not MAX(id)"
    );
    assert!(
        manifest.schema_migrations.is_empty(),
        "a migrated Next schema has no legacy ledger"
    );
    assert!(dump_path.exists());

    // Inspect (file-only validation, no database).
    let contents = inspect(&dump_path).expect("inspect");
    assert_eq!(
        contents.rows,
        contents
            .manifest
            .tables
            .iter()
            .map(|t| t.count)
            .sum::<u64>()
    );

    // Keep using the database after the dump, then restore into it as it
    // is: no wipe and no schema edits first (TOG-11878).
    sqlx::raw_sql(AFTER_DUMP).execute(pool).await.unwrap();
    let settings = settings_state(pool).await;
    assert_eq!(settings.2, 1, "append-only trigger installed");
    let report = restore(pool, &dump_path)
        .await
        .expect("restore into a fully migrated database");
    assert!(
        report.ok,
        "every table count matches the manifest: {:?}",
        report.restored
    );
    assert!(
        report.dropped_columns.is_empty(),
        "no dropped columns on identical schema"
    );
    assert_eq!(report.allocators.get("events"), Some(&3));
    assert_eq!(
        report.allocators.get("xp_awards"),
        Some(&9),
        "never rewound below IDs handed out after the dump"
    );

    // Dumped tables are as dumped; the excluded ones as they were left.
    let after = snapshot(pool, DUMP_TABLES).await;
    assert_eq!(before, after, "restored contents equal the dumped contents");
    assert_eq!(
        settings_state(pool).await,
        settings,
        "guild settings, their counters, audit trail and trigger untouched"
    );

    // Every allocator is past the restored rows and everything it ever
    // handed out: the next writes collide with nothing and reuse no ID.
    assert_eq!(
        next_id(pool, "INSERT INTO xp_awards (guild_id, member_id, source, xp, occurred_at) VALUES ('g1', 'm2', 'message', 1, now()) RETURNING id").await,
        10
    );
    assert_eq!(
        next_id(pool, "INSERT INTO events (event_type, guild_id, occurred_at, source, idempotency_key) VALUES ('member_join', 'g1', now(), 'invite:abc', 'e4') RETURNING id").await,
        4
    );
    assert_eq!(
        next_id(pool, "INSERT INTO internal_idempotency (caller_hash, key_hash, action, payload_hash, state) VALUES (repeat('d', 64), repeat('e', 64), 'role.assign', repeat('f', 64), 'in_flight') RETURNING intent_id").await,
        2
    );
    sqlx::raw_sql(
        "DELETE FROM xp_awards WHERE id = 10; DELETE FROM events WHERE id = 4; \
         DELETE FROM internal_idempotency WHERE intent_id = 2;",
    )
    .execute(pool)
    .await
    .unwrap();

    // TOG-9970: the live writer must refuse a source transcript just past its
    // reader's real line budget, without publishing or replacing any archive.
    use two_bot_core::backup::dump_file::MAX_DUMP_LINE_BYTES;
    let valid_bytes = std::fs::read(&dump_path).unwrap();
    // The real row envelope, from PostgreSQL text output, including JSON
    // escaping and the newline. A source body alone is not the line length.
    let (channel_id, created_at, purge_after, message_count): (String, String, String, String) =
        sqlx::query_as("SELECT channel_id, created_at::text, purge_after::text, message_count::text FROM ticket_transcripts WHERE ticket_id = 't1'")
            .fetch_one(pool).await.unwrap();
    let empty_row = serde_json::json!({"kind":"row", "table":"ticket_transcripts", "data": {
        "ticket_id":"t1", "guild_id":"g1", "channel_id":channel_id, "opener_id":"u1",
        "claimed_by":null, "content":"", "message_count":message_count,
        "created_at":created_at, "purge_after":purge_after
    }});
    let body_at_cap =
        MAX_DUMP_LINE_BYTES as usize - serde_json::to_vec(&empty_row).unwrap().len() - 1;
    sqlx::query("UPDATE ticket_transcripts SET content = $1 WHERE ticket_id = 't1'")
        .bind("x".repeat(body_at_cap))
        .execute(pool)
        .await
        .unwrap();
    let boundary_path = dir.join("two-funnel-line-boundary.ndjson.gz");
    dump(pool, &boundary_path)
        .await
        .expect("exact real line cap");
    let boundary = inspect(&boundary_path).unwrap();
    assert_eq!(
        boundary.buffers["ticket_transcripts"][0]["content"]
            .as_str()
            .unwrap()
            .len(),
        body_at_cap
    );
    // One byte beyond the complete-row cap must be refused, not merely the
    // review's much larger transcript beyond the standalone-body cap.
    sqlx::query("UPDATE ticket_transcripts SET content = $1 WHERE ticket_id = 't1'")
        .bind("x".repeat(body_at_cap + 1))
        .execute(pool)
        .await
        .unwrap();
    let oversize_path = dir.join("two-funnel-oversize.ndjson.gz");
    for destination in [&oversize_path, &dump_path] {
        let err = dump(pool, destination).await.unwrap_err();
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
    inspect(&dump_path).unwrap();
    sqlx::query("UPDATE ticket_transcripts SET content = '' WHERE ticket_id = 't1'")
        .execute(pool)
        .await
        .unwrap();

    // A legacy ledger is read when present; an unreadable one refuses rather
    // than aborting the repeatable-read transaction part way through.
    sqlx::raw_sql(
        "CREATE TABLE schema_migrations (id TEXT PRIMARY KEY); \
         INSERT INTO schema_migrations (id) VALUES ('0010_moderation'), ('0001_initial');",
    )
    .execute(pool)
    .await
    .unwrap();
    let ledger = dump(pool, &dir.join("ledger.ndjson.gz")).await.unwrap();
    assert_eq!(
        ledger.schema_migrations,
        ["0001_initial", "0010_moderation"]
    );
    sqlx::raw_sql(
        "DROP TABLE schema_migrations; CREATE TABLE schema_migrations (wrong_column TEXT);",
    )
    .execute(pool)
    .await
    .unwrap();
    let err = dump(pool, &dir.join("bad-ledger.gz")).await.unwrap_err();
    assert!(
        err.to_string().contains("cannot read schema_migrations"),
        "{err}"
    );
    sqlx::query("DROP TABLE schema_migrations")
        .execute(pool)
        .await
        .unwrap();

    // A table outside the dump that references one inside it: refused by
    // name before anything is truncated, rather than failing on the TRUNCATE
    // or (with CASCADE) silently emptying it.
    sqlx::raw_sql(
        "CREATE TABLE local_notes (ticket_id TEXT PRIMARY KEY REFERENCES tickets (id)); \
         INSERT INTO local_notes (ticket_id) VALUES ('t1');",
    )
    .execute(pool)
    .await
    .unwrap();
    let held = snapshot(pool, DUMP_TABLES).await;
    let err = restore(pool, &dump_path).await.unwrap_err();
    assert!(
        err.to_string().contains("local_notes references tickets"),
        "{err}"
    );
    assert_eq!(
        snapshot(pool, DUMP_TABLES).await,
        held,
        "refused restore changes nothing"
    );
    sqlx::query("DROP TABLE local_notes")
        .execute(pool)
        .await
        .unwrap();

    // A legacy-format dump names tables Next never creates: refused by name,
    // before anything is truncated.
    let legacy_path = write_legacy_fixture(&dir);
    let err = restore(pool, &legacy_path).await.unwrap_err().to_string();
    assert!(
        err.contains("target lacks") && err.contains("moderation_warnings"),
        "{err}"
    );
    assert_eq!(
        snapshot(pool, DUMP_TABLES).await,
        held,
        "refused restore changes nothing"
    );

    std::fs::remove_dir_all(&dir).ok();
    db.finish().await;
}

#[tokio::test]
async fn legacy_fixture_restores_into_a_legacy_shaped_schema() {
    let Some(url) = test_url().await else {
        eprintln!("SKIP backup_roundtrip: TWO_BOT_TEST_DATABASE_URL is not set");
        return;
    };
    let db = Scratch::new(&url, "legacy").await;
    let pool = &db.pool;
    // Audit: every interpolated identifier comes from LEGACY_SCHEMA above.
    for (table, ddl) in LEGACY_SCHEMA {
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE TABLE {table} ({ddl})")))
            .execute(pool)
            .await
            .unwrap();
    }
    let dir = scratch_dir("legacy");

    // The dump is of the current schema only, and says what is missing.
    let refused = dir.join("refused.ndjson.gz");
    let err = dump(pool, &refused).await.unwrap_err();
    assert!(
        err.to_string().contains("does not exist in the target"),
        "{err}"
    );
    assert!(!refused.exists());

    // The frozen legacy writer fixture still restores where it belongs.
    let legacy_path = write_legacy_fixture(&dir);
    let legacy = restore(pool, &legacy_path).await.unwrap();
    assert!(legacy.ok);
    assert!(legacy.dropped_columns.is_empty());
    assert_eq!(legacy.allocators.get("events"), Some(&1));
    let event: (i64, Option<String>, String, String) = sqlx::query_as("SELECT id, member_id, metadata, to_char(occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') FROM events")
        .fetch_one(pool).await.unwrap();
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
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(risk, (3, false, true, "[\"new account\"]".to_owned()));
    let (next_legacy_id,): (i64,) = sqlx::query_as("INSERT INTO events (event_type, occurred_at, recorded_at, source) VALUES ('member_join', now(), now(), 'fixture') RETURNING id")
        .fetch_one(pool).await.unwrap();
    assert_eq!(next_legacy_id, 2);

    std::fs::remove_dir_all(&dir).ok();
    db.finish().await;
}
