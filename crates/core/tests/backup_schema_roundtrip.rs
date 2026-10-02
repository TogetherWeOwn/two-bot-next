//! Complete-schema backup acceptance on the actual cutover migrations.
//!
//! Default (not ignored) tests; CI's integration step enables `db` and supplies
//! TWO_TEST_DATABASE_URL. Only an absent variable skips: invalid configuration,
//! failed migrations and connection errors fail through TestDatabase's guards.
//! No placeholder legacy tables or application/staging database connections.

#![cfg(feature = "db")]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use sqlx::{PgPool, Row};
use two_bot_core::backup::dump::{dump, restore, DbDumpError};
use two_bot_core::backup::dump_file::{
    finish_gzip, inspect, is_destination_owned, new_encoder, write_line, DumpContents,
    DumpTableInfo, SequenceMark, DESTINATION_OWNED_COLUMNS, DUMP_TABLES, DUMP_VERSION,
    EXCLUDED_TABLES, OPTIONAL_LEGACY_TABLES,
};
use two_bot_testsupport::TestDatabase;

async fn database() -> Option<TestDatabase> {
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(std::env::VarError::NotPresent) => {
            eprintln!("SKIP backup_schema_roundtrip: TWO_TEST_DATABASE_URL is not set");
            return None;
        }
        Err(error) => panic!("invalid test bootstrap configuration: {error}"),
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create and migrate isolated test database"),
    )
}

struct ArchiveDirectory(PathBuf);

