use super::*;
use serde_json::Value;
use sqlx::PgPool;
use tokio::sync::{mpsc, Mutex};
use two_bot_cutover::retention::{handle_guild_delete, request_user_deletion};
use two_bot_testsupport::TestDatabase;

pub(super) static RUNTIME_LOCK: Mutex<()> = Mutex::const_new(());
pub(super) const GUILD: &str = "111111111111111111";
const DEPARTED: &str = "222222222222222222";
const MEMBER: &str = "333333333333333333";

pub(super) async fn database() -> Option<TestDatabase> {
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure retention runtime fixtures"
            );
            eprintln!("SKIP retention runtime database fixture: TWO_TEST_DATABASE_URL not set");
            return None;
        }
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create guarded retention fixture"),
    )
}

pub(super) async fn snapshot(pool: &PgPool) -> Vec<Value> {
    let mut rows = Vec::new();
    for table in [
        "presence_probe",
        "events",
        "operational_audit_log",
        "members",
        "member_levels",
        "member_ranks",
        "retention_guild_purge_queue",
        "retention_deletion_requests",
        "internal_idempotency",
    ] {
        let sql = format!("SELECT COALESCE(jsonb_agg(to_jsonb(r) ORDER BY to_jsonb(r)::text), '[]'::jsonb) FROM {table} r");
        rows.push(
            sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
                .fetch_one(pool)
                .await
                .unwrap(),
        );
    }
    rows
}

pub(super) async fn seed(pool: &PgPool) -> String {
    sqlx::query("INSERT INTO presence_probe (guild_id, observed_at, approximate_presence_count) VALUES ($1, to_char(now() - interval '31 days', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), 12)")
        .bind(GUILD).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO events (event_type, member_id, guild_id, occurred_at, source, metadata, idempotency_key) VALUES ('voice_session_ended', $2, $1, now() - interval '31 days', 'fixture', '{}', 'retention-runtime-voice')")
        .bind(GUILD).bind(MEMBER).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO operational_audit_log (entry_id, event_kind, guild_id, occurred_at, actor_id, target_id, metadata_json, created_at) VALUES ('retention-runtime-audit', 'fixture', $1, now() - interval '46 days', $2, $2, '{}', now())")
        .bind(GUILD).bind(MEMBER).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO members (guild_id, member_id) VALUES ($1, $3), ($2, $3)")
        .bind(GUILD)
        .bind(DEPARTED)
        .bind(MEMBER)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO member_levels (guild_id, member_id, xp, message_xp, updated_at) VALUES ($1, $2, 150, 150, now() - interval '60 days')")
        .bind(DEPARTED).bind(MEMBER).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO member_ranks (guild_id, member_id, updated_at) VALUES ($1, $2, now() - interval '60 days')")
        .bind(DEPARTED).bind(MEMBER).execute(pool).await.unwrap();
    handle_guild_delete(pool, DEPARTED, None).await.unwrap();
    request_user_deletion(pool, GUILD, MEMBER).await.unwrap()
}

/// Run the REAL retention action through the production spawn/cancel/join owner.
/// The wrapper only forwards completion to HTTP shutdown; it does not fake the
/// report, call cutover directly, or introduce its own scheduler.
async fn tick(pool: &PgPool, mode: RetentionMode) -> (Result<(), ErrorClass>, String) {
    let capture = crate::tracing_capture::global();
    let offset = capture.text().len();
    let mut registered = job(pool.clone(), mode);
    registered.startup_jitter = Duration::ZERO;
    let actual = registered.action.clone();
    let (tx, mut rx) = mpsc::unbounded_channel();
    registered.action = Arc::new(move || {
        let (actual, tx) = (actual.clone(), tx.clone());
        Box::pin(async move {
            let result = actual().await;
            tx.send(result).unwrap();
            result
        })
    });
    let status = jobs::statuses(&[registered.name], false);
    let (stop, _) = watch::channel(false);
    let outcome = Arc::new(Mutex::new(None));
    let observed = outcome.clone();
    tokio::time::timeout(
        Duration::from_secs(30),
        website_jobs::serve_jobs(vec![registered], status, stop, async move {
            *observed.lock().await = Some(rx.recv().await.expect("supervised retention attempt"));
            Ok(())
        }),
    )
    .await
    .expect("retention attempt must complete within fixture deadline")
    .unwrap();
    let result = outcome.lock().await.take().unwrap();
    (result, capture.text()[offset..].to_owned())
}

