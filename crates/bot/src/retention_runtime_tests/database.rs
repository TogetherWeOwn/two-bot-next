#![cfg(test)]

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

fn expiry_line(logs: &str) -> &str {
    let events: Vec<_> = logs
        .lines()
        .filter(|line| line.contains("game_activity_aggregate_expiry"))
        .collect();
    assert_eq!(
        events.len(),
        1,
        "one aggregate expiry event per sweep with stale rows: {logs}"
    );
    events[0]
}

#[tokio::test]
async fn game_aggregate_expiry_counts_in_dry_run_and_deletes_in_execute() {
    let _serial = RUNTIME_LOCK.lock().await;
    let Some(db) = database().await else { return };
    let pool = db.pool();
    const AGGREGATE_GUILD: &str = "444444444444444444";
    sqlx::query("INSERT INTO game_activity_guild_aggregates (guild_id, game, total_play_ms, in_voice_ms, session_count) VALUES ($1, 'Apex', 600000, 0, 2), ($1, 'Valorant', 600000, 0, 1)")
        .bind(AGGREGATE_GUILD).execute(pool).await.unwrap();
    sqlx::query("UPDATE game_activity_guild_aggregates SET updated_at = now() - make_interval(days => 100) WHERE guild_id = $1 AND game = 'Apex'")
        .bind(AGGREGATE_GUILD).execute(pool).await.unwrap();
    // Dry run previews the same stale row execute will delete, and deletes
    // nothing: both rows survive.
    let (result, logs) = tick(pool, mode_from_setting(None).unwrap().unwrap()).await;
    result.unwrap();
    let preview = expiry_line(&logs);
    field(preview, "stale_rows", 1);
    field(preview, "horizon_days", 90);
    assert!(
        preview.contains("mode=DryRun") || preview.contains("mode=\"DryRun\""),
        "dry run must tag its preview: {preview}"
    );
    let kept: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM game_activity_guild_aggregates WHERE guild_id = $1",
    )
    .bind(AGGREGATE_GUILD)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(kept, 2, "dry run deletes nothing");
    // Execute deletes the stale row and keeps the fresh one.
    let (result, logs) = tick(pool, RetentionMode::Execute).await;
    result.unwrap();
    let swept = expiry_line(&logs);
    field(swept, "stale_rows", 1);
    assert!(
        swept.contains("mode=Execute") || swept.contains("mode=\"Execute\""),
        "execute must tag its sweep: {swept}"
    );
    let games: Vec<String> = sqlx::query_scalar(
        "SELECT game FROM game_activity_guild_aggregates WHERE guild_id = $1 ORDER BY game ASC",
    )
    .bind(AGGREGATE_GUILD)
    .fetch_all(pool)
    .await
    .unwrap();
    assert_eq!(games, vec!["Valorant".to_owned()]);
    db.close().await.unwrap();
}

fn field_str(line: &str, name: &str, expected: &str) {
    let plain = format!("{name}={expected}");
    let quoted = format!("{name}=\"{expected}\"");
    assert!(
        line.split_whitespace()
            .any(|part| part == plain || part == quoted),
        "missing exact report field {plain}: {line}"
    );
}

fn expiry_failure_line(logs: &str) -> &str {
    let failures: Vec<_> = logs
        .lines()
        .filter(|line| line.contains("game_activity_aggregate_expiry_failed"))
        .collect();
    assert_eq!(
        failures.len(),
        1,
        "one aggregate expiry failure event per failed tick: {logs}"
    );
    failures[0]
}

/// The classified token for an injected database error (`RAISE EXCEPTION`
/// or a missing table both classify as `database`) under the house `reason`
/// field. The raw database error text itself must never appear in the log
/// line.
fn assert_classified_db_error(line: &str) {
    let classified = ["reason=database", "reason=\"database\""];
    assert!(
        line.split_whitespace()
            .any(|part| classified.contains(&part)),
        "failure line must carry reason=database, not discard it: {line}"
    );
}

/// A tick whose aggregate expiry failed must not leave behind the plain
/// success line: the committed report still prints once, with distinct text
/// (`retention run committed; aggregate expiry failed`), so the counters are
/// never dropped.
fn assert_no_completed_line(logs: &str) {
    assert!(
        !logs
            .lines()
            .any(|line| line.trim_end().ends_with("retention run completed")),
        "failed tick must not log a plain completed line: {logs}"
    );
}