impl ArchiveDirectory {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        // Same fallback as backup::dump_file's publication tests, not a literal
        // /tmp directory. Controller runs keep every archive in run-owned scratch.
        let root = std::env::var_os("PAPERCLIP_RUN_SCRATCH_DIR")
            .or_else(|| std::env::var_os("RUNNER_TEMP"))
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = root.join(format!(
            "two-bot-backup-schema-{}-{stamp}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).expect("exclusively create test archive directory");
        Self(directory)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for ArchiveDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

// Audit for AssertSqlSafe below: identifiers come from the isolated database's
// catalog and are escaped, or from checked-in allowlists/literal test SQL. The
// fixture is checked-in SQL; no URL, archive cell or external input becomes SQL.
fn audited(sql: String) -> sqlx::AssertSqlSafe<String> {
    sqlx::AssertSqlSafe(sql)
}

async fn actual_tables(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = current_schema() AND c.relkind IN ('r', 'p', 'f') \
         ORDER BY c.relname::text COLLATE \"C\"",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn covered_tables(pool: &PgPool) -> Vec<String> {
    actual_tables(pool)
        .await
        .into_iter()
        .filter(|name| DUMP_TABLES.contains(&name.as_str()))
        .collect()
}

async fn seed(pool: &PgPool) {
    // raw_sql is the documented multi-statement API, executed as a transaction.
    // https://docs.rs/sqlx/0.9.0/sqlx/fn.raw_sql.html
    sqlx::raw_sql(include_str!("fixtures/backup_schema_seed.sql"))
        .execute(pool)
        .await
        .expect("seed every active migrated backup table with valid rows");
    sqlx::raw_sql(
        "CREATE TABLE schema_migrations (id TEXT PRIMARY KEY); \
         INSERT INTO schema_migrations VALUES ('legacy-ledger-diagnostic'), ('target-ledger'); \
         ALTER TABLE members ADD COLUMN backup_identity BIGINT GENERATED ALWAYS AS IDENTITY \
             (START WITH 17 INCREMENT BY 3) UNIQUE; \
         INSERT INTO members (guild_id, member_id, join_source, backup_identity) \
             OVERRIDING SYSTEM VALUE VALUES \
             ('100000000000000001', 'identity-source-member', 'explicit identity', 313);",
    )
    .execute(pool)
    .await
    .unwrap();
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TableSnapshot {
    columns: Vec<(String, String)>,
    // Keep actual NULL distinct from the text "NULL" and avoid delimiter-based
    // comparisons (fixtures also contain pipes, newlines, Unicode and JSON text).
    rows: Vec<Vec<Option<String>>>,
}

type Snapshot = BTreeMap<String, TableSnapshot>;

/// Every archived column. Destination-owned columns are compared separately:
/// restore allocates them afresh by design (see [`assert_fresh_cas_tokens`]).
async fn snapshot(pool: &PgPool, tables: &[String]) -> Snapshot {
    let mut result = BTreeMap::new();
    for table in tables {
        let mut columns: Vec<(String, String)> = sqlx::query_as(
            "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod) \
             FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = current_schema() AND c.relname = $1 \
               AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum",
        )
        .bind(table)
        .fetch_all(pool)
        .await
        .unwrap();
        assert!(!columns.is_empty(), "table {table} must exist");
        columns.retain(|(name, _)| !is_destination_owned(table, name));
        let cells = columns
            .iter()
            .map(|(name, _)| format!("{}::text", identifier(name)))
            .collect::<Vec<_>>();
        let order = cells
            .iter()
            .map(|cell| format!("{cell} COLLATE \"C\" NULLS LAST"))
            .collect::<Vec<_>>()
            .join(", ");
        let rows = sqlx::query(audited(format!(
            "SELECT {} FROM {} ORDER BY {order}",
            cells.join(", "),
            identifier(table)
        )))
        .fetch_all(pool)
        .await
        .unwrap()
        .iter()
        .map(|row| {
            (0..columns.len())
                .map(|index| row.get::<Option<String>, _>(index))
                .collect()
        })
        .collect();
        result.insert(table.clone(), TableSnapshot { columns, rows });
    }
    result
}

fn assert_archive_matches(contents: &DumpContents, expected: &Snapshot) {
    let names: BTreeSet<_> = contents
        .manifest
        .tables
        .iter()
        .map(|table| table.name.as_str())
        .collect();
    assert_eq!(names, expected.keys().map(String::as_str).collect());
    assert_eq!(contents.manifest.version, DUMP_VERSION);
    assert!(contents.manifest.missing_tables().is_empty());
    for table in &contents.manifest.tables {
        let expected = &expected[&table.name];
        assert!(
            !expected.rows.is_empty(),
            "seed fixture omitted {}",
            table.name
        );
        assert_eq!(
            table.count,
            expected.rows.len() as u64,
            "{} count",
            table.name
        );
        assert_eq!(
            table.columns,
            expected
                .columns
                .iter()
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            table.column_types,
            expected
                .columns
                .iter()
                .map(|(_, ty)| ty.clone())
                .collect::<Vec<_>>()
        );
        let rows: Vec<Vec<Option<String>>> = contents.buffers[&table.name]
            .iter()
            .map(|row| {
                assert_eq!(
                    row.len(),
                    table.columns.len(),
                    "{} complete row",
                    table.name
                );
                table
                    .columns
                    .iter()
                    .map(|column| match &row[column] {
                        Value::Null => None,
                        Value::String(value) => Some(value.clone()),
                        other => panic!("{}.{column}: unexpected encoded cell {other}", table.name),
                    })
                    .collect()
            })
            .collect();
        assert_eq!(rows, expected.rows, "{} ordered complete rows", table.name);
    }
    assert_eq!(
        contents.rows,
        expected
            .values()
            .map(|table| table.rows.len() as u64)
            .sum::<u64>()
    );
}

fn copy_contents(contents: &DumpContents) -> DumpContents {
    DumpContents {
        manifest: contents.manifest.clone(),
        buffers: contents.buffers.clone(),
        rows: contents.rows,
    }
}

fn write_archive(path: &Path, contents: &DumpContents) {
    let mut encoder = new_encoder();
    write_line(
        &mut encoder,
        &serde_json::to_value(&contents.manifest).unwrap(),
    )
    .unwrap();
    // Intentionally not FK order. Children such as lfg_signups precede parents;
    // rows within a table are reversed too. Restore must use its own ordering.
    for (table, rows) in contents.buffers.iter().rev() {
        for row in rows.iter().rev() {
            write_line(
                &mut encoder,
                &json!({"kind":"row", "table":table, "data":row}),
            )
            .unwrap();
        }
    }
    write_line(&mut encoder, &json!({"kind":"end", "rows":contents.rows})).unwrap();
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .unwrap();
    std::io::Write::write_all(&mut file, &finish_gzip(encoder).unwrap()).unwrap();
    inspect(path).expect("test archive must pass file validation before reaching restore");
}

async fn triggers(pool: &PgPool) -> Vec<(String, String, String, String)> {
    sqlx::query_as(
        "SELECT c.relname::text, t.tgname::text, t.tgenabled::text, pg_get_triggerdef(t.oid) \
         FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = current_schema() AND NOT t.tgisinternal \
         ORDER BY c.relname::text, t.tgname::text",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// Migrated settings triggers. Restore suspends only the two guards; the CAS
/// allocator stays enabled so restored rows receive fresh tokens.
fn assert_settings_triggers(triggers: &[(String, String, String, String)]) {
    assert_eq!(
        triggers
            .iter()
            .map(|(table, name, _, _)| (table.as_str(), name.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("guild_settings", "trg_guild_settings_revision"),
            ("guild_settings", "trg_guild_settings_version"),
            (
                "guild_settings_audit",
                "trg_guild_settings_audit_append_only"
            ),
        ],
        "every migrated settings trigger exists"
    );
}

/// Next token guild_settings_cas_seq would issue. It descends and is never
/// reseeded, so every token issued so far is strictly greater.
async fn next_cas_token(pool: &PgPool) -> i64 {
    let (last, called): (i64, bool) =
        sqlx::query_as("SELECT last_value, is_called FROM guild_settings_cas_seq")
            .fetch_one(pool)
            .await
            .unwrap();
    if called {
        last - 1
    } else {
        last
    }
}

async fn cas_tokens(pool: &PgPool) -> Vec<(String, String, i64)> {
    sqlx::query_as(
        "SELECT guild_id, key, cas_version FROM guild_settings \
         ORDER BY guild_id COLLATE \"C\", key COLLATE \"C\"",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// A committed restore is a genuine write: every restored row carries a token
/// allocated after `next_before`, so no token issued before it still matches.
async fn assert_fresh_cas_tokens(pool: &PgPool, next_before: i64) {
    let tokens = cas_tokens(pool).await;
    assert!(!tokens.is_empty(), "seeded settings exercise CAS");
    let distinct: BTreeSet<i64> = tokens.iter().map(|(_, _, token)| *token).collect();
    assert_eq!(
        distinct.len(),
        tokens.len(),
        "restored CAS tokens are distinct"
    );
    assert!(
        distinct.iter().all(|token| *token <= next_before),
        "restore must invalidate every earlier CAS token: {tokens:?}, next before {next_before}"
    );
    assert!(next_cas_token(pool).await < next_before);
}

async fn set_guard_modes(pool: &PgPool, revision: &str, audit: &str) {
    for (table, trigger, mode) in [
        ("guild_settings", "trg_guild_settings_revision", revision),
        (
            "guild_settings_audit",
            "trg_guild_settings_audit_append_only",
            audit,
        ),
    ] {
        let action = match mode {
            "O" => "ENABLE",
            "A" => "ENABLE ALWAYS",
            "R" => "ENABLE REPLICA",
            "D" => "DISABLE",
            _ => panic!("invalid test trigger mode"),
        };
        sqlx::query(audited(format!(
            "ALTER TABLE {table} {action} TRIGGER {trigger}"
        )))
        .execute(pool)
        .await
        .unwrap();
    }
}

fn assert_sqlstate(error: &sqlx::Error, code: &str) {
    assert_eq!(
        error
            .as_database_error()
            .expect("database constraint/trigger error")
            .code()
            .as_deref(),
        Some(code),
        "{error}"
    );
}

async fn assert_guards_work(pool: &PgPool) {
    let mut tx = pool.begin().await.unwrap();
    let before: i64 =
        sqlx::query_scalar("SELECT revision FROM guild_settings_revision WHERE singleton = TRUE")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    let updated = sqlx::query("UPDATE guild_settings SET updated_by = 'guard-probe' WHERE (guild_id, key) = (SELECT guild_id, key FROM guild_settings ORDER BY guild_id, key LIMIT 1)").execute(&mut *tx).await.unwrap();
    assert_eq!(updated.rows_affected(), 1);
    let after: i64 =
        sqlx::query_scalar("SELECT revision FROM guild_settings_revision WHERE singleton = TRUE")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert_eq!(after, before + 1, "revision trigger remains functional");
    tx.rollback().await.unwrap();
    for statement in [
        "UPDATE guild_settings_audit SET actor = 'forbidden'",
        "DELETE FROM guild_settings_audit",
        "TRUNCATE guild_settings_audit",
    ] {
        let error = sqlx::query(audited(statement.to_owned()))
            .execute(pool)
            .await
            .expect_err("append-only guard must still fire");
        assert_sqlstate(&error, "P0001");
        assert!(error.to_string().contains("append-only"), "{error}");
    }
}

#[derive(Debug)]
struct OwnedSequence {
    table: String,
    column: String,
    name: String,
    start: i64,
    increment: i64,
}

async fn owned_sequences(pool: &PgPool) -> Vec<OwnedSequence> {
    // pg_get_serial_sequence explicitly supports BOTH serial and identity.
    // https://www.postgresql.org/docs/16/functions-info.html
    let rows: Vec<(String, String, String, i64, i64)> = sqlx::query_as(
        "SELECT c.table_name::text, c.column_name::text, \
         pg_get_serial_sequence(format('%I.%I', c.table_schema, c.table_name), c.column_name), \
         s.seqstart, s.seqincrement FROM information_schema.columns c \
         JOIN pg_sequence s ON s.seqrelid = \
           pg_get_serial_sequence(format('%I.%I', c.table_schema, c.table_name), c.column_name)::regclass \
         WHERE c.table_schema = current_schema() ORDER BY c.table_name, c.ordinal_position",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    rows.into_iter()
        .filter(|(table, _, _, _, _)| DUMP_TABLES.contains(&table.as_str()))
        .map(|(table, column, name, start, increment)| OwnedSequence {
            table,
            column,
            name,
            start,
            increment,
        })
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
struct SequenceSnapshot {
    name: String,
    definition: (i64, i64, i64, i64, i64, bool),
    value: (i64, bool),
}

/// Transactional sequence state. guild_settings_cas_seq is left out: nextval is
/// not transactional, so a rolled-back settings write can consume CAS tokens.
/// Tests assert that sequence only descends instead ([`next_cas_token`]).
async fn sequence_snapshot(pool: &PgPool) -> Vec<SequenceSnapshot> {
    let rows: Vec<(String, i64, i64, i64, i64, i64, bool)> = sqlx::query_as(
        "SELECT format('%I.%I', n.nspname, c.relname), s.seqstart, s.seqincrement, \
         s.seqmin, s.seqmax, s.seqcache, s.seqcycle FROM pg_sequence s \
         JOIN pg_class c ON c.oid = s.seqrelid JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = current_schema() AND c.relname <> 'guild_settings_cas_seq' \
         ORDER BY c.relname::text",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let mut snapshots = Vec::new();
    for (name, start, increment, min, max, cache, cycle) in rows {
        // `name` is fully qualified and already identifier-quoted by format('%I').
        let value = sqlx::query_as(audited(format!("SELECT last_value, is_called FROM {name}")))
            .fetch_one(pool)
            .await
            .unwrap();
        snapshots.push(SequenceSnapshot {
            name,
            definition: (start, increment, min, max, cache, cycle),
            value,
        });
    }
    snapshots
}

/// The value `sequence` would hand out next, without consuming it.
async fn sequence_next(pool: &PgPool, sequence: &str) -> i64 {
    let increment: i64 =
        sqlx::query_scalar("SELECT seqincrement FROM pg_sequence WHERE seqrelid = $1::regclass")
            .bind(sequence)
            .fetch_one(pool)
            .await
            .unwrap();
    // `sequence` comes from pg_get_serial_sequence (already quoted) or a literal.
    let (last, called): (i64, bool) = sqlx::query_as(audited(format!(
        "SELECT last_value, is_called FROM {sequence}"
    )))
    .fetch_one(pool)
    .await
    .unwrap();
    if called {
        last + increment
    } else {
        last
    }
}

/// Per owned allocator, the furthest of the target's own next value and the
/// archive's mark. Restore must not hand out anything before it.
async fn allocation_floors(
    pool: &PgPool,
    marks: &[SequenceMark],
) -> BTreeMap<(String, String), i64> {
    let mut floors = BTreeMap::new();
    for sequence in owned_sequences(pool).await {
        assert!(sequence.increment > 0, "owned allocators ascend");
        let own = sequence_next(pool, &sequence.name).await;
        let marked = marks
            .iter()
            .find(|mark| mark.table == sequence.table && mark.column == sequence.column)
            .map_or(own, |mark| mark.next().unwrap());
        floors.insert((sequence.table, sequence.column), own.max(marked));
    }
    floors
}

async fn allocate_owned_sequences(
    pool: &PgPool,
    empty: bool,
    floors: &BTreeMap<(String, String), i64>,
) -> BTreeMap<(String, String), (i64, i64)> {
    let mut allocated = BTreeMap::new();
    for sequence in owned_sequences(pool).await {
        let maximum: Option<i64> = sqlx::query_scalar(audited(format!(
            "SELECT MAX({})::bigint FROM {}",
            identifier(&sequence.column),
            identifier(&sequence.table)
        )))
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(
            maximum.is_none(),
            empty,
            "{}.{} emptiness",
            sequence.table,
            sequence.column
        );
        let edge = maximum.map_or(sequence.start, |maximum| maximum + sequence.increment);
        let floor = floors[&(sequence.table.clone(), sequence.column.clone())];
        let expected = edge.max(floor);
        let next: i64 = sqlx::query_scalar("SELECT nextval($1::regclass)")
            .bind(&sequence.name)
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(
            next, expected,
            "{}.{} allocation after restore",
            sequence.table, sequence.column
        );
        allocated.insert(
            (sequence.table, sequence.column),
            (next, sequence.increment),
        );
    }
    assert!(allocated.contains_key(&("internal_idempotency".into(), "intent_id".into())));
    assert!(allocated.contains_key(&("internal_action_log".into(), "audit_id".into())));
    assert_eq!(
        allocated[&("members".into(), "backup_identity".into())].1,
        3,
        "test identity uses a non-default increment"
    );
    allocated
}

async fn assert_default_inserts(
    pool: &PgPool,
    allocations: &BTreeMap<(String, String), (i64, i64)>,
) {
    // Every currently migrated owned sequence gets a real DEFAULT allocation
    // through INSERT as well as nextval(). No explicit id is bound here.
    // GENERATED ALWAYS must remain ALWAYS, while restore uses OVERRIDING SYSTEM VALUE.
    // https://www.postgresql.org/docs/16/sql-insert.html
    let inserts = [
        ("events", "id", "INSERT INTO events (event_type, guild_id, occurred_at, source, idempotency_key) VALUES ('member_join', '100000000000000001', now(), 'backup-test', 'post-restore:event') RETURNING id"),
        ("members", "backup_identity", "INSERT INTO members (guild_id, member_id) VALUES ('100000000000000001', 'post-restore-member') RETURNING backup_identity"),
        ("xp_awards", "id", "INSERT INTO xp_awards (guild_id, member_id, source, xp, occurred_at) VALUES ('100000000000000001', '100000000000000002', 'message', 5, now()) RETURNING id"),
        ("level_import_runs", "id", "INSERT INTO level_import_runs (guild_id, source, source_rows, unique_members, inserted, updated, unchanged, duplicate_rows, total_imported_xp, imported_at) VALUES ('100000000000000001', 'mee6', 1, 1, 1, 0, 0, 0, 100, now()) RETURNING id"),
        ("community_facts", "id", "INSERT INTO community_facts (guild_id, event_type, source_event_id, occurred_at, source, classifier_version, classification, matched_rule, idempotency_key) VALUES ('100000000000000001', 'rules_accepted', 'post-restore:fact', '2026-08-04T00:00:00.000Z', 'backup-test', 'classifier-v1', 'test', 'default', 'post-restore:fact') RETURNING id"),
        ("community_scorecard_runs", "id", "INSERT INTO community_scorecard_runs (guild_id, week_start, week_end, classifier_version, watermark, input_count, input_hash, idempotency_key, revision, run_status, coverage_state, evidence_state, scorecard_json, intervention_code, generated_at) VALUES ('100000000000000001', '2026-07-27T00:00:00.000Z', '2026-08-03T00:00:00.000Z', 'classifier-v1', 61, 7, 'post-restore-hash', 'post-restore:scorecard', 4, 'completed', 'complete', 'sufficient', '{}', 'maintain', '2026-08-04T00:00:00.000Z') RETURNING id"),
        ("guild_settings_audit", "id", "INSERT INTO guild_settings_audit (guild_id, key, new_value, actor) VALUES ('100000000000000001', 'TWO_FEEDS_ENABLED', 'true', 'post-restore') RETURNING id"),
        ("internal_idempotency", "intent_id", "INSERT INTO internal_idempotency (caller_hash, key_hash, action, payload_hash, state) VALUES (repeat('4', 64), repeat('5', 64), 'event.read', repeat('6', 64), 'in_flight') RETURNING intent_id"),
        ("internal_action_log", "audit_id", "INSERT INTO internal_action_log (intent_id, phase, caller_hash, action, resolved_role_id) VALUES (83, 'intent', repeat('b', 64), 'role.assign', '100000000000000010') RETURNING audit_id"),
    ];
    assert_eq!(
        inserts
            .iter()
            .map(|(table, column, _)| (table.to_string(), column.to_string()))
            .collect::<BTreeSet<_>>(),
        allocations.keys().cloned().collect(),
        "new owned sequences need a valid DEFAULT INSERT case"
    );
    for (table, column, statement) in inserts {
        let (allocated, increment) = allocations[&(table.to_owned(), column.to_owned())];
        let inserted: i64 = sqlx::query_scalar(audited(statement.to_owned()))
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(
            inserted,
            allocated + increment,
            "{table}.{column} default INSERT must not collide"
        );
    }
    let identity: String = sqlx::query_scalar("SELECT attidentity::text FROM pg_attribute WHERE attrelid = 'members'::regclass AND attname = 'backup_identity'").fetch_one(pool).await.unwrap();
    assert_eq!(identity, "a", "restore must not weaken GENERATED ALWAYS");
    let error = sqlx::query("INSERT INTO members (guild_id, member_id, backup_identity) VALUES ('100000000000000001', 'forbidden-identity', 10000)").execute(pool).await.unwrap_err();
    assert_sqlstate(&error, "428C9");
}

async fn dirty_target(pool: &PgPool) {
    sqlx::raw_sql(
        "UPDATE members SET join_source = 'target-only'; \
         INSERT INTO members (guild_id, member_id) VALUES ('100000000000000001', 'target-only-member'); \
         INSERT INTO xp_cooldowns (guild_id, member_id, source, last_awarded_at) \
             VALUES ('100000000000000001', 'target-only-member', 'voice', now()); \
         UPDATE guild_settings SET updated_by = 'target-only'; \
         UPDATE guild_settings_revision SET revision = 9999; \
         INSERT INTO guild_settings_audit (id, guild_id, key, actor) VALUES (5000, 'target-only', 'target-only', 'target-only'); \
         INSERT INTO internal_discord_events (event_hash) VALUES (repeat('9', 64));",
    ).execute(pool).await.unwrap();
}

#[tokio::test]
async fn migrated_tables_are_explicitly_classified_and_catalog_fks_are_parent_first() {
    let Some(db) = database().await else {
        return;
    };
    let actual: BTreeSet<String> = actual_tables(db.pool()).await.into_iter().collect();
    let covered: BTreeSet<&str> = DUMP_TABLES.iter().copied().collect();
    let excluded: BTreeSet<&str> = EXCLUDED_TABLES.iter().copied().collect();
    let optional: BTreeSet<&str> = OPTIONAL_LEGACY_TABLES.iter().copied().collect();
    assert_eq!(
        covered.len(),
        DUMP_TABLES.len(),
        "no duplicate covered names"
    );
    assert_eq!(
        excluded.len(),
        EXCLUDED_TABLES.len(),
        "no duplicate exclusions"
    );
    assert_eq!(
        optional.len(),
        OPTIONAL_LEGACY_TABLES.len(),
        "no duplicate legacy names"
    );
    assert!(
        covered.is_disjoint(&excluded),
        "classification must be unambiguous"
    );
    assert!(
        optional.is_subset(&covered),
        "only covered tables may be optional"
    );
    for table in &actual {
        assert!(
            covered.contains(table.as_str()) || excluded.contains(table.as_str()),
            "migrated table {table} is neither DUMP_TABLES nor EXCLUDED_TABLES"
        );
    }
    for table in &optional {
        assert!(
            !actual.contains(*table),
            "migrated legacy table {table} must become nonoptional"
        );
    }
    for table in covered.difference(&optional) {
        assert!(
            actual.contains(*table),
            "nonoptional covered table {table} is not migrated"
        );
    }
    for &(table, column) in DESTINATION_OWNED_COLUMNS {
        assert!(covered.contains(table), "{table}.{column} must be covered");
        let migrated: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = $1 AND column_name = $2)",
        )
        .bind(table)
        .bind(column)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert!(
            migrated,
            "destination-owned {table}.{column} is not migrated"
        );
    }
    // Restore resets every owned serial/identity sequence. Any other sequence
    // must be a deliberate choice: version_seq is restarted explicitly and
    // cas_seq feeds the destination-owned guild_settings.cas_version.
    let standalone: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind = 'S' AND n.nspname = current_schema() \
           AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_class'::regclass \
             AND d.objid = c.oid AND d.deptype IN ('a', 'i')) \
         ORDER BY c.relname::text COLLATE \"C\"",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(
        standalone,
        vec!["guild_settings_cas_seq", "guild_settings_version_seq"],
        "a new standalone sequence needs an explicit restore decision"
    );
    let fks: Vec<(String, String, String, bool)> = sqlx::query_as(
        "SELECT constraint_row.conname::text, child.relname::text, parent.relname::text, constraint_row.convalidated \
         FROM pg_constraint constraint_row JOIN pg_class child ON child.oid = constraint_row.conrelid \
         JOIN pg_class parent ON parent.oid = constraint_row.confrelid \
         JOIN pg_namespace n ON n.oid = child.relnamespace \
         WHERE constraint_row.contype = 'f' AND n.nspname = current_schema() \
         ORDER BY child.relname::text, constraint_row.conname::text",
    ).fetch_all(db.pool()).await.unwrap();
    assert!(
        !fks.is_empty(),
        "real migration FK chains must be exercised"
    );
    for (constraint, child, parent, validated) in fks {
        assert!(validated, "{constraint} must remain validated");
        if covered.contains(child.as_str()) {
            let parent_index = DUMP_TABLES
                .iter()
                .position(|table| *table == parent)
                .expect("covered child must not depend on excluded parent");
            let child_index = DUMP_TABLES
                .iter()
                .position(|table| *table == child)
                .unwrap();
            assert!(
                parent_index < child_index,
                "FK {constraint}: {parent} must precede {child}"
            );
        }
    }
    db.close().await.unwrap();
}

#[tokio::test]
async fn every_migrated_row_roundtrips_with_reversed_manifest_and_all_sequence_defaults() {
    let Some(db) = database().await else {
        return;
    };
    let pool = db.pool();
    seed(pool).await;
    let tables = covered_tables(pool).await;
    let before = snapshot(pool, &tables).await;
    let ledgers = vec![
        "_sqlx_migrations".to_owned(),
        "schema_migrations".to_owned(),
    ];
    let guard_before = triggers(pool).await;
    assert_settings_triggers(&guard_before);
    assert_guards_work(pool).await;
    let directory = ArchiveDirectory::new();
    let original = directory.path("complete.ndjson.gz");
    let manifest = dump(pool, &original).await.unwrap();
    assert_eq!(manifest.events_sequence, 107);
    assert_eq!(
        manifest.schema_migrations,
        vec!["legacy-ledger-diagnostic", "target-ledger"]
    );
    let contents = inspect(&original).unwrap();
    assert_archive_matches(&contents, &before);
    assert!(contents
        .manifest
        .tables
        .iter()
        .all(|table| !EXCLUDED_TABLES.contains(&table.name.as_str())));

    // Independent, freshly migrated destination, not a wipe of the source DB.
    let target = database().await.expect("test bootstrap already configured");
    let pool = target.pool();
    sqlx::raw_sql(
        "CREATE TABLE schema_migrations (id TEXT PRIMARY KEY); \
         INSERT INTO schema_migrations VALUES ('destination-ledger-only'); \
         ALTER TABLE members ADD COLUMN backup_identity BIGINT GENERATED ALWAYS AS IDENTITY \
             (START WITH 17 INCREMENT BY 3) UNIQUE;",
    )
    .execute(pool)
    .await
    .unwrap();
    let ledger_before = snapshot(pool, &ledgers).await;
    dirty_target(pool).await;
    let cas_before = next_cas_token(pool).await;
    let report = restore(pool, &original).await.unwrap();
    assert!(report.ok);
    assert!(report.dropped_columns.is_empty());
    assert!(
        report.initialized_tables.is_empty(),
        "v4 restores archived singleton exactly"
    );
    assert_eq!(snapshot(pool, &tables).await, before);
    assert_fresh_cas_tokens(pool, cas_before).await;
    assert_eq!(triggers(pool).await, guard_before);
    assert_eq!(snapshot(pool, &ledgers).await, ledger_before);
    let cooldowns: i64 = sqlx::query_scalar("SELECT count(*) FROM xp_cooldowns")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(cooldowns, 0, "excluded throttles are not replayed");
    assert_guards_work(pool).await;

    let mut reversed = copy_contents(&contents);
    reversed.manifest.tables.reverse();
    let reversed_path = directory.path("reversed.ndjson.gz");
    write_archive(&reversed_path, &reversed);
    let mut floors = BTreeMap::new();
    for (revision_mode, audit_mode) in [("O", "O"), ("A", "R"), ("R", "D"), ("D", "A")] {
        set_guard_modes(pool, revision_mode, audit_mode).await;
        let modes_before = triggers(pool).await;
        dirty_target(pool).await;
        // Keep the floors in force before the last restore.
        floors.extend(allocation_floors(pool, &reversed.manifest.sequence_marks).await);
        let cas_before = next_cas_token(pool).await;
        let report = restore(pool, &reversed_path)
            .await
            .expect("restore must ignore manifest and row block order");
        assert!(report.ok);
        assert!(report.dropped_columns.is_empty());
        assert!(report.initialized_tables.is_empty());
        assert_eq!(
            snapshot(pool, &tables).await,
            before,
            "reversed complete rows: {revision_mode}/{audit_mode}"
        );
        assert_fresh_cas_tokens(pool, cas_before).await;
        assert_eq!(
            triggers(pool).await,
            modes_before,
            "restore original trigger modes and definitions"
        );
    }
    set_guard_modes(pool, "O", "O").await;
    assert_guards_work(pool).await;
    assert_eq!(snapshot(pool, &ledgers).await, ledger_before);
    let allocations = allocate_owned_sequences(pool, false, &floors).await;
    // dirty_target allocated identities on the target past the restored 313;
    // their rows are gone, but restore must not rewind the allocator onto them.
    assert!(
        allocations[&("members".into(), "backup_identity".into())].0 > 316,
        "the target's own high-water survives restore"
    );
    assert_default_inserts(pool, &allocations).await;
    let allocated_version: i64 = sqlx::query_scalar("SELECT nextval('guild_settings_version_seq')")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(
        allocated_version, 84,
        "standalone settings version resumes beyond restored max"
    );
    let inserted_version: i64 = sqlx::query_scalar("INSERT INTO guild_settings (guild_id, key, value, version, updated_by) VALUES ('100000000000000001', 'TWO_BACKUP_TEST', 'true', nextval('guild_settings_version_seq'), 'post-restore') RETURNING version").fetch_one(pool).await.unwrap();
    assert_eq!(inserted_version, 85);
    // Durable dedupe remains enforceable, not merely present in the dump.
    for statement in [
        "INSERT INTO internal_nonces SELECT * FROM internal_nonces WHERE nonce_hash = repeat('a', 64)",
        "INSERT INTO internal_discord_events SELECT * FROM internal_discord_events WHERE event_hash = repeat('3', 64)",
        "INSERT INTO internal_idempotency (caller_hash, key_hash, action, payload_hash, state) VALUES (repeat('b', 64), repeat('c', 64), 'role.assign', repeat('d', 64), 'in_flight')",
        "INSERT INTO feed_deliveries SELECT * FROM feed_deliveries WHERE item_key = 'backup:item:pending'",
        "INSERT INTO self_role_panel_claims SELECT * FROM self_role_panel_claims",
        "INSERT INTO automod_delivery_claims SELECT * FROM automod_delivery_claims",
    ] {
        assert_sqlstate(&sqlx::query(audited(statement.to_owned())).execute(pool).await.unwrap_err(), "23505");
    }
    target.close().await.unwrap();
    db.close().await.unwrap();
}

#[tokio::test]
async fn empty_archived_sequence_tables_keep_the_target_position() {
    let Some(db) = database().await else {
        return;
    };
    let pool = db.pool();
    seed(pool).await;
    let directory = ArchiveDirectory::new();
    let path = directory.path("seeded.ndjson.gz");
    dump(pool, &path).await.unwrap();
    let mut empty = inspect(&path).unwrap();
    let sequences = owned_sequences(pool).await;
    let sequence_tables: BTreeSet<_> = sequences
        .iter()
        .map(|sequence| sequence.table.as_str())
        .chain(std::iter::once("guild_settings"))
        .collect();
    for table in &mut empty.manifest.tables {
        if sequence_tables.contains(table.name.as_str()) {
            table.count = 0;
            empty.buffers.remove(&table.name);
        }
    }
    empty.rows = empty.manifest.tables.iter().map(|table| table.count).sum();
    let empty_path = directory.path("empty-sequences.ndjson.gz");
    write_archive(&empty_path, &empty);
    let floors = allocation_floors(pool, &empty.manifest.sequence_marks).await;
    let report = restore(pool, &empty_path).await.unwrap();
    assert!(report.ok);
    assert!(report.initialized_tables.is_empty());
    for table in sequence_tables {
        assert_eq!(report.restored[table], 0, "{table} must really be empty");
    }
    let allocations = allocate_owned_sequences(pool, true, &floors).await;
    // The seed binds explicit member rows, so the test identity hands out 17
    // and 20 for the two fixture rows before the dump; the target's own next
    // value is 23, and restore keeps that position rather than rewinding to
    // the configured start. `floors` already pins this exact expectation.
    assert_eq!(
        allocations[&("members".into(), "backup_identity".into())].0,
        floors[&("members".into(), "backup_identity".into())],
    );
    assert_eq!(
        allocations[&("members".into(), "backup_identity".into())].0,
        23
    );
    let version: i64 = sqlx::query_scalar("SELECT nextval('guild_settings_version_seq')")
        .fetch_one(pool)
        .await
        .unwrap();
    // Migration 0331's setval leaves an empty table's sequence at (1, called):
    // 1 counts as issued, and restore never reissues a value.
    assert_eq!(
        version, 2,
        "empty standalone sequence resumes past its own issued start"
    );
    let intent: i64 = sqlx::query_scalar("INSERT INTO internal_idempotency (caller_hash, key_hash, action, payload_hash, state) VALUES (repeat('4', 64), repeat('5', 64), 'event.read', repeat('6', 64), 'in_flight') RETURNING intent_id").fetch_one(pool).await.unwrap();
    assert_eq!(intent, 2);
    let audit: i64 = sqlx::query_scalar("INSERT INTO internal_action_log (intent_id, phase, caller_hash, action) VALUES ($1, 'intent', repeat('4', 64), 'event.read') RETURNING audit_id").bind(intent).fetch_one(pool).await.unwrap();
    assert_eq!(audit, 2);
    let identity: i64 = sqlx::query_scalar("INSERT INTO members (guild_id, member_id) VALUES ('100000000000000001', 'empty-default-member') RETURNING backup_identity").fetch_one(pool).await.unwrap();
    assert_eq!(identity, 26);
    db.close().await.unwrap();
}

const EVENT_INSERT: &str = "INSERT INTO events (event_type, guild_id, occurred_at, source, idempotency_key) VALUES ('member_join', '100000000000000001', now(), 'backup-test', $1) RETURNING id";
const MEMBER_INSERT: &str =
    "INSERT INTO members (guild_id, member_id) VALUES ('100000000000000001', $1) RETURNING backup_identity";
const FACT_INSERT: &str = "INSERT INTO community_facts (guild_id, event_type, source_event_id, occurred_at, source, classifier_version, classification, matched_rule, idempotency_key) VALUES ('100000000000000001', 'rules_accepted', $1, '2026-08-04T00:00:00.000Z', 'backup-test', 'classifier-v1', 'test', 'default', $1) RETURNING id";
const SETTING_INSERT: &str = "INSERT INTO guild_settings (guild_id, key, value, version, updated_by) VALUES ('100000000000000001', 'TWO_BACKUP_TEST', 'true', nextval('guild_settings_version_seq'), $1) RETURNING version";
const BACKUP_IDENTITY: &str = "ALTER TABLE members ADD COLUMN backup_identity BIGINT GENERATED ALWAYS AS IDENTITY (START WITH 17 INCREMENT BY 3) UNIQUE";

async fn insert_returning(pool: &PgPool, statement: &'static str, key: &str) -> i64 {
    sqlx::query_scalar(statement)
        .bind(key)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// TOG-12232, the cutover allocator gate: rows alone cannot show values handed
/// out and then deleted, so the archive carries each allocator's mark. Restore
/// resumes past it, keeps a target that is further ahead, and gives restored
/// settings CAS tokens beyond every token the source issued.
#[tokio::test]
async fn restore_never_reissues_values_handed_out_before_their_rows_were_deleted() {
    let Some(db) = database().await else {
        return;
    };
    let source = db.pool();
    seed(source).await;
    // The fixture binds explicit ids; a live source's allocators sit past its rows.
    for sequence in owned_sequences(source).await {
        sqlx::query(audited(format!(
            "SELECT setval($1::regclass, MAX({})::bigint) FROM {}",
            identifier(&sequence.column),
            identifier(&sequence.table)
        )))
        .bind(&sequence.name)
        .execute(source)
        .await
        .unwrap();
    }
    sqlx::query("SELECT setval('guild_settings_version_seq', MAX(version)) FROM guild_settings")
        .execute(source)
        .await
        .unwrap();
    // Insert, then delete, the top rows: the archive holds none of these values.
    let deleted_event = insert_returning(source, EVENT_INSERT, "deleted-top:event").await;
    let deleted_identity = insert_returning(source, MEMBER_INSERT, "deleted-top-member").await;
    let deleted_fact = insert_returning(source, FACT_INSERT, "deleted-top:fact").await;
    let deleted_version = insert_returning(source, SETTING_INSERT, "deleted-top").await;
    sqlx::raw_sql(
        "DELETE FROM events WHERE idempotency_key = 'deleted-top:event'; \
         DELETE FROM members WHERE member_id = 'deleted-top-member'; \
         DELETE FROM community_facts WHERE idempotency_key = 'deleted-top:fact'; \
         DELETE FROM guild_settings WHERE key = 'TWO_BACKUP_TEST';",
    )
    .execute(source)
    .await
    .unwrap();
    // Every other allocator hands out a value too, as a rolled-back insert does.
    for sequence in owned_sequences(source).await {
        sqlx::query("SELECT nextval($1::regclass)")
            .bind(&sequence.name)
            .execute(source)
            .await
            .unwrap();
    }

    let directory = ArchiveDirectory::new();
    let path = directory.path("deleted-top-rows.ndjson.gz");
    let manifest = dump(source, &path).await.unwrap();
    let mut expected = BTreeMap::new();
    for sequence in owned_sequences(source).await {
        let next = sequence_next(source, &sequence.name).await;
        expected.insert((sequence.table, sequence.column), next);
    }
    let version_key = ("guild_settings".to_owned(), "version".to_owned());
    let source_version_next = sequence_next(source, "guild_settings_version_seq").await;
    expected.insert(version_key.clone(), source_version_next);
    let source_cas_next = next_cas_token(source).await;
    expected.insert(
        ("guild_settings".to_owned(), "cas_version".to_owned()),
        source_cas_next,
    );
    assert_eq!(
        manifest
            .sequence_marks
            .iter()
            .map(|mark| (
                (mark.table.clone(), mark.column.clone()),
                mark.next().unwrap()
            ))
            .collect::<BTreeMap<_, _>>(),
        expected,
        "the archive marks every allocator, owned or standalone"
    );
    assert_eq!(
        inspect(&path).unwrap().manifest.sequence_marks,
        manifest.sequence_marks
    );

    let target = database().await.expect("test bootstrap already configured");
    let pool = target.pool();
    sqlx::query(BACKUP_IDENTITY).execute(pool).await.unwrap();
    // A target allocator already further ahead than the archive stays there.
    sqlx::query("SELECT setval(pg_get_serial_sequence('xp_awards', 'id'), 9000)")
        .execute(pool)
        .await
        .unwrap();
    let report = restore(pool, &path).await.unwrap();
    assert!(report.ok);
    assert!(report.dropped_columns.is_empty());
    for sequence in owned_sequences(pool).await {
        let key = (sequence.table, sequence.column);
        let resumes = if key == ("xp_awards".to_owned(), "id".to_owned()) {
            9001
        } else {
            expected[&key]
        };
        assert_eq!(
            sequence_next(pool, &sequence.name).await,
            resumes,
            "{key:?} resumes at the furthest of mark, rows and target"
        );
    }
    assert_eq!(
        sequence_next(pool, "guild_settings_version_seq").await,
        source_version_next
    );
    let restored_tokens = cas_tokens(pool).await;
    assert!(!restored_tokens.is_empty(), "seeded settings exercise CAS");
    assert!(
        restored_tokens
            .iter()
            .all(|(_, _, token)| *token <= source_cas_next),
        "a stale source CAS token must never match a restored row: {restored_tokens:?}, source next {source_cas_next}"
    );

    let event = insert_returning(pool, EVENT_INSERT, "post-restore:event").await;
    assert!(event > deleted_event, "event {deleted_event} reissued");
    let identity = insert_returning(pool, MEMBER_INSERT, "post-restore-member").await;
    assert!(
        identity > deleted_identity,
        "identity {deleted_identity} reissued"
    );
    // Scorecard runs publish MAX(community_facts.id) as their watermark.
    let fact = insert_returning(pool, FACT_INSERT, "post-restore:fact").await;
    assert!(fact > deleted_fact, "fact {deleted_fact} reissued");
    let version = insert_returning(pool, SETTING_INSERT, "post-restore").await;
    assert!(
        version > deleted_version,
        "version {deleted_version} reissued"
    );

    // An archive written before marks existed cannot meet the gate: it holds
    // only the rows, so the deleted top id comes back (docs/cutover.md).
    let mut unmarked = inspect(&path).unwrap();
    unmarked.manifest.sequence_marks.clear();
    let unmarked_path = directory.path("unmarked.ndjson.gz");
    write_archive(&unmarked_path, &unmarked);
    let fresh = database().await.expect("test bootstrap already configured");
    sqlx::query(BACKUP_IDENTITY)
        .execute(fresh.pool())
        .await
        .unwrap();
    assert!(restore(fresh.pool(), &unmarked_path).await.unwrap().ok);
    assert_eq!(
        insert_returning(fresh.pool(), EVENT_INSERT, "post-restore:event").await,
        deleted_event
    );
    fresh.close().await.unwrap();
    target.close().await.unwrap();
    db.close().await.unwrap();
}

#[tokio::test]
async fn restore_sql_failures_and_late_sequence_exhaustion_roll_back_data_guards_and_sequences() {
    let Some(db) = database().await else {
        return;
    };
    let pool = db.pool();
    seed(pool).await;
    let directory = ArchiveDirectory::new();
    let source = directory.path("source.ndjson.gz");
    dump(pool, &source).await.unwrap();
    let contents = inspect(&source).unwrap();
    let mut bad_check = copy_contents(&contents);
    bad_check
        .buffers
        .get_mut("internal_discord_events")
        .unwrap()[0]
        .insert("event_hash".into(), json!("not-a-valid-digest"));
    let bad_check_path = directory.path("valid-envelope-bad-check.ndjson.gz");
    write_archive(&bad_check_path, &bad_check);
    let mut bad_fk = copy_contents(&contents);
    bad_fk.buffers.get_mut("internal_action_log").unwrap()[0]
        .insert("intent_id".into(), json!("123456789"));
    let bad_fk_path = directory.path("valid-envelope-bad-fk.ndjson.gz");
    write_archive(&bad_fk_path, &bad_fk);
    dirty_target(pool).await;
    // Dirty sequence state too: rollback must not leave a reset high-water mark.
    for sequence in owned_sequences(pool).await {
        sqlx::query("SELECT nextval($1::regclass)")
            .bind(sequence.name)
            .execute(pool)
            .await
            .unwrap();
    }
    sqlx::query("SELECT setval('guild_settings_version_seq', 777, TRUE)")
        .execute(pool)
        .await
        .unwrap();
    let tables = actual_tables(pool).await;
    for (revision_mode, audit_mode) in [("O", "O"), ("A", "R"), ("R", "D"), ("D", "A")] {
        set_guard_modes(pool, revision_mode, audit_mode).await;
        for (archive, code) in [(&bad_check_path, "23514"), (&bad_fk_path, "23503")] {
            let before = snapshot(pool, &tables).await;
            let guards = triggers(pool).await;
            let sequences = sequence_snapshot(pool).await;
            let tokens = cas_tokens(pool).await;
            let cas_before = next_cas_token(pool).await;
            let error = restore(pool, archive)
                .await
                .expect_err("valid envelope must reach SQL and fail on the original constraint");
            match error {
                DbDumpError::Db(ref error) => assert_sqlstate(error, code),
                other => panic!("expected database constraint error {code}, got {other}"),
            }
            assert_eq!(
                snapshot(pool, &tables).await,
                before,
                "rollback every table including cooldowns/ledgers"
            );
            assert_eq!(
                triggers(pool).await,
                guards,
                "rollback trigger modes/definitions: {revision_mode}/{audit_mode}"
            );
            assert_eq!(
                sequence_snapshot(pool).await,
                sequences,
                "rollback sequence restarts"
            );
            assert_eq!(cas_tokens(pool).await, tokens, "rollback keeps CAS tokens");
            assert!(
                next_cas_token(pool).await <= cas_before,
                "CAS allocation never moves back up"
            );
        }
    }
    set_guard_modes(pool, "O", "O").await;
    assert_guards_work(pool).await;

    // A refusal in the settings-version reset comes after all INSERTs, guard
    // re-enabling and most owned-sequence resets. It catches nontransactional setval
    // regressions that an earlier constraint error cannot exercise.
    sqlx::query("SELECT setval('guild_settings_version_seq', 7, TRUE)")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("ALTER SEQUENCE guild_settings_version_seq MAXVALUE 83")
        .execute(pool)
        .await
        .unwrap();
    let before = snapshot(pool, &tables).await;
    let guards = triggers(pool).await;
    let sequences = sequence_snapshot(pool).await;
    let tokens = cas_tokens(pool).await;
    let cas_before = next_cas_token(pool).await;
    let error = restore(pool, &source)
        .await
        .expect_err("restored max version 83 exhausts target MAXVALUE 83");
    assert!(matches!(error, DbDumpError::Refused(_)), "{error}");
    assert!(error.to_string().contains("exhausted"), "{error}");
    assert_eq!(snapshot(pool, &tables).await, before);
    assert_eq!(triggers(pool).await, guards);
    assert_eq!(
        sequence_snapshot(pool).await,
        sequences,
        "owned and standalone sequence state survives late refusal"
    );
    assert_eq!(cas_tokens(pool).await, tokens);
    assert!(next_cas_token(pool).await <= cas_before);
    assert_guards_work(pool).await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn v3_prefix_restores_into_migrated_schema_without_retaining_newer_target_data() {
    let Some(db) = database().await else {
        return;
    };
    let pool = db.pool();
    seed(pool).await;
    let tables = covered_tables(pool).await;
    let before = snapshot(pool, &tables).await;
    let ledgers = vec![
        "_sqlx_migrations".to_owned(),
        "schema_migrations".to_owned(),
    ];
    let ledger_before = snapshot(pool, &ledgers).await;
    let guards = triggers(pool).await;
    let directory = ArchiveDirectory::new();
    let source = directory.path("v4-source.ndjson.gz");
    dump(pool, &source).await.unwrap();
    let mut legacy = inspect(&source).unwrap();
    legacy.manifest.version = 3;
    legacy.manifest.sequence_marks.clear();
    // The frozen writer's fixture is tested in backup_roundtrip. Here synthesize
    // its required 22-table envelope using valid REAL migrated prefix rows, and
    // omit every later table to exercise cutover compatibility (no fake DDL).
    legacy.manifest.tables = DUMP_TABLES
        .iter()
        .take(22)
        .map(|name| {
            let mut table = legacy
                .manifest
                .tables
                .iter()
                .find(|table| table.name == *name)
                .cloned()
                .unwrap_or_else(|| DumpTableInfo {
                    name: (*name).to_owned(),
                    columns: Vec::new(),
                    column_types: Vec::new(),
                    count: 0,
                });
            if let Some(rows) = legacy.buffers.get_mut(*name) {
                for row in rows {
                    for (column, ty) in table.columns.iter().zip(&table.column_types) {
                        let value = row.get_mut(column).unwrap();
                        if let Some(text) = value.as_str() {
                            match ty.as_str() {
                                "bigint" | "integer" | "smallint" => {
                                    *value = json!(text.parse::<i64>().unwrap())
                                }
                                "boolean" => *value = json!(text == "true"),
                                "json" | "jsonb" => *value = serde_json::from_str(text).unwrap(),
                                _ => {}
                            }
                        }
                    }
                }
            }
            table.column_types.clear();
            table
        })
        .collect();
    legacy.manifest.tables.reverse();
    legacy
        .buffers
        .retain(|name, _| DUMP_TABLES.iter().take(22).any(|table| *table == name));
    legacy.rows = legacy.manifest.tables.iter().map(|table| table.count).sum();
    let legacy_path = directory.path("v3-real-migrated-prefix.ndjson.gz");
    write_archive(&legacy_path, &legacy);

    // Missing legacy subsystems may be omitted only when their archive is empty.
    // Refuse nonempty data BEFORE touching current tables or guard modes.
    let absent_name = OPTIONAL_LEGACY_TABLES[0];
    let mut incompatible = copy_contents(&legacy);
    let absent = incompatible
        .manifest
        .tables
        .iter_mut()
        .find(|table| table.name == absent_name)
        .unwrap();
    absent.columns = vec!["flagged".to_owned()];
    absent.count = 1;
    incompatible.buffers.insert(
        absent_name.to_owned(),
        vec![serde_json::Map::from_iter([(
            "flagged".to_owned(),
            json!(true),
        )])],
    );
    incompatible.rows += 1;
    let incompatible_path = directory.path("nonempty-absent-legacy.ndjson.gz");
    write_archive(&incompatible_path, &incompatible);
    let all_tables = actual_tables(pool).await;
    let unchanged = snapshot(pool, &all_tables).await;
    let sequences = sequence_snapshot(pool).await;
    let error = restore(pool, &incompatible_path).await.unwrap_err();
    assert!(error
        .to_string()
        .contains(&format!("{absent_name} is absent from target")));
    assert_eq!(snapshot(pool, &all_tables).await, unchanged);
    assert_eq!(triggers(pool).await, guards);
    assert_eq!(sequence_snapshot(pool).await, sequences);

    let report = restore(pool, &legacy_path).await.unwrap();
    assert!(report.ok, "v3's revision baseline is explicitly counted");
    assert!(report.dropped_columns.is_empty());
    assert_eq!(
        report.initialized_tables,
        BTreeMap::from([("guild_settings_revision".to_owned(), 1)])
    );
    let after = snapshot(pool, &tables).await;
    for (table, archived) in &before {
        if DUMP_TABLES.iter().take(22).any(|name| *name == table) {
            assert_eq!(
                &after[table], archived,
                "{table}: native v3 cells survive on real schema"
            );
        } else if table == "guild_settings_revision" {
            assert_eq!(
                after[table].rows,
                vec![vec![Some("true".into()), Some("0".into())]],
                "only required singleton is initialized"
            );
        } else {
            assert!(
                after[table].rows.is_empty(),
                "v3 must clear stale newer data in {table}"
            );
        }
    }
    assert_eq!(triggers(pool).await, guards);
    assert_eq!(snapshot(pool, &ledgers).await, ledger_before);
    let version: i64 = sqlx::query_scalar("INSERT INTO guild_settings (guild_id, key, value, version, updated_by) VALUES ('100000000000000001', 'TWO_BACKUP_TEST', 'true', nextval('guild_settings_version_seq'), 'v3-probe') RETURNING version").fetch_one(pool).await.unwrap();
    assert_eq!(
        version, 2,
        "v3-empty standalone version sequence resumes past the target's issued 1"
    );
    let revision: i64 =
        sqlx::query_scalar("SELECT revision FROM guild_settings_revision WHERE singleton = TRUE")
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(
        revision, 1,
        "v3 baseline permits the next normal settings write"
    );
    let audit: i64 = sqlx::query_scalar("INSERT INTO guild_settings_audit (guild_id, key, actor) VALUES ('100000000000000001', 'TWO_BACKUP_TEST', 'v3-probe') RETURNING id").fetch_one(pool).await.unwrap();
    assert_eq!(
        audit, 1,
        "v3-missing audit history resumes an empty serial sequence"
    );
    assert_guards_work(pool).await;
    db.close().await.unwrap();
}

/// The bootstrap's service URL with only the database path replaced.
/// TestDatabase::create already refused anything but agent_test with an
/// explicit empty password on agent-testdb:5432 and no query, so the shipped
/// binary below can reach nothing else.
fn fixture_url(db: &TestDatabase) -> String {
    let bootstrap = std::env::var("TWO_TEST_DATABASE_URL").expect("bootstrap already validated");
    let (service, _) = bootstrap
        .rsplit_once('/')
        .expect("validated bootstrap names a database");
    format!("{service}/{}", db.name())
}

/// The drill script interpolates both paths unquoted, as the unit does.
fn shell_word(path: &Path) -> &str {
    let text = path.to_str().expect("UTF-8 path");
    assert!(
        !text.is_empty()
            && text
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b)),
        "path must not need shell quoting: {text}"
    );
    text
}

/// The shipped drill's own ExecStart body, with only its archive directory and
/// binary replaced, so a changed selector or restore invocation is exercised
/// here rather than a copy of it.
fn shipped_drill_script(archives: &Path, binary: &Path) -> String {
    let unit = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../deploy/two-bot-next-restore-drill.service"),
    )
    .expect("read the shipped restore drill unit");
    // systemd joins a line ending in a backslash with the next one and
    // replaces the backslash with a space (systemd.syntax(7)).
    let joined = unit.replace("\\\n", " ");
    let exec = joined
        .lines()
        .find_map(|line| line.strip_prefix("ExecStart="))
        .expect("drill unit has an ExecStart");
    let script = exec
        .trim_end()
        .strip_prefix("/usr/bin/bash -o pipefail -c '")
        .and_then(|rest| rest.strip_suffix('\''))
        .expect("drill runs one single-quoted bash -o pipefail script");
    for shipped in ["/var/backups/two-bot-next/", "/opt/two-bot-next/two-bot "] {
        assert_eq!(script.matches(shipped).count(), 1, "{shipped} in {script}");
    }
    script
        .replace(
            "/var/backups/two-bot-next/",
            &format!("{}/", shell_word(archives)),
        )
        .replace(
            "/opt/two-bot-next/two-bot ",
            &format!("{} ", shell_word(binary)),
        )
}

fn shipped_cli(command: &mut Command, label: &str) -> Output {
    let output = command.output().expect("start the shipped two-bot binary");
    // Drill evidence for the CI log. The CLI never prints database URLs.
    println!(
        "--- {label}: {}\n{}{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

async fn schema_version(pool: &PgPool) -> (i64, String, i64) {
    sqlx::query_as(
        "SELECT version, description, (SELECT count(*) FROM _sqlx_migrations) \
         FROM _sqlx_migrations ORDER BY version DESC LIMIT 1",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

fn assert_drill_verified(output: &Output, archive: &Path, tables: &[String]) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "drill failed");
    assert!(
        stdout.contains(&format!(
            "drill: restoring {} into the scratch database",
            archive.display()
        )),
        "the drill must select the newest archive, not the older decoy"
    );
    assert_eq!(stdout.lines().last(), Some("RESTORE VERIFIED"));
    assert!(!stdout.contains("MISMATCH"));
    assert!(!stderr.contains("WARNING"), "no baseline rows initialized");
    assert!(
        !stderr.contains("does not have"),
        "no archived column dropped"
    );
    let verified = stdout
        .lines()
        .filter(|line| line.contains(" manifest ") && line.ends_with(" ok"))
        .count();
    assert_eq!(verified, tables.len(), "one verified count line per table");
    for table in tables {
        assert!(
            stdout
                .lines()
                .any(|line| line.trim_start().starts_with(&format!("{table} "))
                    && line.ends_with(" ok")),
            "{table} restored with a matching count"
        );
    }
}

/// The shipped monthly drill on the complete migrated schema (TOG-11804): the
/// built `two-bot backup`, then the restore drill unit's own ExecStart script,
/// into an independent freshly migrated scratch database, twice. CI's backup
/// CLI step supplies the binary; without TWO_BOT_TEST_BACKUP_BIN this skips.
#[tokio::test]
async fn shipped_backup_and_restore_drill_recover_the_complete_migrated_schema() {
    let Some(binary) = std::env::var_os("TWO_BOT_TEST_BACKUP_BIN").map(PathBuf::from) else {
        eprintln!("SKIP shipped restore drill: TWO_BOT_TEST_BACKUP_BIN is not set");
        return;
    };
    let Some(db) = database().await else {
        return;
    };
    let source = db.pool();
    seed(source).await;
    let tables = covered_tables(source).await;
    let before = snapshot(source, &tables).await;
    let version = schema_version(source).await;
    println!(
        "drill: schema at migration {} {} ({} applied), {} bot-owned tables",
        version.0,
        version.1,
        version.2,
        tables.len()
    );

    // An older archive the newest-file selector must pass over.
    let directory = ArchiveDirectory::new();
    let decoy = directory.path("two-funnel-20000101T000000Z.ndjson.gz");
    std::fs::write(&decoy, b"not a backup").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&decoy)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(946_684_800))
        .unwrap();
    let backup = shipped_cli(
        Command::new(&binary)
            .arg("backup")
            .current_dir(&directory.0)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("TWO_DATABASE_URL", fixture_url(&db))
            .env("TWO_BACKUP_DIR", &directory.0)
            .env("TWO_BACKUP_KEEP", "14"),
        "two-bot backup",
    );
    assert!(backup.status.success(), "backup failed");
    assert!(String::from_utf8_lossy(&backup.stdout).contains("backup: done"));
    let published: Vec<PathBuf> = std::fs::read_dir(&directory.0)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| *path != decoy)
        .collect();
    let [archive] = published.as_slice() else {
        panic!("exactly one published archive besides the decoy: {published:?}");
    };
    let contents = inspect(archive).unwrap();
    assert_archive_matches(&contents, &before);
    assert_eq!(contents.manifest.events_sequence, 107);

    // Independent, freshly migrated scratch target, as the drill unit expects.
    let target = database().await.expect("test bootstrap already configured");
    let pool = target.pool();
    assert_eq!(schema_version(pool).await, version);
    sqlx::raw_sql(
        "CREATE TABLE schema_migrations (id TEXT PRIMARY KEY); \
         INSERT INTO schema_migrations VALUES ('destination-ledger-only'); \
         ALTER TABLE members ADD COLUMN backup_identity BIGINT GENERATED ALWAYS AS IDENTITY \
             (START WITH 17 INCREMENT BY 3) UNIQUE;",
    )
    .execute(pool)
    .await
    .unwrap();
    let guard_before = triggers(pool).await;
    let script = shipped_drill_script(&directory.0, &binary);
    let target_url = fixture_url(&target);
    // Month one restores into a dirtied scratch; month two into the previous
    // drill's result, as the monthly timer does.
    let mut floors = BTreeMap::new();
    for month in 1..=2 {
        dirty_target(pool).await;
        // Keep the floors in force before the last restore, as the
        // reversed-rows caller does: dirty_target hands out target-only
        // identities whose rows restore wipes, but the allocator must not
        // rewind onto them.
        floors.extend(allocation_floors(pool, &contents.manifest.sequence_marks).await);
        let cas_before = next_cas_token(pool).await;
        let drill = shipped_cli(
            Command::new("/usr/bin/bash")
                .args(["-o", "pipefail", "-c", script.as_str()])
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("TWO_RESTORE_URL", &target_url),
            &format!("restore drill, month {month}"),
        );
        assert_drill_verified(&drill, archive, &tables);
        let after = snapshot(pool, &tables).await;
        for table in &tables {
            println!(
                "drill: month {month} {table:32} source {:5} restored {:5}",
                before[table].rows.len(),
                after[table].rows.len()
            );
        }
        assert_eq!(after, before, "month {month}: every archived row");
        assert_fresh_cas_tokens(pool, cas_before).await;
        assert_eq!(triggers(pool).await, guard_before);
        let cooldowns: i64 = sqlx::query_scalar("SELECT count(*) FROM xp_cooldowns")
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(cooldowns, 0, "excluded throttles are not replayed");
    }
    assert_guards_work(pool).await;

    let allocations = allocate_owned_sequences(pool, false, &floors).await;
    for ((table, column), (next, increment)) in &allocations {
        println!("drill: sequence {table}.{column} resumed at {next} (increment {increment})");
    }
    assert_default_inserts(pool, &allocations).await;
    let allocated_version: i64 = sqlx::query_scalar("SELECT nextval('guild_settings_version_seq')")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(
        allocated_version, 84,
        "standalone settings version resumes beyond restored max"
    );
    println!("drill: sequence guild_settings_version_seq resumed at {allocated_version}");
    let inserted_version: i64 = sqlx::query_scalar("INSERT INTO guild_settings (guild_id, key, value, version, updated_by) VALUES ('100000000000000001', 'TWO_BACKUP_TEST', 'true', nextval('guild_settings_version_seq'), 'post-restore') RETURNING version").fetch_one(pool).await.unwrap();
    assert_eq!(inserted_version, 85);
    for (table, statement) in [
        ("internal_nonces", "INSERT INTO internal_nonces SELECT * FROM internal_nonces WHERE nonce_hash = repeat('a', 64)"),
        ("internal_discord_events", "INSERT INTO internal_discord_events SELECT * FROM internal_discord_events WHERE event_hash = repeat('3', 64)"),
        ("internal_idempotency", "INSERT INTO internal_idempotency (caller_hash, key_hash, action, payload_hash, state) VALUES (repeat('b', 64), repeat('c', 64), 'role.assign', repeat('d', 64), 'in_flight')"),
        ("feed_deliveries", "INSERT INTO feed_deliveries SELECT * FROM feed_deliveries WHERE item_key = 'backup:item:pending'"),
        ("self_role_panel_claims", "INSERT INTO self_role_panel_claims SELECT * FROM self_role_panel_claims"),
        ("automod_delivery_claims", "INSERT INTO automod_delivery_claims SELECT * FROM automod_delivery_claims"),
    ] {
        assert_sqlstate(&sqlx::query(audited(statement.to_owned())).execute(pool).await.unwrap_err(), "23505");
        println!("drill: replay refused (23505) in {table}");
    }
    target.close().await.unwrap();
    db.close().await.unwrap();
}
