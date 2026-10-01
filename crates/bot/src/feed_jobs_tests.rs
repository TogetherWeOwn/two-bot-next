use super::*;

use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::stream;
use serde_json::json;
use two_bot_core::feeds::{plan_post, FeedKind};
use two_bot_core::feeds_connector::{
    fetch_feed_with, FeedConnectError, FeedConnector, FeedResolver, FeedResponse, FetchOptions,
};
use two_bot_core::feeds_http::PublicRequest;
use two_bot_testsupport::TestDatabase;

use crate::discord_test_common::{MockRest, ScriptedResponse};

const GUILD: &str = "2222";
const CHANNEL: &str = "3333";

fn executor(mock: &MockRest) -> ActionExecutor {
    crate::gateway::ensure_crypto_provider();
    ActionExecutor::with_proxy("synthetic-feed-test-token".to_owned(), Some(mock.origin())).unwrap()
}

fn feed(id: &str) -> FeedRelay {
    FeedRelay {
        id: id.to_owned(),
        guild_id: GUILD.to_owned(),
        channel_id: CHANNEL.to_owned(),
        kind: FeedKind::Rss,
        source: format!("https://example.org/{id}.xml"),
        enabled: true,
        last_checked_at: None,
        created_by: "4444".to_owned(),
        created_at: 1,
        updated_at: 1,
    }
}

fn item(index: usize) -> FeedItem {
    FeedItem {
        key: format!("item-{index}"),
        title: format!("Title {index}"),
        url: format!("https://example.org/item-{index}"),
        published_at: None,
    }
}

#[derive(Clone)]
struct FixtureHttp {
    xml: String,
    status: u16,
}

impl FixtureHttp {
    fn items(count: usize) -> Self {
        let mut xml = String::from("<rss><channel>");
        for i in (0..count).rev() {
            xml.push_str(&format!("<item><guid>item-{i}</guid><title>Title {i}</title><link>https://example.org/item-{i}</link></item>"));
        }
        xml.push_str("</channel></rss>");
        Self { xml, status: 200 }
    }
}

impl FeedResolver for FixtureHttp {
    async fn resolve(&self, _host: &str, _port: u16) -> Result<Vec<IpAddr>, std::io::Error> {
        Ok(vec!["93.184.216.34".parse().unwrap()])
    }
}

impl FeedConnector for FixtureHttp {
    async fn get<'a>(
        &'a self,
        request: &'a PublicRequest,
    ) -> Result<FeedResponse, FeedConnectError> {
        assert_eq!(request.url().scheme(), "https");
        assert_eq!(request.addresses()[0].ip().to_string(), "93.184.216.34");
        // No socket, public request or second DNS resolution is performed.
        Ok(FeedResponse {
            status: self.status.try_into().unwrap(),
            headers: vec![("content-type".to_owned(), "application/rss+xml".to_owned())],
            body: Box::pin(stream::iter(vec![Ok(self.xml.clone().into_bytes().into())])),
        })
    }
}

impl FeedFetch for FixtureHttp {
    fn fetch(&self, feed: FeedRelay) -> FetchFuture {
        let http = self.clone();
        Box::pin(async move {
            let fetched = fetch_feed_with(&http, &http, &feed.source, &FetchOptions::default())
                .await
                .map_err(|_| ErrorClass::Feed)?;
            parse_fetched(fetched)
        })
    }
}

#[derive(Clone)]
struct Items(Vec<FeedItem>);
impl FeedFetch for Items {
    fn fetch(&self, _feed: FeedRelay) -> FetchFuture {
        let items = self.0.clone();
        Box::pin(async move { Ok(items) })
    }
}

async fn fixture() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("explicit guarded test URL required");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create guarded test fixture")
}

async fn expire(pool: &PgPool) {
    sqlx::query("UPDATE feed_deliveries SET claimed_at = now() - interval '120 seconds' WHERE state = 'pending'")
        .execute(pool).await.unwrap();
}

async fn states(pool: &PgPool) -> (i64, i64) {
    sqlx::query_as("SELECT count(*) FILTER (WHERE state = 'delivered'), count(*) FILTER (WHERE state = 'pending') FROM feed_deliveries")
        .fetch_one(pool).await.unwrap()
}

