//! Builder-owned database tests for `rollback-delta import`'s apply path.
//!
//! The protected acceptance file (`rollback_delta_import_acceptance.rs`)
//! exercises the binary end to end on natural-key tables only and pins no
//! serial-column or conflict-divergence policy. These tests call
//! [`two_bot_cutover::rollback_import::apply_import`] in-process against
//! disposable `agent-testdb` databases, so coverage sees the apply paths and
//! the review's rollback hazards stay pinned: FK child-before-parent files
//! still apply, re-runs report inserted vs skipped rows, owned sequences
//! advance past imported maxima, files without `events` rows never touch
//! an `events` table, LFG posts round-trip with their parent-joined roles
//! and signups, divergent updates are reported per table with keys instead
//! of silently dropped, the divergence check is primary-keyed (index lookup,
//! not a table scan) inside a UTC-pinned transaction with destination-owned
//! columns excluded, identical re-runs stay clean on non-UTC targets, every
//! live FK between exportable tables respects the parent-first apply order,
//! the keyless `member_erasure_audit` never duplicates on re-run, and
//! `guild_settings` re-runs ignore the target-allocated `cas_version` while
//! still flagging genuinely changed values.
//!
//! Same disposable-database pattern as the protected acceptance file: one
//! owned database per test on `agent-testdb`, never staging or production.
//! Skips without `TWO_TEST_DATABASE_URL`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::postgres::{PgPoolOptions, PgSslMode};
use sqlx::PgPool;
use two_bot_cutover::legacy_copy::options::guarded_target;
use two_bot_cutover::rollback_delta::export_delta;
use two_bot_cutover::rollback_import::{
    apply_import, divergence_check_sql, import_table_order, is_exportable_table, parse_import,
    primary_key_columns, ImportApplyError,
};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

struct TestDb {
    admin: PgPool,
    pool: PgPool,
    /// Pool whose search_path is only a non-public schema, if the test has one.
    pinned: Option<PgPool>,
    name: String,
}

impl TestDb {
    async fn new(
        role: &str,
        migrations: &[&str],
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let name = format!(
            "two_bot_test_rbapply_{role}_{}_{}_{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        );
        let url = format!("postgres://agent_test:@agent-testdb:5432/{name}");
        // Never read an application URL or substitute its credentials.
        let options = guarded_target(&url, false)?
            .ssl_mode(PgSslMode::Disable)
            .options([("timezone", "UTC"), ("statement_timeout", "10000")]);
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(options.clone().database("postgres"))
            .await?;
        sqlx::query(database_sql("CREATE DATABASE", &name))
            .execute(&admin)
            .await?;
        // Anything failing after CREATE DATABASE must not leak the database.
        match Self::provision(&admin, options, &name, migrations).await {
            Ok(db) => Ok(db),
            Err(error) => {
                let _ = sqlx::query(database_sql("DROP DATABASE", &name))
                    .execute(&admin)
                    .await;
                Err(error)
            }
        }
    }

    async fn provision(
        admin: &PgPool,
        options: sqlx::postgres::PgConnectOptions,
        name: &str,
        migrations: &[&str],
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await?;
        for migration in migrations.iter().copied() {
            sqlx::raw_sql(sqlx::AssertSqlSafe(migration))
                .execute(&pool)
                .await?;
        }
        Ok(Self {
            admin: admin.clone(),
            pool,
            pinned: None,
            name: name.to_owned(),
        })
    }

    /// Like [`TestDb::new`], plus `schema` and a pool whose search_path names
    /// only that schema (never a connection redirect: same database and
    /// principal). `schema_migrations` run inside it, resolving unqualified
    /// names there. `public_migrations` run in the default schema first.
    async fn new_pinned(
        role: &str,
        schema: &str,
        public_migrations: &[&str],
        schema_migrations: &[&str],
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        assert!(schema.len() <= 40 && schema.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
        let mut db = Self::new(role, public_migrations).await?;
        match db.pin(schema, schema_migrations).await {
            Ok(pool) => {
                db.pinned = Some(pool);
                Ok(db)
            }
            Err(error) => {
                let _ = db.cleanup().await;
                Err(error)
            }
        }
    }

    async fn pin(
        &self,
        schema: &str,
        migrations: &[&str],
    ) -> Result<PgPool, Box<dyn std::error::Error + Send + Sync>> {
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&self.pool)
            .await?;
        let path = schema.to_owned();
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
            .connect_with(self.pool.connect_options().as_ref().clone())
            .await?;
        for migration in migrations.iter().copied() {
            if let Err(error) = sqlx::raw_sql(sqlx::AssertSqlSafe(migration))
                .execute(&pool)
                .await
            {
                pool.close().await;
                return Err(error.into());
            }
        }
        Ok(pool)
    }

    /// The pinned pool; the test must have used [`TestDb::new_pinned`].
    fn pinned(&self) -> &PgPool {
        self.pinned.as_ref().expect("pinned schema pool")
    }

    /// Best-effort teardown: every step runs, the first failure is reported.
    async fn cleanup(&self) -> TestResult {
        if let Some(pinned) = &self.pinned {
            pinned.close().await;
        }
        self.pool.close().await;
        let dropped = sqlx::query(database_sql("DROP DATABASE", &self.name))
            .execute(&self.admin)
            .await
            .map(|_| ());
        self.admin.close().await;
        Ok(dropped?)
    }
}

fn database_sql(operation: &str, name: &str) -> sqlx::AssertSqlSafe<String> {
    assert!(name.starts_with("two_bot_test_rbapply_") && name.len() <= 63);
    assert!(name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'));
    sqlx::AssertSqlSafe(format!("{operation} \"{name}\""))
}

fn db_opted_in() -> bool {
    if std::env::var_os("TWO_TEST_DATABASE_URL").is_none() {
        eprintln!("skipped: TWO_TEST_DATABASE_URL opt-in required for agent-testdb");
        return false;
    }
    true
}

fn line(table: &str, row: &serde_json::Value) -> String {
    serde_json::json!({"table": table, "row": row}).to_string()
}

fn ticket_row(id: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "guild_id": "g",
        "channel_id": format!("ch-{id}"),
        "opener_id": "o1",
        "status": "closed",
        "created_at": "2026-10-01T12:00:00+00:00",
        "closed_at": "2026-10-02T12:00:00+00:00",
    })
}

