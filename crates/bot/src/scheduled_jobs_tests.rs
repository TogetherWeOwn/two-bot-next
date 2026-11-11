#[path = "event_reminder_acceptance_tests.rs"]
mod event_reminder_acceptance;
#[path = "event_reminder_deadline_tests.rs"]
mod event_reminder_deadline;
#[path = "event_reminder_regression_tests.rs"]
mod event_reminder_regression;
#[path = "scheduled_phase_isolation_acceptance_tests.rs"]
mod phase_isolation_acceptance;
#[path = "scheduled_uncertainty_acceptance_tests.rs"]
mod uncertainty_acceptance;

use serde_json::{json, Value};
use tokio::sync::watch;
use two_bot_core::{advance_next_run_iso, get_scheduled, put_scheduled, ScheduledWrite};
use two_bot_testsupport::TestDatabase;

use super::*;

use crate::{
    activation::fixtures,
    discord_test_common::{MockRest, ScriptedResponse},
};

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

fn gates(automations: Option<&str>) -> FeatureGates {
    let mut vars = std::collections::HashMap::new();
    if let Some(value) = automations {
        vars.insert("TWO_AUTOMATIONS".to_owned(), value.to_owned());
    }
    FeatureGates::from_map(&vars).expect("gates parse")
}

/// Legacy `startScheduler(.., { enabled: false })` (commit 2f386e8): a disabled
/// scheduler executes zero jobs. Here that means no supervised job exists, so
/// nothing can ever call the ticker. Only the exact value `1` builds one, and
/// building is lazy.
#[tokio::test]
async fn registration_builds_no_job_while_automations_are_off() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let calls = Arc::new(AtomicUsize::new(0));
    let action: JobAction = Arc::new({
        let calls = calls.clone();
        move || {
            calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    });
    for value in [
        None,
        Some(""),
        Some("0"),
        Some("true"),
        Some("yes"),
        Some(" 1"),
    ] {
        assert!(
            register_gated(gates(value), action.clone()).is_none(),
            "TWO_AUTOMATIONS={value:?} must not register the ticker"
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let job = register_gated(gates(Some("1")), action).expect("enabled gate registers");
    assert_eq!(job.name, "scheduled_messages");
    assert_eq!(job.cadence, Duration::from_secs(15));
    assert_eq!(calls.load(Ordering::SeqCst), 0, "registration is lazy");
    (job.action)().await.expect("action runs");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// TOG-15758: the ticker posts under the token's identity, so the capability
/// fence binds it like the `/schedule` verbs. With `TWO_AUTOMATIONS=1` the
/// staging pair registers; the live pair (automations uncleared), an unknown
/// guild, a mismatched pair and a missing or unparseable token build no job.
#[tokio::test]
async fn identity_fence_registers_the_ticker_only_where_automations_are_permitted() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let calls = Arc::new(AtomicUsize::new(0));
    let action: JobAction = Arc::new({
        let calls = calls.clone();
        move || {
            calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    });
    let on = gates(Some("1"));

    let job = register_fenced(on, &fixtures::staging(), action.clone())
        .expect("staging identity registers the ticker");
    assert_eq!(job.name, "scheduled_messages");
    (job.action)().await.expect("staging action runs");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    for (label, activation) in fixtures::refused() {
        assert!(
            register_fenced(on, &activation, action.clone()).is_none(),
            "{label} must not register the ticker"
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1, "refused jobs never run");

    // Identity never enables what the environment left off.
    for value in [None, Some("0")] {
        assert!(register_fenced(gates(value), &fixtures::staging(), action.clone()).is_none());
    }
}

/// The same legacy row against a database with a due row: the disabled
/// registration leaves the row unclaimed, unposted and unaudited, while the
/// enabled control with the same action posts it once.
#[tokio::test]
async fn disabled_scheduler_leaves_a_due_row_untouched_and_the_enabled_control_posts_it() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool().clone();
    seed(&pool, "due", T0 - 1_000, None).await;
    let before = row(&pool, "due").await;
    let mock = MockRest::start(vec![posted()], ScriptedResponse::status(500)).await;
    let rest = executor(&mock);
    let action: JobAction = {
        let (pool, rest) = (pool.clone(), rest.clone());
        Arc::new(move || {
            let (pool, rest) = (pool.clone(), rest.clone());
            Box::pin(async move { tick(&pool, &rest, GUILD, || T0).await })
        })
    };

    assert!(register_gated(gates(None), action.clone()).is_none());
    assert!(register_gated(gates(Some("0")), action.clone()).is_none());
    assert!(
        posts(&mock).is_empty(),
        "a disabled scheduler posts nothing"
    );
    let untouched = row(&pool, "due").await;
    assert_eq!(untouched.next_run_at, before.next_run_at);
    assert!(untouched.enabled);
    assert_eq!(untouched.claim_token, None);
    assert_eq!(untouched.last_run_at, None);
    assert!(audits(&pool, "due").await.is_empty());

    let job = register_gated(gates(Some("1")), action).expect("enabled control registers");
    (job.action)().await.expect("enabled tick");
    assert_eq!(posts(&mock).len(), 1, "the control posts the due row once");
    assert_eq!(audits(&pool, "due").await, [audit("ok", None)]);
    mock.shutdown().await;
    fixture.close().await.unwrap();
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
    // The shared send path defuses @everyone in the text and sends the
    // legacy-closed allowed_mentions (nothing parsed, no roles/users/reply ping).
    assert_eq!(sent[0]["content"], "standup @\u{200b}everyone");
    assert_eq!(
        sent[0]["allowed_mentions"],
        json!({"parse": [], "roles": [], "users": [], "replied_user": false})
    );
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
async fn ambiguous_503_arms_durable_uncertainty_and_never_reposts() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    seed(pool, "flaky", T0 - 1_000, None).await;
    let mock = MockRest::start(
        vec![ScriptedResponse::status(503)],
        ScriptedResponse::status(500),
    )
    .await;
    let rest = executor(&mock);
    let delay = clamp_retry_delay_ms(None);
    assert_eq!(delay, two_bot_core::RETRY_DEFAULT_MS);

    tick(pool, &rest, GUILD, || T0).await.expect("tick");
    let fenced = row(pool, "flaky").await;
    assert!(fenced.enabled, "uncertainty never completes the occurrence");
    assert_eq!(fenced.next_run_at, format_iso_ms(T0 + delay));
    assert_eq!(fenced.claim_token, None);
    let nonce = fenced.occurrence_nonce.clone().expect("nonce kept");
    assert!(
        fenced.delivery_uncertain,
        "the pre-wire fence survives the ambiguous 503"
    );
    assert_eq!(fenced.last_run_at, None);
    assert_eq!(fenced.last_message_id, None);
    assert_eq!(
        audits(pool, "flaky").await,
        [audit("delivery_uncertain", Some("discord_unavailable"))],
    );

    // Not due until the clamped delay passes.
    tick(pool, &rest, GUILD, || T0 + delay - 1)
        .await
        .expect("tick");
    assert_eq!(posts(&mock).len(), 1);
    assert_eq!(audits(pool, "flaky").await.len(), 1);

    // At the retry horizon the fenced occurrence is held, never re-posted.
    tick(pool, &rest, GUILD, || T0 + delay).await.expect("tick");
    assert_eq!(posts(&mock).len(), 1, "a fenced row never POSTs again");
    let held = row(pool, "flaky").await;
    assert!(held.enabled);
    assert!(held.claim_token.is_some(), "the held claim stays parked");
    assert_eq!(held.occurrence_nonce.as_deref(), Some(nonce.as_str()));
    assert!(held.delivery_uncertain);
    assert_eq!(
        audits(pool, "flaky").await,
        [
            audit("delivery_uncertain", Some("discord_unavailable")),
            audit("delivery_uncertain", Some("scheduled_delivery_uncertain")),
        ]
    );

    // Past the lease the reclaim still holds without posting.
    let lease = lease_until_ms(T0 + delay);
    tick(pool, &rest, GUILD, || lease).await.expect("tick");
    assert_eq!(posts(&mock).len(), 1);
    assert!(row(pool, "flaky").await.delivery_uncertain);
    assert_eq!(
        audits(pool, "flaky").await.len(),
        3,
        "each reclaim audits the hold"
    );
    mock.shutdown().await;
    fixture.close().await.unwrap();
}

#[tokio::test]
async fn proven_429_retries_without_a_fence_and_completes() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    seed(pool, "limited", T0 - 1_000, None).await;
    let mock = MockRest::start(
        vec![ScriptedResponse::status(429), posted()],
        ScriptedResponse::status(500),
    )
    .await;
    let rest = executor(&mock);
    let delay = clamp_retry_delay_ms(None);

    tick(pool, &rest, GUILD, || T0).await.expect("tick");
    let requeued = row(pool, "limited").await;
    assert!(requeued.enabled);
    assert_eq!(requeued.next_run_at, format_iso_ms(T0 + delay));
    assert_eq!(requeued.claim_token, None);
    assert_eq!(
        requeued.occurrence_nonce, None,
        "a proven no-effect clears the nonce for a fresh identity"
    );
    assert!(
        !requeued.delivery_uncertain,
        "a 429 never arms durable uncertainty"
    );
    assert_eq!(
        audits(pool, "limited").await,
        [audit("retry_scheduled", Some("discord_rate_limited"))],
    );
    assert_eq!(posts(&mock).len(), 1);
    let first_nonce = posts(&mock)[0]["nonce"].clone();

    tick(pool, &rest, GUILD, || T0 + delay - 1)
        .await
        .expect("tick");
    assert_eq!(posts(&mock).len(), 1);

    tick(pool, &rest, GUILD, || T0 + delay).await.expect("tick");
    let sent = posts(&mock);
    assert_eq!(sent.len(), 2);
    assert_ne!(
        sent[1]["nonce"], first_nonce,
        "the safe retry uses a fresh nonce"
    );
    let done = row(pool, "limited").await;
    assert!(!done.enabled);
    assert_eq!(done.last_message_id.as_deref(), Some("9001"));
    assert!(!done.delivery_uncertain);
    assert_eq!(
        audits(pool, "limited").await,
        [
            audit("retry_scheduled", Some("discord_rate_limited")),
            audit("ok", None),
        ]
    );
    mock.shutdown().await;
    fixture.close().await.unwrap();
}

#[tokio::test]
async fn guard_refusal_retries_without_a_fence_and_completes() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    seed(pool, "guarded", T0 - 1_000, None).await;
    let mock = MockRest::start(vec![posted()], ScriptedResponse::status(500)).await;
    crate::gateway::ensure_crypto_provider();
    let guard = std::sync::Arc::new(
        two_bot_discord::ratelimit_guard::RateLimitGuard::new(Default::default()).unwrap(),
    );
    guard.observe_status(401, true);
    let refused = two_bot_discord::executor::ActionExecutor::with_proxy_and_guard(
        "synthetic-job-test-token".to_owned(),
        Some(mock.origin()),
        guard,
    )
    .unwrap();
    let delay = clamp_retry_delay_ms(None);

    tick(pool, &refused, GUILD, || T0).await.expect("tick");
    assert!(
        posts(&mock).is_empty(),
        "a guard refusal never reaches the wire"
    );
    let requeued = row(pool, "guarded").await;
    assert!(requeued.enabled);
    assert_eq!(requeued.next_run_at, format_iso_ms(T0 + delay));
    assert_eq!(requeued.claim_token, None);
    assert_eq!(requeued.occurrence_nonce, None);
    assert!(!requeued.delivery_uncertain);
    assert_eq!(
        audits(pool, "guarded").await,
        [audit("retry_scheduled", Some("discord_guard_refused"))],
    );

    let fresh_guard = std::sync::Arc::new(
        two_bot_discord::ratelimit_guard::RateLimitGuard::new(Default::default()).unwrap(),
    );
    let rest = two_bot_discord::executor::ActionExecutor::with_proxy_and_guard(
        "synthetic-job-test-token".to_owned(),
        Some(mock.origin()),
        fresh_guard,
    )
    .unwrap();
    tick(pool, &rest, GUILD, || T0 + delay).await.expect("tick");
    assert_eq!(posts(&mock).len(), 1);
    let done = row(pool, "guarded").await;
    assert!(!done.enabled);
    assert!(!done.delivery_uncertain);
    assert_eq!(
        audits(pool, "guarded").await,
        [
            audit("retry_scheduled", Some("discord_guard_refused")),
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
async fn definition_replaced_mid_post_surfaces_a_refused_orphan_delete() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool().clone();
    seed(&pool, "edited", T0 - 1_000, None).await;
    let (mock, gate) = MockRest::start_gated(vec![posted()], ScriptedResponse::status(403)).await;
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
    assert_eq!(posts(&mock).len(), 1, "the orphan is never re-posted");
    let replaced = row(&pool, "edited").await;
    assert!(replaced.enabled, "the new definition stands");
    assert_eq!(replaced.next_run_at, format_iso_ms(T0 + 600_000));
    assert_eq!(
        audits(&pool, "edited").await,
        [audit("stale_completion", Some("discord_rejected"))]
    );
    mock.shutdown().await;
    fixture.close().await.unwrap();
}

#[tokio::test]
async fn definition_replaced_mid_post_surfaces_a_timed_out_orphan_delete() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool().clone();
    seed(&pool, "edited", T0 - 1_000, None).await;
    let (mock, gate) = MockRest::start_gated(
        vec![posted()],
        ScriptedResponse::status(204).delayed(Duration::from_secs(6)),
    )
    .await;
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
    assert_eq!(posts(&mock).len(), 1, "the orphan is never re-posted");
    let replaced = row(&pool, "edited").await;
    assert!(replaced.enabled, "the new definition stands");
    assert_eq!(replaced.next_run_at, format_iso_ms(T0 + 600_000));
    assert_eq!(
        audits(&pool, "edited").await,
        [audit("stale_completion", Some("discord_timeout"))]
    );
    mock.shutdown().await;
    fixture.close().await.unwrap();
}

/// Seed a due row with a caller-chosen body, stored verbatim exactly as
/// a row written before any validate-side screen would already sit in
/// the table. The ticker — the post boundary — is what must keep such a
/// stored body from ambushing the channel.
async fn seed_body(pool: &PgPool, id: &str, due_at: u64, body: &str) {
    let at = format_iso_ms(T0 - 3_600_000);
    let stored = put_scheduled(
        pool,
        &ScheduledWrite {
            id: id.to_owned(),
            guild_id: GUILD.to_owned(),
            channel_id: CHANNEL.to_owned(),
            body: body.to_owned(),
            next_run_at: format_iso_ms(due_at),
            interval_seconds: None,
            enabled: true,
            created_by: "5555".to_owned(),
            created_at: at.clone(),
            updated_by: "5555".to_owned(),
            updated_at: at,
        },
    )
    .await
    .expect("seed scheduled row with body");
    assert!(stored);
}

/// Inert on the wire: nothing left for the shared mention neutralizer
/// to rewrite (raw and zero-width-split mass mentions alike) and no
/// contiguous invite host Discord would linkify. A property of the
/// captured output, not a pinned neutralization spelling — and never
/// satisfied by a raw repost of the stored body.
fn assert_posted_inert(sent: &Value) {
    let content = sent["content"].as_str().expect("posted content text");
    assert!(
        !two_bot_core::message_safety::contains_mass_mention(content),
        "posted content still carries a mass mention: {content:?}"
    );
    assert!(
        !content.contains("@everyone") && !content.contains("@here"),
        "posted content still contains a pingable mass mention: {content:?}"
    );
    assert!(
        !content.to_lowercase().contains("discord.gg"),
        "posted content still contains a linkable invite host: {content:?}"
    );
    // Mention pills render as text only while parsing stays closed:
    // nothing in any posted body may actually notify a user or role.
    assert_eq!(
        sent["allowed_mentions"],
        json!({"parse": [], "roles": [], "users": [], "replied_user": false}),
        "ticker posts must keep the closed legacy mention policy"
    );
}

/// Acceptance: hostile bodies already in storage (written before any
/// validate-side screen, or by any other writer) never reach the
/// channel as pingable or linkable output. Each hostile row may be
/// defused or not posted at all; the benign control row in the same
/// tick proves the ticker really ran, and every hostile post that does
/// go out must be inert while keeping the announcement around the
/// payload.
#[tokio::test]
async fn hostile_stored_bodies_never_post_pingable_or_linkable_output() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    let hostile = [
        "Game night @everyone bring snacks",
        "Game night @here bring snacks",
        "Game night @e\u{200b}v\u{200b}e\u{200b}r\u{200b}y\u{200b}o\u{200b}n\u{200b}e bring snacks",
        "Game night @he\u{200d}re bring snacks",
        "Game night discord.gg/free-nitro bring snacks",
        "Game night https://DISCORD.GG/FREE-NITRO bring snacks",
        "Game night www.discord.gg/free-nitro bring snacks",
        "Game night @here claim at discord.gg/free-nitro <@&123> <@456> bring snacks",
    ];
    for (index, body) in hostile.iter().enumerate() {
        seed_body(
            pool,
            &format!("hostile-{index}"),
            T0 - 10_000 + index as u64 * 100,
            body,
        )
        .await;
    }
    seed_body(pool, "control", T0 - 1_000, "Game night Friday at 19:00").await;
    let mock = MockRest::start(Vec::new(), posted()).await;
    let rest = executor(&mock);

    tick(pool, &rest, GUILD, || T0).await.expect("tick");

    let sent = posts(&mock);
    let control = sent
        .iter()
        .find(|post| post["content"] == "Game night Friday at 19:00")
        .expect("the benign control posts in the same tick, byte-identical");
    assert_posted_inert(control);
    assert!(
        sent.len() <= hostile.len() + 1,
        "no stored body may post more than once, got {} posts",
        sent.len()
    );
    for post in &sent {
        assert_posted_inert(post);
        let content = post["content"].as_str().expect("posted content text");
        assert!(
            content.contains("Game night"),
            "defusing must keep the announcement, not replace it: {content:?}"
        );
        if content != "Game night Friday at 19:00" {
            assert!(
                content.contains("bring snacks"),
                "defusing must keep the text after the payload: {content:?}"
            );
        }
    }
    mock.shutdown().await;
    fixture.close().await.unwrap();
}

/// Acceptance: benign stored bodies post byte-identical — markdown,
/// newlines, dotted non-links and legitimate Unicode joiners are
/// announcement text, not an ambush, and the safety screen must not
/// rewrite them on the way to the channel.
#[tokio::test]
async fn benign_stored_bodies_post_byte_identical() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    let benign = [
        "Game night Friday at 19:00 — bring snacks!",
        "Line one\nLine two with **bold** and a `code` span",
        "Reminder: update to v1.2 and check notes.txt, می\u{200c}روم \u{1f469}\u{200d}\u{1f4bb}",
    ];
    for (index, body) in benign.iter().enumerate() {
        seed_body(
            pool,
            &format!("benign-{index}"),
            T0 - 10_000 + index as u64 * 100,
            body,
        )
        .await;
    }
    let mock = MockRest::start(Vec::new(), posted()).await;
    let rest = executor(&mock);

    tick(pool, &rest, GUILD, || T0).await.expect("tick");

    let sent = posts(&mock);
    let contents: Vec<&str> = sent
        .iter()
        .map(|post| post["content"].as_str().expect("posted content text"))
        .collect();
    assert_eq!(
        contents, benign,
        "benign bodies reach the channel byte-identical, in due order"
    );
    for post in &sent {
        assert_posted_inert(post);
    }
    mock.shutdown().await;
    fixture.close().await.unwrap();
}

