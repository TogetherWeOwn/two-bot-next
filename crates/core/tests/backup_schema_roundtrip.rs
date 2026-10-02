//! Complete-schema backup acceptance on the actual cutover migrations.
//!
//! Default (not ignored) tests; CI's integration step enables `db` and supplies
//! TWO_TEST_DATABASE_URL. Only an absent variable skips: invalid configuration,
//! failed migrations and connection errors fail through TestDatabase's guards.
//! No placeholder legacy tables or application/staging database connections.

#![cfg(feature = "db")]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use sqlx::{PgPool, Row};
use two_bot_core::backup::dump::{dump, restore, DbDumpError};
use two_bot_core::backup::dump_file::{
    finish_gzip, inspect, is_destination_owned, new_encoder, write_line, DumpContents,
    DumpTableInfo, DESTINATION_OWNED_COLUMNS, DUMP_TABLES, DUMP_VERSION, EXCLUDED_TABLES,
    OPTIONAL_LEGACY_TABLES,
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
/// not transactional, so even a rolled-back restore consumes CAS tokens. Tests
/// assert that sequence only descends instead ([`next_cas_token`]).
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

async fn allocate_owned_sequences(
    pool: &PgPool,
    empty: bool,
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
        let expected = maximum.map_or(sequence.start, |maximum| maximum + sequence.increment);
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
    for (revision_mode, audit_mode) in [("O", "O"), ("A", "R"), ("R", "D"), ("D", "A")] {
        set_guard_modes(pool, revision_mode, audit_mode).await;
        let modes_before = triggers(pool).await;
        dirty_target(pool).await;
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
    let allocations = allocate_owned_sequences(pool, false).await;
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
async fn empty_archived_sequence_tables_restart_at_the_target_configured_start() {
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
    let report = restore(pool, &empty_path).await.unwrap();
    assert!(report.ok);
    assert!(report.initialized_tables.is_empty());
    for table in sequence_tables {
        assert_eq!(report.restored[table], 0, "{table} must really be empty");
    }
    let allocations = allocate_owned_sequences(pool, true).await;
    assert_eq!(
        allocations[&("members".into(), "backup_identity".into())].0,
        17
    );
    let version: i64 = sqlx::query_scalar("SELECT nextval('guild_settings_version_seq')")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(
        version, 1,
        "empty standalone sequence restarts at its configured start"
    );
    let intent: i64 = sqlx::query_scalar("INSERT INTO internal_idempotency (caller_hash, key_hash, action, payload_hash, state) VALUES (repeat('4', 64), repeat('5', 64), 'event.read', repeat('6', 64), 'in_flight') RETURNING intent_id").fetch_one(pool).await.unwrap();
    assert_eq!(intent, 2);
    let audit: i64 = sqlx::query_scalar("INSERT INTO internal_action_log (intent_id, phase, caller_hash, action) VALUES ($1, 'intent', repeat('4', 64), 'event.read') RETURNING audit_id").bind(intent).fetch_one(pool).await.unwrap();
    assert_eq!(audit, 2);
    let identity: i64 = sqlx::query_scalar("INSERT INTO members (guild_id, member_id) VALUES ('100000000000000001', 'empty-default-member') RETURNING backup_identity").fetch_one(pool).await.unwrap();
    assert_eq!(identity, 20);
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
                "rollback TRUNCATE RESTART IDENTITY"
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

    // A refusal in the LAST sequence reset comes after all INSERTs, guard
    // re-enabling and owned-sequence resets. It catches nontransactional setval
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
        version, 1,
        "v3-empty standalone version sequence starts at 1"
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