#[tokio::test]
async fn game_aggregate_expiry_denied_delete_identifies_guild_horizon_mode_and_cause() {
    let _serial = RUNTIME_LOCK.lock().await;
    let Some(db) = database().await else { return };
    let pool = db.pool();
    const DENIED_GUILD: &str = "555555555555555555";
    sqlx::query("INSERT INTO game_activity_guild_aggregates (guild_id, game, total_play_ms, in_voice_ms, session_count) VALUES ($1, 'Apex', 600000, 0, 2)")
        .bind(DENIED_GUILD).execute(pool).await.unwrap();
    sqlx::query("UPDATE game_activity_guild_aggregates SET updated_at = now() - make_interval(days => 100) WHERE guild_id = $1")
        .bind(DENIED_GUILD).execute(pool).await.unwrap();
    sqlx::raw_sql("CREATE FUNCTION aggregate_expiry_refuse() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'synthetic aggregate expiry refusal'; END $$; CREATE TRIGGER aggregate_expiry_refuse BEFORE DELETE ON game_activity_guild_aggregates FOR EACH ROW EXECUTE FUNCTION aggregate_expiry_refuse();").execute(pool).await.unwrap();
    let (result, logs) = tick(pool, RetentionMode::Execute).await;
    assert_eq!(
        result,
        Err(ErrorClass::Database),
        "a denied aggregate delete stays a retryable database failure: {logs}"
    );
    let failure = expiry_failure_line(&logs);
    assert!(
        failure.starts_with("WARN ") || failure.starts_with("ERROR "),
        "expiry failure must log above info level: {failure}"
    );
    field_str(failure, "guild_id", DENIED_GUILD);
    field(failure, "horizon_days", 90);
    assert!(
        failure.contains("mode=Execute") || failure.contains("mode=\"Execute\""),
        "failure line must tag the mode that attempted the delete: {failure}"
    );
    assert_classified_db_error(failure);
    assert!(
        !failure.contains("synthetic aggregate expiry refusal"),
        "failure line must classify the database error, never log it raw: {failure}"
    );
    assert_no_completed_line(&logs);
    let committed = report_line(&logs);
    assert!(
        committed.contains("mode=Execute") || committed.contains("mode=\"Execute\""),
        "committed report must tag the mode on a failed expiry: {committed}"
    );
    assert!(
        committed.contains("aggregate expiry failed"),
        "committed report must use distinct text, not the plain success line: {committed}"
    );
    let kept: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM game_activity_guild_aggregates WHERE guild_id = $1",
    )
    .bind(DENIED_GUILD)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(kept, 1, "a denied delete removes nothing");
    // Once the refusal is gone the same stale row sweeps on the next tick.
    sqlx::raw_sql("DROP TRIGGER aggregate_expiry_refuse ON game_activity_guild_aggregates; DROP FUNCTION aggregate_expiry_refuse();").execute(pool).await.unwrap();
    let (result, logs) = tick(pool, RetentionMode::Execute).await;
    result.unwrap();
    field(expiry_line(&logs), "stale_rows", 1);
    let kept: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM game_activity_guild_aggregates WHERE guild_id = $1",
    )
    .bind(DENIED_GUILD)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(kept, 0, "the settled tick deletes the stale row");
    db.close().await.unwrap();
}

#[tokio::test]
async fn game_aggregate_expiry_listing_failure_identifies_horizon_mode_and_cause() {
    let _serial = RUNTIME_LOCK.lock().await;
    let Some(db) = database().await else { return };
    let pool = db.pool();
    // Hiding the aggregate table fails the stale-guild listing itself,
    // before any guild is known: the failure line still owes the horizon,
    // the mode and the classified cause.
    sqlx::query("ALTER TABLE game_activity_guild_aggregates RENAME TO game_activity_guild_aggregates_hidden")
        .execute(pool).await.unwrap();
    let (result, logs) = tick(pool, mode_from_setting(None).unwrap().unwrap()).await;
    assert_eq!(
        result,
        Err(ErrorClass::Database),
        "a failed stale-guild listing stays a retryable database failure: {logs}"
    );
    let failure = expiry_failure_line(&logs);
    field(failure, "horizon_days", 90);
    assert!(
        failure.contains("mode=DryRun") || failure.contains("mode=\"DryRun\""),
        "listing failure must tag the mode that attempted the sweep: {failure}"
    );
    assert_classified_db_error(failure);
    assert!(
        !failure.contains("does not exist"),
        "failure line must classify the database error, never log it raw: {failure}"
    );
    assert_no_completed_line(&logs);
    let committed = report_line(&logs);
    assert!(
        committed.contains("mode=DryRun") || committed.contains("mode=\"DryRun\""),
        "committed report must tag the mode on a failed listing: {committed}"
    );
    assert!(
        committed.contains("aggregate expiry failed"),
        "committed report must use distinct text, not the plain success line: {committed}"
    );
    sqlx::query("ALTER TABLE game_activity_guild_aggregates_hidden RENAME TO game_activity_guild_aggregates")
        .execute(pool).await.unwrap();
    let (result, _logs) = tick(pool, mode_from_setting(None).unwrap().unwrap()).await;
    result.unwrap();
    db.close().await.unwrap();
}