#[tokio::test]
async fn retryable_failure_with_replaced_definition_audits_stale_completion() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool().clone();
    seed(&pool, "retry-stale", T0 - 1_000, None).await;
    let (mock, gate) = MockRest::start_gated(
        vec![ScriptedResponse::status(503)],
        ScriptedResponse::status(500),
    )
    .await;
    let rest = executor(&mock);

    let ticker = {
        let (pool, rest) = (pool.clone(), rest.clone());
        tokio::spawn(async move { tick(&pool, &rest, GUILD, || T0).await })
    };
    gate.wait_for_request().await;
    // The claim goes stale first: replacing the definition clears it, so the
    // re-queue lands nowhere.
    seed(&pool, "retry-stale", T0 + 600_000, None).await;
    gate.release();
    ticker.await.unwrap().expect("tick");

    assert_eq!(
        posts(&mock).len(),
        1,
        "the retryable failure is never re-posted"
    );
    let replaced = row(&pool, "retry-stale").await;
    assert!(replaced.enabled, "the new definition stands");
    assert_eq!(replaced.next_run_at, format_iso_ms(T0 + 600_000));
    assert_eq!(replaced.claim_token, None);
    assert_eq!(
        audits(&pool, "retry-stale").await,
        [audit("stale_completion", Some("discord_unavailable"))],
        "a retryable failure whose claim went stale is audited, not silent"
    );
    mock.shutdown().await;
    fixture.close().await.unwrap();
}

