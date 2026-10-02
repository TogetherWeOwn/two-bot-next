use serde_json::{json, Value};
use tokio::sync::watch;
use two_bot_core::{advance_next_run_iso, get_scheduled, put_scheduled, ScheduledWrite};
use two_bot_testsupport::TestDatabase;

use super::*;

use crate::discord_test_common::{MockRest, ScriptedResponse};

const GUILD: &str = "3333";
const CHANNEL: &str = "4444";
const POST_PATH: &str = "/api/v10/channels/4444/messages";
/// 2026-09-21T…Z: any fixed instant works; the store compares ISO text.
const T0: u64 = 1_790_000_000_000;

fn executor(mock: &MockRest) -> ActionExecutor {
    crate::gateway::ensure_crypto_provider();
    ActionExecutor::with_proxy("synthetic-job-test-token".to_owned(), Some(mock.origin())).unwrap()
}

fn posted() -> ScriptedResponse {
    ScriptedResponse::json(200, json!({"id": "9001"}))
}

async fn fixture() -> Option<TestDatabase> {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI must supply the guarded test database"
        );
        eprintln!("SKIP scheduled ticker integration: TWO_TEST_DATABASE_URL is not set");
        return None;
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create migrated agent-testdb fixture"),
    )
}

async fn seed(pool: &PgPool, id: &str, due_at: u64, interval_seconds: Option<i64>) {
    let at = format_iso_ms(T0 - 3_600_000);
    let stored = put_scheduled(
        pool,
        &ScheduledWrite {
            id: id.to_owned(),
            guild_id: GUILD.to_owned(),
            channel_id: CHANNEL.to_owned(),
            body: "standup @everyone".to_owned(),
            next_run_at: format_iso_ms(due_at),
            interval_seconds,
            enabled: true,
            created_by: "5555".to_owned(),
            created_at: at.clone(),
            updated_by: "5555".to_owned(),
            updated_at: at,
        },
    )
    .await
    .expect("seed scheduled row");
    assert!(stored);
}

async fn row(pool: &PgPool, id: &str) -> ScheduledMessageRow {
    get_scheduled(pool, GUILD, id)
        .await
        .expect("read scheduled row")
        .expect("row exists")
}

async fn audits(pool: &PgPool, id: &str) -> Vec<(String, Option<String>)> {
    sqlx::query_as(
        "SELECT outcome, reason FROM automation_audit_log
          WHERE guild_id = $1 AND target_key = $2 AND action = 'scheduled.run'
          ORDER BY created_at, id",
    )
    .bind(GUILD)
    .bind(id)
    .fetch_all(pool)
    .await
    .expect("read audit rows")
}

fn audit(outcome: &str, reason: Option<&str>) -> (String, Option<String>) {
    (outcome.to_owned(), reason.map(str::to_owned))
}

fn posts(mock: &MockRest) -> Vec<Value> {
    mock.requests()
        .into_iter()
        .filter(|request| request.method == "POST" && request.path == POST_PATH)
        .map(|request| serde_json::from_slice(&request.body).expect("JSON post body"))
        .collect()
}

#[test]
fn the_job_runs_every_fifteen_seconds_with_bounded_jitter() {
    let job = job(Arc::new(|| Box::pin(async { Ok(()) })));
    assert_eq!(job.name, "scheduled_messages");
    assert_eq!(job.cadence, Duration::from_secs(15));
    assert!(job.startup_jitter <= Duration::from_secs(5));
    assert!(job.timeout > Duration::from_secs(10 * 5 * 2));
    assert_eq!(random_nonce().len(), 24, "inside Discord's 25-char ceiling");
}