fn transcript_row(ticket_id: &str) -> serde_json::Value {
    serde_json::json!({
        "ticket_id": ticket_id,
        "guild_id": "g",
        "channel_id": format!("ch-{ticket_id}"),
        "opener_id": "o1",
        "content": "closed with a transcript",
        "message_count": 3,
        "created_at": "2026-10-01T13:00:00+00:00",
        "purge_after": "2027-01-01T00:00:00+00:00",
    })
}

/// A child row listed before its parent still applies: the import replays
/// parent-first, so the transcript's immediate FK to `tickets` is satisfied.
#[tokio::test]
async fn apply_import_reorders_child_before_parent() -> TestResult {
    if !db_opted_in() {
        return Ok(());
    }
    let db = TestDb::new("fkorder", &[include_str!("../migrations/0210_tickets.sql")]).await?;
    let result = async {
        // Exporter (TABLE_SPECS) order puts the transcript first; keep that
        // file order here so the test pins the reorder, not the fixture.
        let contents = format!(
            "{}\n{}\n",
            line("ticket_transcripts", &transcript_row("t-fk1")),
            line("tickets", &ticket_row("t-fk1")),
        );
        let rows = parse_import(&contents).expect("valid fk fixture");
        let outcome = apply_import(&db.pool, &rows).await.expect("fk import");
        assert_eq!(outcome.inserted, 2, "both rows land");
        assert_eq!(outcome.skipped, 0, "nothing pre-existed");
        assert_eq!(outcome.divergent, 0, "no conflicts to classify");
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ticket_transcripts")
            .fetch_one(&db.pool)
            .await?;
        assert_eq!(count, 1, "transcript row present");
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    let cleaned = db.cleanup().await;
    result.and(cleaned)
}

/// Re-running an identical import inserts nothing and reports every row as
/// already present instead of claiming fresh writes.
#[tokio::test]
async fn apply_import_reports_inserted_vs_skipped() -> TestResult {
    if !db_opted_in() {
        return Ok(());
    }
    let db = TestDb::new("counts", &[include_str!("../migrations/0210_tickets.sql")]).await?;
    let result = async {
        let contents = format!(
            "{}\n{}\n",
            line("tickets", &ticket_row("t-c1")),
            line("tickets", &ticket_row("t-c2")),
        );
        let rows = parse_import(&contents).expect("valid count fixture");
        let first = apply_import(&db.pool, &rows).await.expect("first import");
        assert_eq!((first.inserted, first.skipped, first.total()), (2, 0, 2));
        assert_eq!(first.divergent, 0, "fresh rows never diverge");
        let second = apply_import(&db.pool, &rows).await.expect("rerun import");
        assert_eq!((second.inserted, second.skipped, second.total()), (0, 2, 2));
        assert_eq!(
            second.divergent, 0,
            "identical re-run skips without divergence"
        );
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tickets")
            .fetch_one(&db.pool)
            .await?;
        assert_eq!(count, 2, "re-run duplicates nothing");
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    let cleaned = db.cleanup().await;
    result.and(cleaned)
}

/// An explicitly imported `BIGSERIAL` id advances its sequence in the same
/// transaction: the bot's next id-less insert lands past the import instead
/// of colliding with it.
#[tokio::test]
async fn apply_import_advances_owned_sequences() -> TestResult {
    if !db_opted_in() {
        return Ok(());
    }
    let db = TestDb::new(
        "sequences",
        &[include_str!("../migrations/0002_leveling.sql")],
    )
    .await?;
    let result = async {
        // Id 1 collides with the sequence's first value on a fresh target:
        // without the advancement this test's follow-up insert fails with a
        // duplicate primary key.
        let contents = format!(
            "{}\n",
            line(
                "xp_awards",
                &serde_json::json!({
                    "id": 1,
                    "guild_id": "g",
                    "member_id": "m1",
                    "source": "message",
                    "xp": 5,
                    "occurred_at": "2026-10-01T12:00:00+00:00",
                }),
            ),
        );
        let rows = parse_import(&contents).expect("valid award fixture");
        let outcome = apply_import(&db.pool, &rows).await.expect("award import");
        assert_eq!((outcome.inserted, outcome.skipped), (1, 0));
        assert_eq!(outcome.divergent, 0, "fresh award never diverges");
        let next: i64 = sqlx::query_scalar(
            "INSERT INTO xp_awards (guild_id, member_id, source, xp, occurred_at) \
             VALUES ('g', 'm2', 'voice', 7, '2026-10-02T12:00:00+00:00') RETURNING id",
        )
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(next, 2, "sequence resumes past the imported maximum");
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    let cleaned = db.cleanup().await;
    result.and(cleaned)
}

/// Files without `events` rows never touch an `events` table: a partially
/// migrated target that lacks it still imports.
#[tokio::test]
async fn apply_import_without_events_needs_no_events_table() -> TestResult {
    if !db_opted_in() {
        return Ok(());
    }
    // 0210 only: no `events` table exists on this target.
    let db = TestDb::new(
        "noevents",
        &[include_str!("../migrations/0210_tickets.sql")],
    )
    .await?;
    let result = async {
        let contents = format!("{}\n", line("tickets", &ticket_row("t-ne1")));
        let rows = parse_import(&contents).expect("valid no-events fixture");
        let outcome = apply_import(&db.pool, &rows)
            .await
            .expect("no-events import");
        assert_eq!((outcome.inserted, outcome.skipped), (1, 0));
        assert_eq!(outcome.divergent, 0, "fresh ticket never diverges");
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    let cleaned = db.cleanup().await;
    result.and(cleaned)
}

fn lfg_post_row(id: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "guild_id": "g",
        "channel_id": "ch-lfg",
        "message_id": null,
        "title": "Friday raid",
        "starts_at": "2026-10-01T18:00:00+00:00",
        "status": "open",
        "created_by": "o1",
        "created_at": "2026-10-01T12:00:00+00:00",
        "closed_at": null,
    })
}

fn lfg_role_row(lfg_id: &str) -> serde_json::Value {
    serde_json::json!({
        "lfg_id": lfg_id,
        "role_key": "tank",
        "label": "Tank",
        "slots": 1,
        "position": 0,
    })
}

fn lfg_signup_row(lfg_id: &str) -> serde_json::Value {
    serde_json::json!({
        "lfg_id": lfg_id,
        "user_id": "u1",
        "role_key": "tank",
        "joined_at": "2026-10-01T13:00:00+00:00",
    })
}

/// A post-freeze LFG post round-trips with its parent-joined roles and
/// signups: the `(lfg_id, role_key)` FK is satisfied even when the file lists
/// the signup first.
#[tokio::test]
async fn apply_import_round_trips_lfg_post_with_roles_and_signup() -> TestResult {
    if !db_opted_in() {
        return Ok(());
    }
    let db = TestDb::new("lfg", &[include_str!("../migrations/0170_lfg.sql")]).await?;
    let result = async {
        // Child-first file order pins the parent-first replay, not the fixture.
        let contents = format!(
            "{}\n{}\n{}\n",
            line("lfg_signups", &lfg_signup_row("p-lfg1")),
            line("lfg_roles", &lfg_role_row("p-lfg1")),
            line("lfg_posts", &lfg_post_row("p-lfg1")),
        );
        let rows = parse_import(&contents).expect("valid lfg fixture");
        let outcome = apply_import(&db.pool, &rows).await.expect("lfg import");
        assert_eq!((outcome.inserted, outcome.skipped), (3, 0));
        assert_eq!(outcome.divergent, 0, "fresh LFG rows never diverge");
        let tables: Vec<&str> = outcome.per_table.iter().map(|t| t.table.as_str()).collect();
        assert_eq!(
            tables,
            vec!["lfg_posts", "lfg_roles", "lfg_signups"],
            "parents apply before children"
        );
        for table in ["lfg_posts", "lfg_roles", "lfg_signups"] {
            let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT COUNT(*) FROM \"{table}\""
            )))
            .fetch_one(&db.pool)
            .await?;
            assert_eq!(count, 1, "{table} row present");
        }
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    let cleaned = db.cleanup().await;
    result.and(cleaned)
}