#[tokio::test]
async fn game_aggregate_expiry_zero_stale_ticks_emit_a_swept_clean_event() {
    let _serial = RUNTIME_LOCK.lock().await;
    let Some(db) = database().await else { return };
    let pool = db.pool();
    const FRESH_GUILD: &str = "666666666666666666";
    sqlx::query("INSERT INTO game_activity_guild_aggregates (guild_id, game, total_play_ms, in_voice_ms, session_count) VALUES ($1, 'Valorant', 600000, 0, 1)")
        .bind(FRESH_GUILD).execute(pool).await.unwrap();
    // A tick that ran and found nothing stale must say so in both modes:
    // `stale_rows=0` on the sweep event is what separates "swept clean"
    // from "expiry never ran".
    let (result, logs) = tick(pool, mode_from_setting(None).unwrap().unwrap()).await;
    result.unwrap();
    let preview = expiry_line(&logs);
    field(preview, "stale_rows", 0);
    field(preview, "horizon_days", 90);
    assert!(
        preview.contains("mode=DryRun") || preview.contains("mode=\"DryRun\""),
        "zero-stale dry run must tag its preview: {preview}"
    );
    assert!(
        !preview.contains("skip"),
        "zero-stale event must read as a sweep, not a skip: {preview}"
    );
    let (result, logs) = tick(pool, RetentionMode::Execute).await;
    result.unwrap();
    let swept = expiry_line(&logs);
    field(swept, "stale_rows", 0);
    field(swept, "horizon_days", 90);
    assert!(
        swept.contains("mode=Execute") || swept.contains("mode=\"Execute\""),
        "zero-stale execute must tag its sweep: {swept}"
    );
    let games: Vec<String> = sqlx::query_scalar(
        "SELECT game FROM game_activity_guild_aggregates WHERE guild_id = $1 ORDER BY game ASC",
    )
    .bind(FRESH_GUILD)
    .fetch_all(pool)
    .await
    .unwrap();
    assert_eq!(games, vec!["Valorant".to_owned()], "fresh rows survive");
    db.close().await.unwrap();
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
async fn runtime_schema_gap_defers_deletions_and_guild_purges_and_retries_after_repair() {
    // S1 schema guard stays fail-closed per request (no receipt while gapped)
    // but no longer aborts the run: time-window deletes commit, the gapped
    // request defers as a partial Ok with a distinct schema-gap counter, and
    // every queued guild stays queued until the plan is repaired.
    let _serial = RUNTIME_LOCK.lock().await;
    let Some(db) = database().await else { return };
    let pool = db.pool();
    let id = seed(pool).await;
    sqlx::query("CREATE TABLE retention_uncovered_fixture (guild_id TEXT, member_id TEXT)")
        .execute(pool)
        .await
        .unwrap();
    let (result, logs) = tick(pool, RetentionMode::Execute).await;
    result.unwrap();
    let report = report_line(&logs);
    assert!(
        report.starts_with("WARN "),
        "schema-gap deferral must not be a silent success: {report}"
    );
    field(report, "deletions_deferred", 1);
    field(report, "schema_gap_deferrals", 1);
    field(report, "deletions_completed", 0);
    field(report, "presence_raw_deleted", 1);
    field(report, "voice_raw_deleted", 1);
    field(report, "audit_cache_deleted", 1);
    field(report, "guild_rows_purged", 0);
    field(report, "guild_purges_failed", 1);
    let pending: bool = sqlx::query_scalar("SELECT status = 'pending' AND receipt_id IS NULL FROM retention_deletion_requests WHERE request_id = $1").bind(&id).fetch_one(pool).await.unwrap();
    assert!(pending, "gapped request stays pending with no receipt");
    let presence_left: i64 = sqlx::query_scalar("SELECT count(*) FROM presence_probe")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(
        presence_left, 0,
        "time-window deletes must commit despite the schema gap"
    );
    let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM retention_guild_purge_queue")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(
        queued, 1,
        "queued guild must stay queued while the schema gap exists"
    );
    sqlx::query("DROP TABLE retention_uncovered_fixture")
        .execute(pool)
        .await
        .unwrap();
    let (result, logs) = tick(pool, RetentionMode::Execute).await;
    result.unwrap();
    let settled = report_line(&logs);
    field(settled, "deletions_completed", 1);
    field(settled, "schema_gap_deferrals", 0);
    field(settled, "guild_purges_failed", 0);
    let complete: bool = sqlx::query_scalar("SELECT status = 'completed' AND receipt_id IS NOT NULL FROM retention_deletion_requests WHERE request_id = $1").bind(id).fetch_one(pool).await.unwrap();
    assert!(complete);
    let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM retention_guild_purge_queue")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(queued, 0, "repaired run must purge the deferred guild");
    db.close().await.unwrap();
}