#[tokio::test]
async fn due_one_shot_posts_once_with_legacy_mentions_and_leaves_the_due_set() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    seed(pool, "once", T0 - 1_000, None).await;
    seed(pool, "later", T0 + 60_000, None).await;
    let mock = MockRest::start(vec![posted()], ScriptedResponse::status(500)).await;
    let rest = executor(&mock);

    tick(pool, &rest, GUILD, || T0).await.expect("tick");

    let sent = posts(&mock);
    assert_eq!(sent.len(), 1, "only the due row posts");
    // The shared send path defuses @everyone in the text as well as sending
    // `allowed_mentions.parse = []`.
    assert_eq!(sent[0]["content"], "standup @\u{200b}everyone");
    assert_eq!(sent[0]["allowed_mentions"], json!({"parse": []}));
    assert_eq!(sent[0]["enforce_nonce"], true);
    assert_eq!(sent[0]["nonce"].as_str().map(str::len), Some(24));
    let done = row(pool, "once").await;
    assert!(!done.enabled, "a one-shot leaves the due set");
    assert_eq!(done.last_run_at.as_deref(), Some(&*format_iso_ms(T0)));
    assert_eq!(done.last_message_id.as_deref(), Some("9001"));
    assert_eq!(done.claim_token, None);
    assert_eq!(done.occurrence_nonce, None);
    assert_eq!(audits(pool, "once").await, [audit("ok", None)]);

    // Long after the lease, the disabled one-shot never posts again.
    tick(pool, &rest, GUILD, || T0 + 30_000)
        .await
        .expect("tick");
    tick(pool, &rest, GUILD, || T0 + 50_000)
        .await
        .expect("tick");
    assert_eq!(posts(&mock).len(), 1);
    assert!(row(pool, "later").await.enabled, "not yet due");
    mock.shutdown().await;
    fixture.close().await.unwrap();
}

#[tokio::test]
async fn recurring_row_advances_next_run_at_from_the_run() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    seed(pool, "daily", T0 - 1_000, Some(86_400)).await;
    let mock = MockRest::start(vec![posted(), posted()], ScriptedResponse::status(500)).await;
    let rest = executor(&mock);

    tick(pool, &rest, GUILD, || T0).await.expect("tick");

    let advanced = row(pool, "daily").await;
    assert!(advanced.enabled);
    assert_eq!(
        advanced.next_run_at,
        advance_next_run_iso(&format_iso_ms(T0), 86_400).unwrap()
    );
    assert_eq!(advanced.claim_token, None);
    assert_eq!(
        advanced.occurrence_nonce, None,
        "the next occurrence gets a fresh nonce"
    );
    tick(pool, &rest, GUILD, || T0 + 3_600_000)
        .await
        .expect("tick");
    assert_eq!(posts(&mock).len(), 1, "not due again for a day");

    let next = parse_iso(&advanced.next_run_at);
    tick(pool, &rest, GUILD, || next).await.expect("tick");
    let sent = posts(&mock);
    assert_eq!(sent.len(), 2);
    assert_ne!(
        sent[0]["nonce"], sent[1]["nonce"],
        "each occurrence is distinct"
    );
    assert_eq!(
        audits(pool, "daily").await,
        [audit("ok", None), audit("ok", None)]
    );
    mock.shutdown().await;
    fixture.close().await.unwrap();
}

fn parse_iso(iso: &str) -> u64 {
    two_bot_core::parse_iso_ms(iso).expect("ISO timestamp")
}