/// A signup without its parent-joined roles fails closed and names the table
/// and constraint, instead of a redacted generic with nothing imported.
#[tokio::test]
async fn apply_import_missing_lfg_roles_names_table_and_constraint() -> TestResult {
    if !db_opted_in() {
        return Ok(());
    }
    let db = TestDb::new("lfgmiss", &[include_str!("../migrations/0170_lfg.sql")]).await?;
    let result = async {
        let contents = format!(
            "{}\n{}\n",
            line("lfg_posts", &lfg_post_row("p-lfg2")),
            line("lfg_signups", &lfg_signup_row("p-lfg2")),
        );
        let rows = parse_import(&contents).expect("valid lfg fixture without roles");
        let error = apply_import(&db.pool, &rows)
            .await
            .expect_err("missing roles must fail");
        match error {
            two_bot_cutover::rollback_import::ImportApplyError::ConstraintViolation {
                table,
                constraint,
            } => {
                assert!(
                    table.contains("lfg_signups"),
                    "names the failing table: {table}"
                );
                assert!(
                    constraint.contains("lfg_signups"),
                    "names the failing constraint: {constraint}"
                );
            }
            other => panic!("expected a named constraint violation, got {other}"),
        }
        let posts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM lfg_posts")
            .fetch_one(&db.pool)
            .await?;
        assert_eq!(posts, 0, "single transaction rolls back");
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    let cleaned = db.cleanup().await;
    result.and(cleaned)
}

/// The exporter parent-joins roles to every exported post, so the file the
/// importer reads already carries what the FK needs.
#[tokio::test]
async fn export_delta_parent_joins_lfg_roles() -> TestResult {
    if !db_opted_in() {
        return Ok(());
    }
    let db = TestDb::new("lfgexp", &[include_str!("../migrations/0170_lfg.sql")]).await?;
    let result = async {
        sqlx::query(
            "INSERT INTO lfg_posts (id, guild_id, channel_id, title, starts_at, status, created_by, created_at) \
             VALUES ('p-exp', 'g', 'ch', 'Friday raid', '2026-10-01T18:00:00+00:00', 'open', 'o1', '2026-10-01T12:00:00+00:00')",
        )
        .execute(&db.pool)
        .await?;
        sqlx::query(
            "INSERT INTO lfg_roles (lfg_id, role_key, label, slots, position) \
             VALUES ('p-exp', 'tank', 'Tank', 1, 0)",
        )
        .execute(&db.pool)
        .await?;
        sqlx::query(
            "INSERT INTO lfg_signups (lfg_id, user_id, role_key, joined_at) \
             VALUES ('p-exp', 'u1', 'tank', '2026-10-01T13:00:00+00:00')",
        )
        .execute(&db.pool)
        .await?;
        let mut lines = Vec::new();
        let exported = export_delta(&db.pool, "2026-09-30T12:00:00Z", &mut |line: String| {
            lines.push(line);
            Ok(())
        })
        .await?;
        assert!(exported >= 3, "post plus parent-joined roles plus signup");
        let mut tables: Vec<String> = Vec::new();
        let mut saw_roles_for_post = false;
        for line in &lines {
            let value: serde_json::Value = serde_json::from_str(line)?;
            let table = value
                .get("table")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_owned();
            tables.push(table.clone());
            if table == "lfg_roles"
                && value
                    .get("row")
                    .and_then(|r| r.get("lfg_id"))
                    .and_then(serde_json::Value::as_str)
                    == Some("p-exp")
            {
                saw_roles_for_post = true;
            }
        }
        assert!(tables.contains(&"lfg_posts".to_owned()), "post exported");
        assert!(tables.contains(&"lfg_signups".to_owned()), "signup exported");
        assert!(saw_roles_for_post, "roles parent-joined to the exported post");
        // The emitted file is itself importable: the parent join is in the
        // importer's vocabulary.
        for table in tables {
            assert!(
                is_exportable_table(&table),
                "exported table is importable: {table}"
            );
        }
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    let cleaned = db.cleanup().await;
    result.and(cleaned)
}

fn member_level_row(member: &str, xp: i64) -> serde_json::Value {
    serde_json::json!({
        "guild_id": "g",
        "member_id": member,
        "xp": xp,
        "message_xp": xp,
        "voice_xp": 0,
        "imported_xp": 0,
        "updated_at": "2026-10-01T12:00:00+00:00",
    })
}

/// A first run that drops a post-freeze update reports it as divergent with
/// its key, while an identical re-run reports the same skip counts with zero
/// divergence: the two are distinguishable and the exit code can tell them
/// apart.
#[tokio::test]
async fn apply_import_flags_divergent_updates_per_table() -> TestResult {
    if !db_opted_in() {
        return Ok(());
    }
    let db = TestDb::new(
        "divergent",
        &[include_str!("../migrations/0002_leveling.sql")],
    )
    .await?;
    let result = async {
        sqlx::query(
            "INSERT INTO member_levels (guild_id, member_id, xp, message_xp, voice_xp, imported_xp, updated_at) \
             VALUES ('g', 'm-div', 10, 10, 0, 0, '2026-09-29T12:00:00+00:00')",
        )
        .execute(&db.pool)
        .await?;
        let contents = format!("{}\n", line("member_levels", &member_level_row("m-div", 17)));
        let rows = parse_import(&contents).expect("valid divergent fixture");
        let outcome = apply_import(&db.pool, &rows)
            .await
            .expect("divergent import");
        assert_eq!((outcome.inserted, outcome.skipped), (0, 1));
        assert_eq!(outcome.divergent, 1, "dropped update is divergent");
        assert_eq!(outcome.per_table.len(), 1);
        assert_eq!(outcome.per_table[0].table, "member_levels");
        assert_eq!(outcome.per_table[0].divergent, 1);
        assert!(
            outcome
                .divergent_keys
                .iter()
                .any(|k| k.contains("member_levels") && k.contains("m-div")),
            "divergent key names table and row: {:?}",
            outcome.divergent_keys
        );
        let xp: i64 = sqlx::query_scalar(
            "SELECT xp FROM member_levels WHERE guild_id = 'g' AND member_id = 'm-div'",
        )
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(xp, 10, "target values kept");
        // An identical re-run (the baseline row that matches the target)
        // skips the same row with zero divergence.
        let rerun = format!("{}\n", line("member_levels", &{
            let mut row = member_level_row("m-div", 10);
            row["updated_at"] = serde_json::json!("2026-09-29T12:00:00+00:00");
            row
        }));
        let rerun_rows = parse_import(&rerun).expect("valid rerun fixture");
        let rerun_outcome = apply_import(&db.pool, &rerun_rows)
            .await
            .expect("rerun import");
        assert_eq!((rerun_outcome.inserted, rerun_outcome.skipped), (0, 1));
        assert_eq!(
            rerun_outcome.divergent, 0,
            "identical re-run is not divergent"
        );
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    let cleaned = db.cleanup().await;
    result.and(cleaned)
}

/// The parent-first order holds for every live FK between exportable tables,
/// not just the hard-coded pairs: read the real constraint graph and assert
/// the apply order puts each parent first.
#[tokio::test]
async fn import_table_order_respects_live_foreign_keys() -> TestResult {
    if !db_opted_in() {
        return Ok(());
    }
    let db = TestDb::new(
        "fkgraph",
        &[
            include_str!("../migrations/0210_tickets.sql"),
            include_str!("../migrations/0180_feeds.sql"),
            include_str!("../migrations/0350_internal_actions.sql"),
            include_str!("../migrations/0170_lfg.sql"),
            include_str!("../migrations/0120_channel_moderation.sql"),
            include_str!("../migrations/0123_channel_execution_fence.sql"),
            include_str!("../migrations/0224_voice_rooms.sql"),
            include_str!("../migrations/0416_voice_room_privacy.sql"),
            include_str!("../migrations/0418_voice_join_grants.sql"),
            include_str!("../migrations/0413_voice_owner_grants.sql"),
            include_str!("../migrations/0226_voice_text_channels.sql"),
            include_str!("../migrations/0200_self_roles.sql"),
            include_str!("../migrations/0201_self_role_intent_initialization.sql"),
            include_str!("../migrations/0202_self_role_compensation_phase.sql"),
            include_str!("../migrations/0203_self_role_pending_exchange.sql"),
            include_str!("../migrations/0204_self_role_terminal_repair.sql"),
            include_str!("../migrations/0205_self_role_exchange_receipts.sql"),
            include_str!("../migrations/0206_self_role_exchange_baselines.sql"),
            include_str!("../migrations/0300_website_contract.sql"),
            include_str!("../migrations/0370_containment_claims.sql"),
        ],
    )
    .await?;
    let result = async {
        let edges: Vec<(String, String)> = sqlx::query_as(
            "SELECT child.relname::text, parent.relname::text \
             FROM pg_constraint c \
             JOIN pg_class child ON child.oid = c.conrelid \
             JOIN pg_class parent ON parent.oid = c.confrelid \
             JOIN pg_namespace n ON n.oid = child.relnamespace \
             WHERE c.contype = 'f' AND n.nspname = 'public'",
        )
        .fetch_all(&db.pool)
        .await?;
        assert!(!edges.is_empty(), "test target carries foreign keys");
        let mut checked = 0;
        for (child, parent) in &edges {
            if !(is_exportable_table(child) && is_exportable_table(parent)) {
                continue;
            }
            assert!(
                import_table_order(parent) < import_table_order(child),
                "parent-first order violated: {parent} must precede {child}"
            );
            checked += 1;
        }
        assert!(
            checked >= 8,
            "expected the exportable FK graph to be covered, checked only {checked}: {edges:?}"
        );
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    let cleaned = db.cleanup().await;
    result.and(cleaned)
}

/// An identical re-run reports zero divergent rows even when the target
/// session defaults to a non-UTC zone: the apply transaction pins UTC like
/// the exporter, so `to_jsonb` renders timestamptz identically. A genuinely
/// changed row through the same session still reports divergent, so the pin
/// does not neuter the signal.
#[tokio::test]
async fn apply_import_identical_rerun_stable_under_non_utc_timezone() -> TestResult {
    if !db_opted_in() {
        return Ok(());
    }
    let db = TestDb::new("nonutc", &[include_str!("../migrations/0002_leveling.sql")]).await?;
    let result = async {
        let row = member_level_row("m-tz", 17);
        let contents = format!("{}\n", line("member_levels", &row));
        let rows = parse_import(&contents).expect("valid tz fixture");
        let first = apply_import(&db.pool, &rows).await.expect("tz import");
        assert_eq!(
            (first.inserted, first.skipped, first.divergent),
            (1, 0, 0),
            "fresh row lands"
        );
        // A session whose default zone is New York: without the UTC pin the
        // re-run would render `updated_at` with a -04:00 offset and report a
        // false divergent row. `after_connect` sets the hostile default at
        // session scope (connect options cannot: sqlx pins `TimeZone: UTC` in
        // every startup packet), and the single connection keeps this test
        // deterministic.
        use sqlx::Executor as _;
        let ny_options = guarded_target(
            &format!("postgres://agent_test:@agent-testdb:5432/{}", db.name),
            false,
        )
        .map_err(|refused| -> Box<dyn std::error::Error + Send + Sync> {
            format!("test database refused: {refused}").into()
        })?
        .ssl_mode(PgSslMode::Disable);
        let ny_pool = PgPoolOptions::new()
            .max_connections(1)
            .after_connect(|conn, _| {
                Box::pin(async move {
                    conn.execute("SET TIME ZONE 'America/New_York'").await?;
                    Ok(())
                })
            })
            .connect_with(ny_options)
            .await?;
        let zone: String = sqlx::query_scalar("SHOW TIMEZONE")
            .fetch_one(&ny_pool)
            .await?;
        assert_ne!(zone, "UTC", "test session really runs non-UTC: {zone}");
        let rerun = apply_import(&ny_pool, &rows).await.expect("tz rerun");
        let changed_contents =
            format!("{}\n", line("member_levels", &member_level_row("m-tz", 42)));
        let changed_rows = parse_import(&changed_contents).expect("valid changed fixture");
        let changed = apply_import(&ny_pool, &changed_rows)
            .await
            .expect("tz changed import");
        ny_pool.close().await;
        assert_eq!(
            (rerun.inserted, rerun.skipped, rerun.divergent),
            (0, 1, 0),
            "identical re-run under a non-UTC session is not divergent"
        );
        assert_eq!(
            (changed.inserted, changed.skipped, changed.divergent),
            (0, 1, 1),
            "a dropped update still reports divergent under a non-UTC session"
        );
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    let cleaned = db.cleanup().await;
    result.and(cleaned)
}

/// One changed row in a table with thousands of existing rows classifies via
/// the primary key: the divergence query apply_import runs plans as an index
/// lookup, not a sequential scan.
#[tokio::test]
async fn apply_import_divergence_check_uses_primary_key() -> TestResult {
    if !db_opted_in() {
        return Ok(());
    }
    let db = TestDb::new("pkdiv", &[include_str!("../migrations/0002_leveling.sql")]).await?;
    let result = async {
        sqlx::query(
            "INSERT INTO member_levels (guild_id, member_id, xp, message_xp, voice_xp, imported_xp, updated_at) \
             SELECT 'g', 'bulk-' || g, 0, 0, 0, 0, '2026-09-01T12:00:00+00:00' \
             FROM generate_series(1, 3000) g",
        )
        .execute(&db.pool)
        .await?;
        // One post-freeze update among thousands of untouched rows.
        let changed = member_level_row("bulk-1500", 17);
        let contents = format!("{}\n", line("member_levels", &changed));
        let rows = parse_import(&contents).expect("valid keyed fixture");
        let outcome = apply_import(&db.pool, &rows)
            .await
            .expect("keyed divergent import");
        assert_eq!(
            (outcome.inserted, outcome.skipped, outcome.divergent),
            (0, 1, 1),
            "the single changed row classifies divergent"
        );
        assert!(
            outcome
                .divergent_keys
                .iter()
                .any(|k| k.contains("member_levels") && k.contains("bulk-1500")),
            "divergent key names the row: {:?}",
            outcome.divergent_keys
        );
        // The check apply_import ran is the builder's keyed SQL, and it plans
        // as a primary-key lookup.
        let pk = primary_key_columns(&db.pool, "member_levels").await?;
        assert_eq!(
            pk,
            vec!["guild_id".to_owned(), "member_id".to_owned()],
            "composite PK in index order"
        );
        let sql = divergence_check_sql("member_levels", &pk)?;
        let plan_rows: Vec<String> =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("EXPLAIN {sql}")))
                .bind(&changed)
                .fetch_all(&db.pool)
                .await?;
        let plan = plan_rows.join("\n");
        assert!(
            plan.contains("Index"),
            "divergence check uses the PK index: {plan}"
        );
        assert!(
            !plan.contains("Seq Scan"),
            "divergence check never sequentially scans: {plan}"
        );
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    let cleaned = db.cleanup().await;
    result.and(cleaned)
}

/// The keyless `member_erasure_audit` has no primary or unique key, so bare
/// `ON CONFLICT DO NOTHING` can never skip: the import inserts only when no
/// identical row exists, and an identical re-run stays a clean skip.
#[tokio::test]
async fn apply_import_keyless_table_rerun_does_not_duplicate() -> TestResult {
    if !db_opted_in() {
        return Ok(());
    }
    let db = TestDb::new(
        "keyless",
        &[include_str!("../migrations/0410_member_erasure_audit.sql")],
    )
    .await?;
    let result = async {
        let row = serde_json::json!({
            "actor": "op1",
            "erased_at": "2026-10-01T12:00:00+00:00",
        });
        let contents = format!("{}\n", line("member_erasure_audit", &row));
        let rows = parse_import(&contents).expect("valid erasure fixture");
        let first = apply_import(&db.pool, &rows).await.expect("first import");
        assert_eq!(
            (first.inserted, first.skipped, first.divergent),
            (1, 0, 0),
            "fresh erasure receipt lands"
        );
        let second = apply_import(&db.pool, &rows).await.expect("rerun import");
        assert_eq!(
            (second.inserted, second.skipped, second.divergent),
            (0, 1, 0),
            "identical re-run skips without divergence"
        );
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM member_erasure_audit")
            .fetch_one(&db.pool)
            .await?;
        assert_eq!(count, 1, "re-run duplicates nothing");
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    let cleaned = db.cleanup().await;
    result.and(cleaned)
}

fn guild_settings_row(value_text: &str) -> serde_json::Value {
    serde_json::json!({
        "guild_id": "g",
        "key": "WELCOME_MESSAGE",
        "value": {"text": value_text},
        "version": 1,
        // The exporter emits Next's token; the target trigger replaces it
        // with its own allocation on every insert.
        "cas_version": -77,
        "updated_at": "2026-10-01T12:00:00+00:00",
        "updated_by": "tester",
    })
}

/// `guild_settings.cas_version` is destination-owned: the row trigger replaces
/// any supplied token, so an identical re-run must ignore it (divergent 0),
/// while a genuinely changed value still reports divergent.
#[tokio::test]
async fn apply_import_guild_settings_ignores_destination_owned_cas() -> TestResult {
    if !db_opted_in() {
        return Ok(());
    }
    let db = TestDb::new(
        "gscas",
        &[
            include_str!("../migrations/0330_guild_settings.sql"),
            include_str!("../migrations/0331_guild_settings_versions.sql"),
            include_str!("../migrations/0332_guild_settings_allocator.sql"),
            include_str!("../migrations/0333_guild_settings_revision.sql"),
            include_str!("../migrations/0334_guild_settings_cas.sql"),
        ],
    )
    .await?;
    let result = async {
        let contents = format!("{}\n", line("guild_settings", &guild_settings_row("hi")));
        let rows = parse_import(&contents).expect("valid settings fixture");
        let first = apply_import(&db.pool, &rows).await.expect("first import");
        assert_eq!(
            (first.inserted, first.skipped, first.divergent),
            (1, 0, 0),
            "fresh settings row lands"
        );
        let stored_cas: i64 = sqlx::query_scalar(
            "SELECT cas_version FROM guild_settings WHERE guild_id = 'g' AND key = 'WELCOME_MESSAGE'",
        )
        .fetch_one(&db.pool)
        .await?;
        assert_ne!(
            stored_cas, -77,
            "target allocates its own token, ignoring the export: {stored_cas}"
        );
        let rerun = apply_import(&db.pool, &rows).await.expect("rerun import");
        assert_eq!(
            (rerun.inserted, rerun.skipped, rerun.divergent),
            (0, 1, 0),
            "identical re-run ignores the owned token"
        );
        // A changed value through the same path still reports divergent and
        // keeps the target values.
        let changed_contents = format!("{}\n", line("guild_settings", &guild_settings_row("bye")));
        let changed_rows = parse_import(&changed_contents).expect("changed fixture");
        let changed = apply_import(&db.pool, &changed_rows)
            .await
            .expect("changed import");
        assert_eq!(
            (changed.inserted, changed.skipped, changed.divergent),
            (0, 1, 1),
            "a dropped settings update still reports divergent"
        );
        assert!(
            changed
                .divergent_keys
                .iter()
                .any(|k| k.contains("guild_settings")),
            "divergent key names the table: {:?}",
            changed.divergent_keys
        );
        let stored_value: serde_json::Value = sqlx::query_scalar(
            "SELECT value FROM guild_settings WHERE guild_id = 'g' AND key = 'WELCOME_MESSAGE'",
        )
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(
            stored_value,
            serde_json::json!({"text": "hi"}),
            "target values kept"
        );
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    let cleaned = db.cleanup().await;
    result.and(cleaned)
}

/// Minimal identity-bearing audit table for the pre-activation import case.
const AUDIT_TABLE_SQL: &str = "CREATE TABLE internal_action_log (
    audit_id BIGINT PRIMARY KEY, created_at TIMESTAMPTZ NOT NULL)";
const AUDIT_TRIGGER_SQL: &str = include_str!("../migrations/0474_internal_audit_order.sql");

fn audit_line() -> String {
    line(
        "internal_action_log",
        &serde_json::json!({"audit_id": 7, "created_at": "2026-10-01T12:00:00+00:00"}),
    )
}

/// With the ordering trigger installed, audit rows refuse before any write, in
/// every trigger state, and leave even an unrelated row of the file unapplied.
#[tokio::test]
async fn apply_import_refuses_audit_rows_when_ordering_trigger_installed() -> TestResult {
    if !db_opted_in() {
        return Ok(());
    }
    let db = TestDb::new(
        "auditguard",
        &[
            include_str!("../migrations/0210_tickets.sql"),
            // Activation binds both audit tables; use their real base migration.
            include_str!("../migrations/0350_internal_actions.sql"),
            AUDIT_TRIGGER_SQL,
        ],
    )
    .await?;
    let result = async {
        let contents = format!(
            "{}\n{}\n",
            line("tickets", &ticket_row("t-a1")),
            audit_line()
        );
        let rows = parse_import(&contents).expect("valid audit fixture");
        for state in ["ENABLE", "DISABLE"] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "ALTER TABLE internal_action_log {state} TRIGGER internal_audit_allocate_order"
            )))
            .execute(&db.pool)
            .await?;
            let error = apply_import(&db.pool, &rows)
                .await
                .expect_err("must refuse");
            assert!(
                matches!(error, ImportApplyError::AuditOrderingInstalled { rows: 1 }),
                "{state}: {error}"
            );
        }
        // Catalog damage must not turn an activated installation into an
        // unfenced identity import. The retained state still proves activation.
        sqlx::raw_sql("DROP TRIGGER internal_audit_allocate_order ON internal_action_log; ALTER FUNCTION public.internal_audit_allocate_order() RENAME TO hidden_allocator")
            .execute(&db.pool).await?;
        assert!(matches!(apply_import(&db.pool, &rows).await,
            Err(ImportApplyError::AuditOrderingInstalled { rows: 1 })));
        let tickets: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tickets")
            .fetch_one(&db.pool)
            .await?;
        let audits: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM internal_action_log")
            .fetch_one(&db.pool)
            .await?;
        assert_eq!((tickets, audits), (0, 0), "nothing was written");
        let allocator_unchanged: bool = sqlx::query_scalar(
            "SELECT last_audit_id = 0 AND last_created_at = TIMESTAMPTZ '1970-01-01 00:00:00+00'
             FROM internal_audit_order WHERE singleton",
        )
        .fetch_one(&db.pool)
        .await?;
        assert!(allocator_unchanged, "refused imports never advance allocation");
        // An audit-free file is not blocked by the installed trigger.
        let free = parse_import(&line("tickets", &ticket_row("t-a2"))).expect("valid");
        let outcome = apply_import(&db.pool, &free)
            .await
            .expect("audit-free import");
        assert_eq!(outcome.inserted, 1);
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    let cleaned = db.cleanup().await;
    result.and(cleaned)
}

/// Without the trigger the audit import keeps its identity-preserving behavior.
#[tokio::test]
async fn apply_import_keeps_audit_identity_without_ordering_trigger() -> TestResult {
    if !db_opted_in() {
        return Ok(());
    }
    let db = TestDb::new("auditfree", &[AUDIT_TABLE_SQL]).await?;
    let result = async {
        let rows = parse_import(&audit_line()).expect("valid audit fixture");
        let outcome = apply_import(&db.pool, &rows).await.expect("audit import");
        assert_eq!(outcome.inserted, 1);
        let id: i64 = sqlx::query_scalar("SELECT audit_id FROM internal_action_log")
            .fetch_one(&db.pool)
            .await?;
        assert_eq!(id, 7, "audit_id preserved");
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    let cleaned = db.cleanup().await;
    result.and(cleaned)
}

const NON_PUBLIC: &str = "audit_nonpublic";

/// The audit allocator lives in a non-public schema and `public` does not
/// exist. The importer resolves the audit table through search_path, so the
/// fence must follow it: audit rows refuse before any write in every trigger
/// state and under schema-qualified catalog damage, and the allocator position
/// and unrelated rows stay put.
#[tokio::test]
async fn apply_import_refuses_audit_rows_when_allocator_is_outside_public() -> TestResult {
    if !db_opted_in() {
        return Ok(());
    }
    let db = TestDb::new_pinned(
        "anp",
        NON_PUBLIC,
        &[],
        &[
            include_str!("../migrations/0210_tickets.sql"),
            include_str!("../migrations/0350_internal_actions.sql"),
            AUDIT_TRIGGER_SQL,
        ],
    )
    .await?;
    let result = async {
        let pool = db.pinned();
        sqlx::raw_sql("DROP SCHEMA public CASCADE")
            .execute(&db.pool)
            .await?;
        let public_absent: bool = sqlx::query_scalar("SELECT to_regnamespace('public') IS NULL")
            .fetch_one(pool)
            .await?;
        assert!(public_absent, "public must not hold any allocator");
        // An unrelated destination row and a non-initial allocator position.
        let seeded = parse_import(&line("tickets", &ticket_row("t-pre"))).expect("valid");
        assert_eq!(apply_import(pool, &seeded).await?.inserted, 1);
        sqlx::raw_sql("UPDATE internal_audit_order SET last_audit_id = 41")
            .execute(pool)
            .await?;
        let contents = format!(
            "{}\n{}\n",
            line("tickets", &ticket_row("t-a1")),
            audit_line()
        );
        let rows = parse_import(&contents).expect("valid audit fixture");
        for state in ["ENABLE", "DISABLE"] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "ALTER TABLE internal_action_log {state} TRIGGER internal_audit_allocate_order"
            )))
            .execute(pool)
            .await?;
            let error = apply_import(pool, &rows).await.expect_err("must refuse");
            assert!(
                matches!(error, ImportApplyError::AuditOrderingInstalled { rows: 1 }),
                "{state}: {error}"
            );
        }
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DROP TRIGGER internal_audit_allocate_order ON internal_action_log; \
             ALTER FUNCTION {NON_PUBLIC}.internal_audit_allocate_order() RENAME TO hidden_allocator"
        )))
        .execute(pool)
        .await?;
        assert!(matches!(
            apply_import(pool, &rows).await,
            Err(ImportApplyError::AuditOrderingInstalled { rows: 1 })
        ));
        let tickets: Vec<String> = sqlx::query_scalar("SELECT id FROM tickets ORDER BY id")
            .fetch_all(pool)
            .await?;
        assert_eq!(tickets, ["t-pre"], "no row of a refused file was written");
        let audits: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM internal_action_log")
            .fetch_one(pool)
            .await?;
        assert_eq!(audits, 0, "no audit row was written");
        let allocator: i64 =
            sqlx::query_scalar("SELECT last_audit_id FROM internal_audit_order WHERE singleton")
                .fetch_one(pool)
                .await?;
        assert_eq!(allocator, 41, "refused imports never move the allocator");
        // An audit-free file is not blocked by the installed allocator.
        let free = parse_import(&line("tickets", &ticket_row("t-a2"))).expect("valid");
        assert_eq!(apply_import(pool, &free).await?.inserted, 1);
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    let cleaned = db.cleanup().await;
    result.and(cleaned)
}