/// Make recording a run fail the way a store error would: the completion
/// update sets `last_run_at`, the claim/retry updates never do, so a
/// trigger on that column refuses exactly the recording write.
async fn refuse_run_recording(pool: &PgPool) {
    sqlx::raw_sql(
        "CREATE FUNCTION refuse_scheduled_completion() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.last_run_at IS DISTINCT FROM OLD.last_run_at THEN RAISE EXCEPTION 'fixture completion failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER fail_scheduled_completion BEFORE UPDATE ON scheduled_messages FOR EACH ROW EXECUTE FUNCTION refuse_scheduled_completion();",
    )
    .execute(pool)
    .await
    .expect("install completion-refusing trigger");
}

#[tokio::test]
async fn unrecorded_post_with_lost_claim_and_surviving_orphan_audits_stale_completion() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool().clone();
    seed(&pool, "unrecorded-surviving", T0 - 1_000, None).await;
    refuse_run_recording(&pool).await;
    // The gate holds the orphan DELETE (the last scripted response): the
    // window between the failed recording and the re-queue attempt. The
    // delete is refused, so the orphan survives and keeps no nonce.
    let (mock, gate) = MockRest::start_gated(
        vec![posted(), ScriptedResponse::status(403)],
        ScriptedResponse::status(500),
    )
    .await;
    let rest = executor(&mock);

    let ticker = {
        let (pool, rest) = (pool.clone(), rest.clone());
        tokio::spawn(async move { tick(&pool, &rest, GUILD, || T0).await })
    };
    gate.wait_for_request().await;
    // The claim is lost before the re-queue: replacing clears it.
    seed(&pool, "unrecorded-surviving", T0 + 600_000, None).await;
    gate.release();
    let outcome = ticker.await.unwrap();
    assert!(
        matches!(outcome, Err(ErrorClass::Database)),
        "a failed recording still fails the attempt, got {outcome:?}"
    );

    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].method, "DELETE");
    assert_eq!(requests[1].path, "/api/v10/channels/4444/messages/9001");
    assert_eq!(posts(&mock).len(), 1, "the orphan is never re-posted");
    let replaced = row(&pool, "unrecorded-surviving").await;
    assert!(replaced.enabled, "the new definition stands");
    assert_eq!(replaced.next_run_at, format_iso_ms(T0 + 600_000));
    assert_eq!(
        audits(&pool, "unrecorded-surviving").await,
        [audit("stale_completion", Some("database"))],
        "a surviving orphan with a lost claim audits stale, not retry-idempotent"
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
    supervisor.await.unwrap().unwrap();
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