#[tokio::test]
async fn retryable_failures_requeue_with_the_clamped_delay_and_keep_the_nonce() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    seed(pool, "flaky", T0 - 1_000, None).await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(503),
            ScriptedResponse::status(429),
            posted(),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let rest = executor(&mock);
    let delay = clamp_retry_delay_ms(None);
    assert_eq!(delay, two_bot_core::RETRY_DEFAULT_MS);

    tick(pool, &rest, GUILD, || T0).await.expect("tick");
    let requeued = row(pool, "flaky").await;
    assert!(requeued.enabled);
    assert_eq!(requeued.next_run_at, format_iso_ms(T0 + delay));
    assert_eq!(requeued.claim_token, None);
    let nonce = requeued.occurrence_nonce.clone().expect("nonce kept");

    // Not due until the clamped delay passes.
    tick(pool, &rest, GUILD, || T0 + delay - 1)
        .await
        .expect("tick");
    assert_eq!(posts(&mock).len(), 1);

    tick(pool, &rest, GUILD, || T0 + delay).await.expect("tick");
    assert_eq!(
        row(pool, "flaky").await.next_run_at,
        format_iso_ms(T0 + 2 * delay)
    );
    tick(pool, &rest, GUILD, || T0 + 2 * delay)
        .await
        .expect("tick");

    let sent = posts(&mock);
    assert_eq!(sent.len(), 3);
    assert!(
        sent.iter().all(|body| body["nonce"] == nonce.as_str()),
        "every retry of one occurrence carries its nonce"
    );
    assert!(!row(pool, "flaky").await.enabled);
    assert_eq!(
        audits(pool, "flaky").await,
        [
            audit("retry_scheduled", Some("discord_unavailable")),
            audit("retry_scheduled", Some("discord_rate_limited")),
            audit("ok", None),
        ]
    );
    mock.shutdown().await;
    fixture.close().await.unwrap();
}

#[tokio::test]
async fn non_retryable_failure_completes_the_occurrence_and_is_audited() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    seed(pool, "denied", T0 - 2_000, None).await;
    seed(pool, "hourly", T0 - 1_000, Some(3_600)).await;
    let mock = MockRest::start(
        vec![ScriptedResponse::status(403), ScriptedResponse::status(404)],
        ScriptedResponse::status(500),
    )
    .await;
    let rest = executor(&mock);

    tick(pool, &rest, GUILD, || T0).await.expect("tick");

    assert_eq!(posts(&mock).len(), 2, "one refusal does not stop the batch");
    let denied = row(pool, "denied").await;
    assert!(!denied.enabled, "a refused one-shot completes");
    assert_eq!(denied.last_message_id, None);
    let hourly = row(pool, "hourly").await;
    assert!(hourly.enabled);
    assert_eq!(
        hourly.next_run_at,
        advance_next_run_iso(&format_iso_ms(T0), 3_600).unwrap(),
        "a refused recurring row advances instead of wedging the queue"
    );
    for id in ["denied", "hourly"] {
        assert_eq!(
            audits(pool, id).await,
            [audit("post_failed", Some("discord_rejected"))]
        );
    }
    tick(pool, &rest, GUILD, || T0 + 120_000)
        .await
        .expect("tick");
    assert_eq!(posts(&mock).len(), 2);
    mock.shutdown().await;
    fixture.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_concurrent_tickers_post_a_row_once() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    for id in ["a", "b", "c"] {
        seed(pool, id, T0 - 1_000, None).await;
    }
    let mock = MockRest::start(Vec::new(), posted()).await;
    let rest = executor(&mock);

    let (first, second) = tokio::join!(
        tick(pool, &rest, GUILD, || T0),
        tick(pool, &rest, GUILD, || T0),
    );
    first.expect("first ticker");
    second.expect("second ticker");

    assert_eq!(posts(&mock).len(), 3, "each due row posts exactly once");
    for id in ["a", "b", "c"] {
        assert!(!row(pool, id).await.enabled);
        assert_eq!(audits(pool, id).await, [audit("ok", None)]);
    }
    mock.shutdown().await;
    fixture.close().await.unwrap();
}