fn post_count(mock: &MockRest) -> usize {
    mock.requests()
        .iter()
        .filter(|r| r.method == "POST")
        .count()
}

fn me() -> ScriptedResponse {
    ScriptedResponse::json(200, json!({"id":"5555"}))
}

fn history(post: &FeedPost) -> ScriptedResponse {
    ScriptedResponse::json(
        200,
        json!([{
            "id":"9001", "channel_id":post.channel_id,
            "author":{"id":"5555"}, "nonce":post.nonce
        }]),
    )
}

#[tokio::test]
async fn nonce_wire_identity_is_string_even_with_leading_zeroes() {
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id":"9001"}))).await;
    let rest = executor(&mock);
    for nonce in ["000000000000000000000001", "abcdef1234567890abcdef12"] {
        assert_eq!(
            rest.post_message_with_nonce(CHANNEL, "@everyone fixture", nonce)
                .await
                .unwrap(),
            "9001"
        );
        let body: serde_json::Value =
            serde_json::from_slice(&mock.requests().last().unwrap().body).unwrap();
        assert_eq!(body["nonce"], json!(nonce));
        assert_eq!(body["enforce_nonce"], true);
        assert_eq!(body["allowed_mentions"]["parse"], json!([]));
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn bounded_history_requires_exact_nonce_and_confirmed_own_message() {
    let post = plan_post(&feed("history"), &item(1)).unwrap();
    let good =
        json!({"id":"9001", "channel_id":CHANNEL, "author":{"id":"5555"}, "nonce":post.nonce});
    for bad in [
        json!([]),
        json!([{"id":"9001", "channel_id":CHANNEL, "author":{"id":"5555"}, "nonce":123}]),
        json!([{"id":"0", "channel_id":CHANNEL, "author":{"id":"5555"}, "nonce":post.nonce}]),
        json!([{"id":"9001", "channel_id":"9999", "author":{"id":"5555"}, "nonce":post.nonce}]),
        json!([{"id":"9001", "channel_id":CHANNEL, "author":{"id":"9999"}, "nonce":post.nonce}]),
        json!([good.clone(), good.clone()]),
        json!({"not":"history"}),
    ] {
        let mock = MockRest::start(
            vec![me(), ScriptedResponse::json(200, bad)],
            ScriptedResponse::status(500),
        )
        .await;
        assert!(reconcile(&executor(&mock), &post).await.is_err());
        assert_eq!(post_count(&mock), 0);
        assert!(mock.requests()[1].path.contains("limit=100"));
        mock.shutdown().await;
    }
    let mock = MockRest::start(vec![me(), history(&post)], ScriptedResponse::status(500)).await;
    assert_eq!(reconcile(&executor(&mock), &post).await.unwrap(), "9001");
    mock.shutdown().await;
}

#[test]
fn default_off_and_interval_validation() {
    let gates = FeatureGates::from_map(&std::collections::HashMap::new()).unwrap();
    assert!(!gates.announcements);
    assert_eq!(gates.feed_poll_seconds, 300);
    let action: JobAction = Arc::new(|| Box::pin(async { Ok(()) }));
    for seconds in [0, 59, 86401, u64::MAX] {
        assert!(scheduled_job(seconds, action.clone()).is_err());
    }
    for seconds in [60, 300, 86400] {
        let job = scheduled_job(seconds, action.clone()).unwrap();
        assert_eq!(job.startup_jitter, Duration::ZERO);
        assert_eq!(job.cadence, Duration::from_secs(seconds));
    }
}

#[tokio::test(start_paused = true)]
async fn registration_parks_off_values_without_constructing_work() {
    let calls = Arc::new(AtomicUsize::new(0));
    let action: JobAction = Arc::new({
        let calls = calls.clone();
        move || {
            calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    });
    for value in [None, Some("0"), Some("true"), Some("yes")] {
        let mut vars = std::collections::HashMap::new();
        if let Some(value) = value {
            vars.insert("TWO_ANNOUNCEMENTS".to_owned(), value.to_owned());
        }
        let gates = FeatureGates::from_map(&vars).unwrap();
        assert!(register_gated(gates, action.clone()).is_none());
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let gates = FeatureGates::from_map(&std::collections::HashMap::from([
        ("TWO_ANNOUNCEMENTS".to_owned(), "1".to_owned()),
        ("TWO_FEED_POLL_SECONDS".to_owned(), "60".to_owned()),
    ]))
    .unwrap();
    let job = register_gated(gates, action).unwrap();
    assert_eq!(job.cadence, Duration::from_secs(60));
    assert_eq!(calls.load(Ordering::SeqCst), 0, "registration is lazy");
    (job.action)().await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

struct Active(Arc<AtomicUsize>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[tokio::test(start_paused = true)]
async fn immediate_nonoverlap_and_supervisor_cancels_and_joins() {
    let starts = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let action: JobAction = Arc::new({
        let starts = starts.clone();
        let active = active.clone();
        let entered = entered.clone();
        move || {
            let starts = starts.clone();
            let active = active.clone();
            let entered = entered.clone();
            Box::pin(async move {
                starts.fetch_add(1, Ordering::SeqCst);
                assert_eq!(active.fetch_add(1, Ordering::SeqCst), 0);
                let _active = Active(active);
                entered.notify_one();
                std::future::pending().await
            })
        }
    });
    let job = scheduled_job(60, action).unwrap();
    let status = crate::jobs::statuses(&[NAME], false);
    let (stop, receiver) = tokio::sync::watch::channel(false);
    let owner = tokio::spawn(crate::jobs::supervise(vec![job], status.clone(), receiver));
    entered.notified().await;
    assert_eq!(
        starts.load(Ordering::SeqCst),
        1,
        "first tick without advancing time"
    );
    tokio::time::advance(Duration::from_secs(61)).await;
    tokio::task::yield_now().await;
    assert_eq!(starts.load(Ordering::SeqCst), 1, "busy tick skipped");
    stop.send_replace(true);
    owner.await.unwrap();
    assert_eq!(active.load(Ordering::SeqCst), 0, "active action joined");
    assert!(!status.read().await[NAME].running);
    tokio::time::advance(Duration::from_secs(600)).await;
    assert_eq!(starts.load(Ordering::SeqCst), 1, "no detached timer");
}

#[tokio::test(start_paused = true)]
async fn aborted_schedule_guard_allows_later_poll() {
    let starts = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let action: JobAction = Arc::new({
        let starts = starts.clone();
        let entered = entered.clone();
        move || {
            let starts = starts.clone();
            let entered = entered.clone();
            Box::pin(async move {
                starts.fetch_add(1, Ordering::SeqCst);
                entered.notify_one();
                std::future::pending().await
            })
        }
    });
    let job = scheduled_job(60, action).unwrap();
    let task = tokio::spawn((job.action)());
    entered.notified().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::advance(Duration::from_secs(60)).await;
    let task = tokio::spawn((job.action)());
    entered.notified().await;
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    task.abort();
    let _ = task.await;
}

#[tokio::test(start_paused = true)]
async fn supervisor_timeout_finishes_schedule_and_later_tick_runs() {
    let starts = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let action: JobAction = Arc::new({
        let starts = starts.clone();
        let active = active.clone();
        move || {
            let starts = starts.clone();
            let active = active.clone();
            Box::pin(async move {
                assert_eq!(active.fetch_add(1, Ordering::SeqCst), 0);
                let _active = Active(active);
                if starts.fetch_add(1, Ordering::SeqCst) == 0 {
                    std::future::pending::<()>().await;
                }
                Ok(())
            })
        }
    });
    let mut job = scheduled_job(60, action).unwrap();
    job.timeout = Duration::from_secs(3);
    let status = crate::jobs::statuses(&[NAME], false);
    let (stop, receiver) = tokio::sync::watch::channel(false);
    let owner = tokio::spawn(crate::jobs::supervise(vec![job], status.clone(), receiver));
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert_eq!(starts.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(3)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert_eq!(active.load(Ordering::SeqCst), 0);
    assert_eq!(
        status.read().await[NAME].last_error_class,
        Some(ErrorClass::Timeout)
    );
    tokio::time::advance(Duration::from_secs(57)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert_eq!(starts.load(Ordering::SeqCst), 2, "timeout guard released");
    assert!(status.read().await[NAME].last_success.is_some());
    assert_eq!(status.read().await[NAME].consecutive_failures, 0);
    stop.send_replace(true);
    owner.await.unwrap();
}

#[tokio::test]
async fn hermetic_http_pipeline_checks_status_and_parses() {
    let http = FixtureHttp::items(25);
    let parsed = http.fetch(feed("fixture")).await.unwrap();
    assert_eq!(parsed.len(), 25);
    let failed = FixtureHttp {
        status: 503,
        ..http
    };
    assert_eq!(
        failed.fetch(feed("fixture")).await.err(),
        Some(ErrorClass::Feed)
    );
}

#[tokio::test]
#[ignore = "requires guarded agent-testdb; CI runs explicitly"]
async fn overflow_and_restart_send_known_successes_once() {
    let db = fixture().await;
    store::add_feed(db.pool(), &feed("overflow")).await.unwrap();
    let script = (0..25)
        .map(|i| ScriptedResponse::json(200, json!({"id":(9001+i).to_string()})))
        .collect();
    let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
    let rest = executor(&mock);
    let http = FixtureHttp::items(25);
    run_once(db.pool(), &rest, GUILD, &http).await.unwrap();
    assert_eq!(post_count(&mock), 20);
    assert_eq!(
        states(db.pool()).await,
        (20, 0),
        "overflow was never sent and released"
    );
    let reopened = db.independent_pool().await.unwrap();
    run_once(&reopened, &rest, GUILD, &http).await.unwrap();
    run_once(&reopened, &rest, GUILD, &http).await.unwrap();
    assert_eq!(post_count(&mock), 25);
    assert_eq!(states(&reopened).await, (25, 0));
    for (i, request) in mock.requests().iter().enumerate() {
        let post = plan_post(&feed("overflow"), &item(i)).unwrap();
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["nonce"], json!(post.nonce));
        assert_eq!(body["enforce_nonce"], true);
        assert_eq!(body["allowed_mentions"]["parse"], json!([]));
    }
    reopened.close().await;
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires guarded agent-testdb; CI runs explicitly"]
async fn concurrent_pollers_have_one_winning_claim() {
    let db = fixture().await;
    store::add_feed(db.pool(), &feed("concurrent"))
        .await
        .unwrap();
    let second = db.independent_pool().await.unwrap();
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id":"9001"}))).await;
    let rest = executor(&mock);
    let http = FixtureHttp::items(1);
    let (a, b) = tokio::join!(
        run_once(db.pool(), &rest, GUILD, &http),
        run_once(&second, &rest, GUILD, &http)
    );
    a.unwrap();
    b.unwrap();
    assert_eq!(post_count(&mock), 1);
    assert_eq!(states(db.pool()).await, (1, 0));
    second.close().await;
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires guarded agent-testdb; CI runs explicitly"]
async fn ambiguous_send_stays_pending_on_history_miss_even_after_item_disappears() {
    let db = fixture().await;
    store::add_feed(db.pool(), &feed("ambiguous"))
        .await
        .unwrap();
    // 200 with no usable ID is an ambiguous receipt, not a confirmed delivery.
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!({})),
            me(),
            ScriptedResponse::json(200, json!([])),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let rest = executor(&mock);
    assert_eq!(
        run_once(db.pool(), &rest, GUILD, &FixtureHttp::items(1))
            .await
            .err(),
        Some(ErrorClass::RecoveryRequired)
    );
    assert_eq!(states(db.pool()).await, (0, 1));
    expire(db.pool()).await;
    let reopened = db.independent_pool().await.unwrap();
    assert_eq!(
        run_once(&reopened, &rest, GUILD, &FixtureHttp::items(0))
            .await
            .err(),
        Some(ErrorClass::RecoveryRequired)
    );
    assert_eq!(states(&reopened).await, (0, 1));
    assert_eq!(
        post_count(&mock),
        1,
        "no blind repost after bounded history miss"
    );
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM announcements_audit_log WHERE outcome = 'recovery_required'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(audits, 2);
    reopened.close().await;
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires guarded agent-testdb; CI runs explicitly"]
async fn success_before_completion_db_failure_recovers_without_repost() {
    let db = fixture().await;
    let relay = feed("db_failure");
    store::add_feed(db.pool(), &relay).await.unwrap();
    sqlx::raw_sql("CREATE FUNCTION refuse_feed_completion() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.state = 'delivered' THEN RAISE EXCEPTION 'fixture completion failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER fail_completion BEFORE UPDATE ON feed_deliveries FOR EACH ROW EXECUTE FUNCTION refuse_feed_completion();")
        .execute(db.pool()).await.unwrap();
    let post = plan_post(&relay, &item(0)).unwrap();
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!({"id":"9001"})),
            me(),
            history(&post),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let rest = executor(&mock);
    assert_eq!(
        run_once(db.pool(), &rest, GUILD, &FixtureHttp::items(1))
            .await
            .err(),
        Some(ErrorClass::RecoveryRequired)
    );
    assert_eq!(states(db.pool()).await, (0, 1));
    sqlx::raw_sql(
        "DROP TRIGGER fail_completion ON feed_deliveries; DROP FUNCTION refuse_feed_completion();",
    )
    .execute(db.pool())
    .await
    .unwrap();
    expire(db.pool()).await;
    let reopened = db.independent_pool().await.unwrap();
    run_once(&reopened, &rest, GUILD, &FixtureHttp::items(0))
        .await
        .unwrap();
    assert_eq!(states(&reopened).await, (1, 0));
    assert_eq!(post_count(&mock), 1);
    reopened.close().await;
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires guarded agent-testdb; CI runs explicitly"]
async fn item_failures_are_isolated_and_foreign_guild_is_silent() {
    let db = fixture().await;
    store::add_feed(db.pool(), &feed("isolation"))
        .await
        .unwrap();
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(403),
            ScriptedResponse::json(200, json!({"id":"9001"})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let rest = executor(&mock);
    let mut items = vec![item(1), item(0)];
    items.push(FeedItem {
        key: String::new(),
        title: String::new(),
        url: String::new(),
        published_at: None,
    });
    assert!(run_once(db.pool(), &rest, GUILD, &Items(items))
        .await
        .is_err());
    assert_eq!(post_count(&mock), 2);
    assert_eq!(
        states(db.pool()).await,
        (1, 1),
        "HTTP error acceptance conservatively stays pending"
    );
    run_once(db.pool(), &rest, "7777", &FixtureHttp::items(1))
        .await
        .unwrap();
    assert_eq!(post_count(&mock), 2);
    mock.shutdown().await;
    db.close().await.unwrap();
}

struct FailFirstFeed {
    failed: String,
    visited: Mutex<Vec<String>>,
}

impl FeedFetch for FailFirstFeed {
    fn fetch(&self, feed: FeedRelay) -> FetchFuture {
        self.visited.lock().unwrap().push(feed.id.clone());
        let failed = feed.id == self.failed;
        Box::pin(async move {
            if failed {
                Err(ErrorClass::Feed)
            } else {
                FixtureHttp::items(1).fetch(feed).await
            }
        })
    }
}

#[tokio::test]
#[ignore = "requires guarded agent-testdb; CI runs explicitly"]
async fn failed_feed_does_not_stop_later_feeds_or_hide_checked_status() {
    let db = fixture().await;
    for id in ["failed", "healthy"] {
        store::add_feed(db.pool(), &feed(id)).await.unwrap();
    }
    let feeds = store::list_feeds(db.pool(), GUILD, true).await.unwrap();
    let http = FailFirstFeed {
        failed: feeds[0].id.clone(),
        visited: Mutex::new(Vec::new()),
    };
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id":"9001"}))).await;
    assert_eq!(
        run_once(db.pool(), &executor(&mock), GUILD, &http)
            .await
            .err(),
        Some(ErrorClass::Feed)
    );
    assert_eq!(
        *http.visited.lock().unwrap(),
        feeds.iter().map(|f| f.id.clone()).collect::<Vec<_>>()
    );
    assert_eq!(post_count(&mock), 1);
    assert_eq!(states(db.pool()).await, (1, 0));
    assert!(store::list_feeds(db.pool(), GUILD, true)
        .await
        .unwrap()
        .iter()
        .all(|f| f.last_checked_at.is_some()));
    let outcomes: Vec<(String, String)> = sqlx::query_as("SELECT target_key, outcome FROM announcements_audit_log WHERE outcome IN ('poll_failed', 'poll_result') ORDER BY target_key, outcome")
        .fetch_all(db.pool()).await.unwrap();
    assert_eq!(outcomes.len(), 3, "failed feed plus one summary per feed");
    assert!(outcomes.contains(&(http.failed.clone(), "poll_failed".to_owned())));
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires guarded agent-testdb; CI runs explicitly"]
async fn wire_timeout_keeps_pending_but_later_item_delivers_and_history_miss_never_reposts() {
    let db = fixture().await;
    store::add_feed(db.pool(), &feed("timeout")).await.unwrap();
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!({"id":"9001"})).delayed(Duration::from_secs(6)),
            ScriptedResponse::json(200, json!({"id":"9002"})),
            me(),
            ScriptedResponse::json(200, json!([])),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let rest = executor(&mock);
    assert_eq!(
        run_once(db.pool(), &rest, GUILD, &FixtureHttp::items(2))
            .await
            .err(),
        Some(ErrorClass::RecoveryRequired)
    );
    assert_eq!(states(db.pool()).await, (1, 1));
    assert_eq!(
        post_count(&mock),
        2,
        "single attempt per item even on timeout"
    );
    expire(db.pool()).await;
    let reopened = db.independent_pool().await.unwrap();
    assert_eq!(
        run_once(&reopened, &rest, GUILD, &FixtureHttp::items(0))
            .await
            .err(),
        Some(ErrorClass::RecoveryRequired)
    );
    assert_eq!(states(&reopened).await, (1, 1));
    assert_eq!(post_count(&mock), 2);
    reopened.close().await;
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires guarded agent-testdb; CI runs explicitly"]
async fn bounded_recovery_rotates_unresolved_rows_even_when_they_remain_in_xml() {
    let db = fixture().await;
    let relay = feed("recovery_budget");
    store::add_feed(db.pool(), &relay).await.unwrap();
    for index in 0..25 {
        let post = plan_post(&relay, &item(index)).unwrap();
        assert_eq!(
            store::claim_delivery(
                db.pool(),
                GUILD,
                &post,
                &format!("seed-{index}"),
                now_millis_for_test() - 120_000
            )
            .await
            .unwrap(),
            Some(DeliveryClaim::Fresh)
        );
    }
    let pending = store::pending_deliveries(db.pool(), &relay, now_millis_for_test(), 200)
        .await
        .unwrap();
    assert_eq!(pending.len(), 25);
    let mut script = Vec::new();
    for _ in 0..20 {
        script.extend([me(), ScriptedResponse::json(200, json!([]))]);
    }
    script.push(ScriptedResponse::json(200, json!({"id":"9999"})));
    // Only leases actually searched rotate. The five previously unsearched
    // rows must be first next pass, regardless of their current XML presence.
    for post in &pending[20..] {
        script.extend([me(), history(post)]);
    }
    for _ in 0..15 {
        script.extend([me(), ScriptedResponse::json(200, json!([]))]);
    }
    let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
    let rest = executor(&mock);
    assert_eq!(
        run_once(db.pool(), &rest, GUILD, &FixtureHttp::items(26))
            .await
            .err(),
        Some(ErrorClass::RecoveryRequired)
    );
    assert_eq!(states(db.pool()).await, (1, 25));
    assert_eq!(
        post_count(&mock),
        1,
        "fresh item not blocked by recovery budget"
    );
    assert_eq!(
        mock.requests().iter().filter(|r| r.method == "GET").count(),
        40,
        "twenty bounded recovery attempts, shared with XML"
    );
    sqlx::query("UPDATE feed_deliveries SET claimed_at = claimed_at - interval '120 seconds' WHERE state = 'pending'").execute(db.pool()).await.unwrap();
    let rotated = store::pending_deliveries(db.pool(), &relay, now_millis_for_test(), 200)
        .await
        .unwrap();
    assert_eq!(
        rotated[..5].iter().map(|p| &p.item_key).collect::<Vec<_>>(),
        pending[20..]
            .iter()
            .map(|p| &p.item_key)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        run_once(db.pool(), &rest, GUILD, &FixtureHttp::items(0))
            .await
            .err(),
        Some(ErrorClass::RecoveryRequired)
    );
    assert_eq!(states(db.pool()).await, (6, 20));
    assert_eq!(post_count(&mock), 1, "recovery never posts");
    assert_eq!(
        mock.requests().iter().filter(|r| r.method == "GET").count(),
        80
    );
    mock.shutdown().await;
    db.close().await.unwrap();
}
