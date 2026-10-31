//! EXPLAIN seq-scan check for the `web_v1` read contract (report-only).
//!
//! The website reads only `web_v1` views, so every view is a hot-path query
//! the bot owns. This test migrates a scratch schema, applies the contract,
//! then runs `EXPLAIN SELECT * FROM <view>` for every view it finds (a new
//! view is picked up with no edit here) on one session with
//! `enable_seqscan = off`. A scratch database is nearly empty, so the planner
//! would pick a sequential scan everywhere; with sequential scans disabled a
//! `Seq Scan` node survives only when no index can serve the relation, which
//! is the signal that matters and does not depend on table size.
//!
//! Report-only: findings go to stderr as GitHub `::warning` lines and to the
//! job summary (`GITHUB_STEP_SUMMARY`). Set `EXPLAIN_SEQSCAN_ENFORCE=1` to
//! make any finding outside [`ALLOWED`] fail the test; flip that on once the
//! allowlist reflects the reviewed whole-table views (counts, aggregates).
//!
//! Only agent-testdb or a CI service container is admitted, with the
//! agent_test identity. DATABASE_URL is never consulted.
//!
//! Run: `cargo test -p two-bot-store --locked --test explain_seqscan -- --ignored`
//! with `TEST_DATABASE_URL` pointed at the authorized test database.
use std::collections::BTreeSet;
use std::io::Write;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Pool, Postgres};
use two_bot_store::{apply_web_contract, migrate, DB_POOL_MAX};

/// Reviewed `(view, relation)` pairs that scan a whole table by design
/// (counters, funnels). Empty until the first report is triaged.
const ALLOWED: &[(&str, &str)] = &[];

static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);

/// Relation names of every sequential-scan node in one `EXPLAIN` text plan,
/// including parallel scans. A schema qualifier is dropped.
fn seq_scan_relations(plan: &[String]) -> BTreeSet<String> {
    const MARKER: &str = "Seq Scan on ";
    plan.iter()
        .filter_map(|line| {
            let rest = &line[line.find(MARKER)? + MARKER.len()..];
            let token = rest.split_whitespace().next()?;
            Some(token.rsplit('.').next().unwrap_or(token).to_owned())
        })
        .collect()
}

#[test]
fn plan_parser_finds_plain_parallel_and_qualified_scans() {
    let plan: Vec<String> = [
        "Hash Join  (cost=1.0..2.0 rows=1 width=4)",
        "  ->  Seq Scan on members m  (cost=0.00..1.00 rows=1 width=4)",
        "        Disabled: true",
        "  ->  Parallel Seq Scan on s6_test_1.events  (cost=0.00..1.00 rows=1 width=4)",
        "  ->  Index Scan using events_pkey on events e  (cost=0.15..8.17 rows=1 width=4)",
        "  ->  Bitmap Heap Scan on funnel  (cost=4.2..8.0 rows=1 width=4)",
    ]
    .iter()
    .map(|line| (*line).to_owned())
    .collect();
    assert_eq!(
        seq_scan_relations(&plan),
        BTreeSet::from(["events".to_owned(), "members".to_owned()])
    );
    assert!(seq_scan_relations(&[]).is_empty());
}

struct Fixture {
    admin: Pool<Postgres>,
    pool: Pool<Postgres>,
    schema: String,
}

impl Fixture {
    async fn new() -> Self {
        let url = std::env::var("TEST_DATABASE_URL").expect("set TEST_DATABASE_URL for DB tests");
        let options = PgConnectOptions::from_str(&url).expect("invalid TEST_DATABASE_URL");
        let ci_container = std::env::var("CI").as_deref() == Ok("true")
            && matches!(options.get_host(), "localhost" | "127.0.0.1" | "postgres");
        assert!(
            options.get_host() == "agent-testdb" || ci_container,
            "test containers only"
        );
        assert_eq!(options.get_username(), "agent_test", "test identity only");
        let admin = two_bot_store::connect_pool_with_tls(
            &url,
            two_bot_core::database_tls::TlsPolicy::LocalOnly,
        )
        .await
        .expect("test DB connect failed");
        let schema = format!(
            "s6_explain_{}_{}",
            std::process::id(),
            NEXT_SCHEMA.fetch_add(1, Ordering::Relaxed)
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .unwrap();
        let pool = PgPoolOptions::new()
            .max_connections(DB_POOL_MAX)
            .connect_with(options.options([
                ("search_path", schema.clone()),
                ("statement_timeout", "15000ms".to_owned()),
            ]))
            .await
            .unwrap();
        Self {
            admin,
            pool,
            schema,
        }
    }

    async fn finish(self) {
        self.pool.close().await;
        // Only schemas created by this fixture; never shared tables or roles.
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA IF EXISTS {}_web_v1 CASCADE; DROP SCHEMA {} CASCADE",
            self.schema, self.schema
        )))
        .execute(&self.admin)
        .await
        .unwrap();
        self.admin.close().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn web_contract_views_report_unindexed_seq_scans() {
    let f = Fixture::new().await;
    migrate(&f.pool).await.unwrap();
    apply_web_contract(&f.pool).await.unwrap();
    let web = format!("{}_web_v1", f.schema);
    let views: Vec<String> = sqlx::query_scalar(
        "SELECT table_name::text FROM information_schema.views
         WHERE table_schema = $1 ORDER BY 1",
    )
    .bind(&web)
    .fetch_all(&f.pool)
    .await
    .unwrap();
    assert!(
        views.len() >= 10,
        "web contract views vanished from {web}: {views:?}"
    );
    assert!(
        views.iter().any(|view| view == "rank_ladder"),
        "web_v1.rank_ladder missing from {web}: {views:?}"
    );

    // One session for the setting and every EXPLAIN.
    let mut conn = f.pool.acquire().await.unwrap();
    sqlx::query("SET enable_seqscan = off")
        .execute(&mut *conn)
        .await
        .unwrap();
    let mut findings = Vec::new();
    for view in &views {
        let plan: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "EXPLAIN SELECT * FROM {web}.{view}"
        )))
        .fetch_all(&mut *conn)
        .await
        .unwrap();
        for relation in seq_scan_relations(&plan) {
            if !ALLOWED.contains(&(view.as_str(), relation.as_str())) {
                findings.push((view.clone(), relation));
            }
        }
    }
    drop(conn);

    let mut summary = String::from("### EXPLAIN seq-scan check (web_v1, report-only)\n\n");
    summary.push_str(&format!(
        "{} views checked with `enable_seqscan = off`; {} unindexed seq-scan finding(s).\n\n",
        views.len(),
        findings.len()
    ));
    for (view, relation) in &findings {
        // Direct fd write on purpose: libtest captures `eprintln!` output for
        // passing tests, and this check is report-only (it always passes), so
        // the `::warning` lines would never reach the CI log or annotations.
        // `writeln!` to `Stderr` bypasses that capture. Do not switch back to
        // `eprintln!` without adding `--nocapture` to the CI step.
        let _ = writeln!(
            std::io::stderr(),
            "::warning title=EXPLAIN seq scan::web_v1.{view} seq-scans {relation} even with seq scans disabled"
        );
        summary.push_str(&format!("- `web_v1.{view}` seq-scans `{relation}`\n"));
    }
    if let Some(path) = std::env::var_os("GITHUB_STEP_SUMMARY") {
        if let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(path) {
            let _ = file.write_all(summary.as_bytes());
        }
    }
    f.finish().await;
    if std::env::var("EXPLAIN_SEQSCAN_ENFORCE").as_deref() == Ok("1") {
        assert!(findings.is_empty(), "seq-scan findings: {findings:?}");
    }
}