/// Control: an allocator in `public` that the target's resolved audit table
/// does not use must not fence an unactivated non-public target, and the
/// imported identity is preserved exactly. The unrelated allocator stays idle.
#[tokio::test]
async fn apply_import_ignores_unrelated_public_allocator_for_unactivated_target() -> TestResult {
    if !db_opted_in() {
        return Ok(());
    }
    let db = TestDb::new_pinned(
        "aother",
        NON_PUBLIC,
        &[
            include_str!("../migrations/0350_internal_actions.sql"),
            AUDIT_TRIGGER_SQL,
        ],
        &[AUDIT_TABLE_SQL],
    )
    .await?;
    let result = async {
        let pool = db.pinned();
        let resolved: String = sqlx::query_scalar(
            "SELECT n.nspname::text FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.oid = to_regclass('internal_action_log')",
        )
        .fetch_one(pool)
        .await?;
        assert_eq!(
            resolved, NON_PUBLIC,
            "the importer resolves the non-public table"
        );
        let rows = parse_import(&audit_line()).expect("valid audit fixture");
        let outcome = apply_import(pool, &rows).await.expect("audit import");
        assert_eq!(outcome.inserted, 1);
        let preserved: bool = sqlx::query_scalar(
            "SELECT audit_id = 7 AND created_at = TIMESTAMPTZ '2026-10-01 12:00:00+00' \
             FROM internal_action_log",
        )
        .fetch_one(pool)
        .await?;
        assert!(preserved, "audit_id and created_at preserved");
        let public_idle: bool = sqlx::query_scalar(
            "SELECT last_audit_id = 0 AND last_created_at = TIMESTAMPTZ '1970-01-01 00:00:00+00' \
                 AND NOT EXISTS (SELECT 1 FROM public.internal_action_log) \
             FROM public.internal_audit_order WHERE singleton",
        )
        .fetch_one(&db.pool)
        .await?;
        assert!(public_idle, "the unrelated public allocator was not used");
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    let cleaned = db.cleanup().await;
    result.and(cleaned)
}