fn report_line(logs: &str) -> &str {
    let reports: Vec<_> = logs
        .lines()
        .filter(|line| {
            line.contains("message=retention_run") || line.contains("message=\"retention_run\"")
        })
        .collect();
    assert_eq!(
        reports.len(),
        1,
        "one runtime report per completed attempt: {logs}"
    );
    reports[0]
}

fn field(line: &str, name: &str, expected: u64) {
    let needle = format!("{name}={expected}");
    assert!(
        line.split_whitespace().any(|part| part == needle),
        "missing exact report field {needle}: {line}"
    );
}

#[tokio::test]
async fn dry_run_reports_candidates_and_preserves_raw_queue_requests_and_receipts() {
    let _serial = RUNTIME_LOCK.lock().await;
    let Some(db) = database().await else { return };
    seed(db.pool()).await;
    let before = snapshot(db.pool()).await;
    let (result, logs) = tick(db.pool(), mode_from_setting(None).unwrap().unwrap()).await;
    result.unwrap();
    let report = report_line(&logs);
    field(report, "presence_raw_deleted", 1);
    field(report, "voice_raw_deleted", 1);
    field(report, "audit_cache_deleted", 1);
    field(report, "guild_rows_purged", 3);
    field(report, "deletions_completed", 1);
    field(report, "guild_purges_failed", 0);
    field(report, "deletions_deferred", 0);
    field(report, "deletions_overdue", 0);
    assert!(report.contains("mode=DryRun") || report.contains("mode=\"DryRun\""));
    assert_eq!(
        snapshot(db.pool()).await,
        before,
        "dry-run must be byte-identical, including receipt and queue timestamps"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn runtime_surfaces_deferred_overdue_and_completes_after_replay_fence_settles() {
    let _serial = RUNTIME_LOCK.lock().await;
    let Some(db) = database().await else { return };
    let pool = db.pool();
    sqlx::query("INSERT INTO members (guild_id, member_id) VALUES ($1, $2)")
        .bind(GUILD)
        .bind(MEMBER)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO internal_idempotency (caller_hash, key_hash, action, payload_hash, state, guild_id, actor_id, target_id) VALUES (repeat('a', 64), repeat('b', 64), 'role.assign', repeat('c', 64), 'unknown', $1, $2, $2)")
        .bind(GUILD).bind(MEMBER).execute(pool).await.unwrap();
    let id = request_user_deletion(pool, GUILD, MEMBER).await.unwrap();
    sqlx::query("UPDATE retention_deletion_requests SET requested_at = now() - interval '73 hours' WHERE request_id = $1").bind(&id).execute(pool).await.unwrap();
    for _ in 0..2 {
        let before = snapshot(pool).await;
        let (_, logs) = tick(pool, RetentionMode::Execute).await;
        let report = report_line(&logs);
        assert!(
            report.starts_with("WARN "),
            "partial Ok must not be a silent success: {report}"
        );
        field(report, "deletions_deferred", 1);
        field(report, "deletions_overdue", 1);
        field(report, "deletions_completed", 0);
        assert_eq!(
            snapshot(pool).await,
            before,
            "unresolved fence must preserve pending work and no receipt"
        );
    }
    // Resolve the synthetic replay fence, never bypass the erasure guard.
    sqlx::query("UPDATE internal_idempotency SET state = 'completed', response_code = 'no_effect', http_status = 502 WHERE guild_id = $1").bind(GUILD).execute(pool).await.unwrap();
    let (result, logs) = tick(pool, RetentionMode::Execute).await;
    result.unwrap();
    field(report_line(&logs), "deletions_completed", 1);
    field(report_line(&logs), "deletions_deferred", 0);
    let receipt: (String, Option<String>, bool) = sqlx::query_as("SELECT status, receipt_id, completed_at IS NOT NULL FROM retention_deletion_requests WHERE request_id = $1").bind(&id).fetch_one(pool).await.unwrap();
    assert_eq!(receipt.0, "completed");
    assert!(receipt.1.is_some());
    assert!(receipt.2);
    let members: i64 = sqlx::query_scalar("SELECT count(*) FROM members WHERE guild_id = $1")
        .bind(GUILD)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(members, 0);
    db.close().await.unwrap();
}

#[tokio::test]
async fn runtime_surfaces_failed_guild_purge_and_retries_the_queued_work() {
    let _serial = RUNTIME_LOCK.lock().await;
    let Some(db) = database().await else { return };
    let pool = db.pool();
    sqlx::query("INSERT INTO members (guild_id, member_id) VALUES ($1, $2)")
        .bind(DEPARTED)
        .bind(MEMBER)
        .execute(pool)
        .await
        .unwrap();
    handle_guild_delete(pool, DEPARTED, None).await.unwrap();
    sqlx::raw_sql("CREATE FUNCTION retention_runtime_refuse() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'synthetic retention refusal'; END $$; CREATE TRIGGER retention_runtime_refuse BEFORE DELETE ON members FOR EACH ROW EXECUTE FUNCTION retention_runtime_refuse();").execute(pool).await.unwrap();
    let before = snapshot(pool).await;
    for _ in 0..2 {
        let (_, logs) = tick(pool, RetentionMode::Execute).await;
        let report = report_line(&logs);
        assert!(report.starts_with("WARN "));
        field(report, "guild_purges_failed", 1);
        field(report, "guild_rows_purged", 0);
        assert_eq!(
            snapshot(pool).await,
            before,
            "failed purge remains queued with intact data"
        );
    }
    sqlx::raw_sql("DROP TRIGGER retention_runtime_refuse ON members; DROP FUNCTION retention_runtime_refuse();").execute(pool).await.unwrap();
    let (result, logs) = tick(pool, RetentionMode::Execute).await;
    result.unwrap();
    field(report_line(&logs), "guild_purges_failed", 0);
    field(report_line(&logs), "guild_rows_purged", 1);
    let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM retention_guild_purge_queue")
        .fetch_one(pool)
        .await
        .unwrap();
    let members: i64 = sqlx::query_scalar("SELECT count(*) FROM members")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!((pending, members), (0, 0));
    db.close().await.unwrap();
}

#[tokio::test]
async fn runtime_database_error_preserves_s1_schema_guard_and_retries_after_repair() {
    let _serial = RUNTIME_LOCK.lock().await;
    let Some(db) = database().await else { return };
    let pool = db.pool();
    let id = seed(pool).await;
    sqlx::query("CREATE TABLE retention_uncovered_fixture (guild_id TEXT, member_id TEXT)")
        .execute(pool)
        .await
        .unwrap();
    let before = snapshot(pool).await;
    let (result, _) = tick(pool, RetentionMode::Execute).await;
    assert_eq!(
        result,
        Err(ErrorClass::Database),
        "schema coverage refusal must reach the supervisor"
    );
    assert_eq!(
        snapshot(pool).await,
        before,
        "whole transaction rolls back on schema refusal"
    );
    sqlx::query("DROP TABLE retention_uncovered_fixture")
        .execute(pool)
        .await
        .unwrap();
    let (result, logs) = tick(pool, RetentionMode::Execute).await;
    result.unwrap();
    field(report_line(&logs), "deletions_completed", 1);
    let complete: bool = sqlx::query_scalar("SELECT status = 'completed' AND receipt_id IS NOT NULL FROM retention_deletion_requests WHERE request_id = $1").bind(id).fetch_one(pool).await.unwrap();
    assert!(complete);
    db.close().await.unwrap();
}