#[tokio::test]
async fn restart_during_a_lease_waits_for_expiry_and_reuses_the_nonce() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    seed(pool, "leased", T0 - 1_000, None).await;
    // A previous process claimed the row and died before recording the run.
    let claimed = claim_due(
        pool,
        GUILD,
        &format_iso_ms(T0),
        "dead-process-claim",
        &format_iso_ms(lease_until_ms(T0)),
        "deadbeefdeadbeefdeadbeef",
    )
    .await
    .expect("pre-claim");
    assert_eq!(claimed.len(), 1);
    let mock = MockRest::start(Vec::new(), posted()).await;
    let rest = executor(&mock);

    tick(pool, &rest, GUILD, || T0 + 1_000).await.expect("tick");
    tick(pool, &rest, GUILD, || lease_until_ms(T0) - 1)
        .await
        .expect("tick");
    assert!(
        mock.requests().is_empty(),
        "a live lease is never re-posted"
    );

    tick(pool, &rest, GUILD, || lease_until_ms(T0))
        .await
        .expect("tick");
    let sent = posts(&mock);
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0]["nonce"], "deadbeefdeadbeefdeadbeef",
        "the re-run dedupes against a post the dead process may have made"
    );
    assert!(!row(pool, "leased").await.enabled);
    assert_eq!(audits(pool, "leased").await, [audit("ok", None)]);
    mock.shutdown().await;
    fixture.close().await.unwrap();
}

#[tokio::test]
async fn definition_replaced_mid_post_deletes_the_orphan() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool().clone();
    seed(&pool, "edited", T0 - 1_000, None).await;
    let (mock, gate) = MockRest::start_gated(vec![posted()], ScriptedResponse::status(204)).await;
    let rest = executor(&mock);

    let ticker = {
        let (pool, rest) = (pool.clone(), rest.clone());
        tokio::spawn(async move { tick(&pool, &rest, GUILD, || T0).await })
    };
    gate.wait_for_request().await;
    seed(&pool, "edited", T0 + 600_000, None).await;
    gate.release();
    ticker.await.unwrap().expect("tick");

    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].method, "DELETE");
    assert_eq!(requests[1].path, "/api/v10/channels/4444/messages/9001");
    let replaced = row(&pool, "edited").await;
    assert!(replaced.enabled, "the new definition stands");
    assert_eq!(replaced.next_run_at, format_iso_ms(T0 + 600_000));
    assert_eq!(
        audits(&pool, "edited").await,
        [audit("stale_completion", None)]
    );
    mock.shutdown().await;
    fixture.close().await.unwrap();
}

#[tokio::test]
async fn supervised_runs_reach_the_metrics_endpoint() {
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use tower::ServiceExt as _;

    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool().clone();
    seed(&pool, "metered", now_ms() - 1_000, None).await;
    let mock = MockRest::start(Vec::new(), posted()).await;
    let rest = executor(&mock);
    let action: JobAction = {
        let (pool, rest) = (pool.clone(), rest.clone());
        Arc::new(move || {
            let (pool, rest) = (pool.clone(), rest.clone());
            Box::pin(async move { tick(&pool, &rest, GUILD, now_ms).await })
        })
    };
    let status = jobs::statuses(&NAMES, false);
    let (stop, shutdown) = watch::channel(false);
    let supervisor = tokio::spawn(jobs::supervise(
        vec![Job {
            startup_jitter: Duration::ZERO,
            ..job(action)
        }],
        status.clone(),
        shutdown,
    ));
    tokio::time::timeout(Duration::from_secs(30), async {
        while status.read().await[NAMES[0]].last_success.is_none() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("first supervised tick succeeds");
    stop.send(true).unwrap();
    supervisor.await.unwrap();
    assert_eq!(posts(&mock).len(), 1);

    let response = crate::metrics_http::router()
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let text = String::from_utf8(
        to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    let successes: u64 = text
        .lines()
        .find_map(|line| {
            line.strip_prefix(
                "two_bot_job_runs_total{job=\"scheduled_messages\",outcome=\"success\"} ",
            )
        })
        .expect("scheduled_messages is a named job label, not `other`")
        .parse()
        .unwrap();
    assert!(successes >= 1);
    assert!(
        text.contains("two_bot_job_last_success_timestamp_seconds{job=\"scheduled_messages\"} ")
    );
    mock.shutdown().await;
    fixture.close().await.unwrap();
}
