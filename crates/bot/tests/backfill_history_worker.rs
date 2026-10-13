//! Production-path history acceptance; deliberately red until the worker exists.
//!
//! Minimal production API required in `two_bot::backfill_history`:
//! - `HistoryWorker::new(sqlx::PgPool, ActionExecutor) -> HistoryWorker`;
//! - `run_page(&self, HistoryRequest) -> Result<HistoryPage, _>` (async);
//! - `HistoryRequest { channel_id: String, limit: u16, before: Option<String>,
//!   after: Option<String>, around: Option<String> }`;
//! - `HistoryPage { fetched: usize, inserted: u64, message_count: i64,
//!   cursor: Option<String> }` describes the committed page, not a planned write.
//!
//! An explicit cursor seeds a request; without one the worker loads its durable
//! channel frontier, or starts at the newest page if none exists. A page and its
//! unique-row count/checkpoint commit atomically. Before/default catch-up uses
//! the oldest returned message as its next frontier. An empty page preserves it.
//! A fresh worker/pool must resume without any test-side cursor read/write logic.
//!
//! The fake is only the HTTP server. Production owns request construction,
//! extraction, pagination, persistence, counts and checkpoints. SQL below only
//! observes state or injects a constraint failure in an owned disposable DB.
//!
//! Official references re-read 2026-10-13:
//! <https://docs.discord.com/developers/topics/rate-limits>
//! <https://docs.discord.com/developers/resources/message#get-channel-messages>
//! Use Retry-After/retry_after, not remembered quotas. Shared-scope 429s do not
//! count toward invalid requests. New DB acceptance tests are not ignored.

#[path = "../../discord/tests/common/mod.rs"]
mod common;

use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use common::{MockRest, RestRequest, ScriptedResponse};
use futures_util::{future::LocalBoxFuture, FutureExt};
use serde_json::{json, Value};
use sqlx::PgPool;
use two_bot::backfill_history::{HistoryRequest, HistoryWorker};
use two_bot_cutover::staging_migrate::MIGRATOR;
use two_bot_discord::{
    ratelimit_guard::{GuardConfig, RateLimitGuard},
    ActionExecutor,
};
use two_bot_testsupport::TestDatabase;

const CHANNEL: &str = "123456789012345678";
const AUTHOR: &str = "777777777777777777";
const NEWEST: &str = "333333333333333333";
const MIDDLE: &str = "222222222222222222";
const OLDEST: &str = "111111111111111111";
const BODY: &str = "HISTORY-BODY-MUST-NOT-BE-PERSISTED";

async fn with_fixture(test: impl for<'db> FnOnce(&'db TestDatabase) -> LocalBoxFuture<'db, ()>) {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test bootstrap required");
    let db = TestDatabase::create(&url, &MIGRATOR)
        .await
        .expect("create cutover-chain fixture");
    // Failing acceptance must still explicitly tear down its owned database.
    let result = AssertUnwindSafe(test(&db)).catch_unwind().await;
    db.close()
        .await
        .expect("drop fixture even after assertion failure");
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

fn request(channel: &str, limit: u16) -> HistoryRequest {
    HistoryRequest {
        channel_id: channel.to_owned(),
        limit,
        before: None,
        after: None,
        around: None,
    }
}

fn message(id: &str, timestamp: &str) -> Value {
    json!({
        "id": id, "channel_id": CHANNEL,
        "author": {"id": AUTHOR, "username": BODY, "discriminator": "0", "avatar": null},
        "timestamp": timestamp, "edited_timestamp": null,
        "content": BODY, "attachments": [], "embeds": [{"description": BODY}],
        "mentions": [], "mention_roles": [], "mention_everyone": false,
        "pinned": false, "tts": false, "type": 0, "flags": 0,
        "components": []
    })
}

fn first_page() -> Value {
    json!([
        message(NEWEST, "2026-10-01T12:00:02Z"),
        message(MIDDLE, "2026-10-01T12:00:01Z")
    ])
}

fn last_page() -> Value {
    json!([message(OLDEST, "2026-10-01T12:00:00Z")])
}

fn ok(body: Value) -> ScriptedResponse {
    ScriptedResponse::json(200, body)
}

fn limited(scope: &str) -> ScriptedResponse {
    let mut response = ScriptedResponse::rate_limited(0.4, "0.4");
    response
        .headers
        .push(("X-RateLimit-Scope".to_owned(), scope.to_owned()));
    response
}

fn denied(status: u16) -> ScriptedResponse {
    ScriptedResponse::json(status, json!({"message": "fixture rejection"}))
}

struct Http {
    rest: MockRest,
}

impl Http {
    async fn start(responses: Vec<ScriptedResponse>) -> Self {
        Self {
            rest: MockRest::start(responses, denied(500)).await,
        }
    }

    fn worker(&self, pool: &PgPool, guard: Arc<RateLimitGuard>) -> HistoryWorker {
        let executor = ActionExecutor::with_proxy_and_guard(
            "fixture-only-history-token".to_owned(),
            Some(self.rest.origin()),
            guard,
        )
        .expect("production executor on loopback");
        HistoryWorker::new(pool.clone(), executor)
    }

    fn seen(&self) -> Vec<RestRequest> {
        self.rest.requests()
    }
}

fn guard() -> Arc<RateLimitGuard> {
    Arc::new(RateLimitGuard::new(GuardConfig::default()).expect("production guard"))
}

// Parse only captured wire traces. No request builder or pagination helper is
// called by either side of these expectations; parameter order is irrelevant.
fn assert_wire(seen: &RestRequest, channel: &str, expected: &[(&str, &str)]) {
    assert_eq!(seen.method, "GET");
    let (path, query) = seen.path.split_once('?').expect("query parameters");
    assert_eq!(path, format!("/api/v10/channels/{channel}/messages"));
    let pairs: Vec<_> = query
        .split('&')
        .map(|p| p.split_once('=').expect("key=value"))
        .collect();
    let params: BTreeMap<_, _> = pairs.iter().copied().collect();
    assert_eq!(
        pairs.len(),
        params.len(),
        "duplicate parameters are forbidden"
    );
    assert_eq!(params, expected.iter().copied().collect());
}

async fn checkpoint(pool: &PgPool) -> Vec<(String, String)> {
    sqlx::query_as("SELECT family, cursor FROM backfill_cursors ORDER BY family")
        .fetch_all(pool)
        .await
        .expect("observe durable frontier")
}

async fn stored(pool: &PgPool) -> (Vec<(String, String, String, String)>, i64) {
    let rows = sqlx::query_as(
        "SELECT message_id, channel_id, author_id,
                to_char(sent_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')
         FROM backfill_history_messages ORDER BY message_id",
    )
    .fetch_all(pool)
    .await
    .expect("observe production metadata");
    let count = sqlx::query_scalar(
        "SELECT message_count FROM backfill_history_counts WHERE channel_id = $1",
    )
    .bind(CHANNEL)
    .fetch_one(pool)
    .await
    .expect("observe production count");
    (rows, count)
}

// Catches production omission/wrong clamping and silently discarded cursor modes.
#[tokio::test]
async fn worker_normalizes_boundary_limits_and_sends_each_cursor_shape() {
    with_fixture(|db| {
        async move {
            let http = Http::start((0..16).map(|_| ok(json!([]))).collect()).await;
            let worker = http.worker(db.pool(), guard());
            let mut case = 0_u64;
            for (input, normalized) in [(0, "1"), (1, "1"), (100, "100"), (101, "100")] {
                for cursor in [None, Some("before"), Some("after"), Some("around")] {
                    let channel = (123456789012345678_u64 + case).to_string();
                    let mut req = request(&channel, input);
                    match cursor {
                        Some("before") => req.before = Some(MIDDLE.to_owned()),
                        Some("after") => req.after = Some(MIDDLE.to_owned()),
                        Some("around") => req.around = Some(MIDDLE.to_owned()),
                        None => {}
                        _ => unreachable!("literal cursor cases"),
                    }
                    let result = worker.run_page(req).await.expect("production page");
                    assert_eq!(
                        (result.fetched, result.inserted, result.message_count),
                        (0, 0, 0)
                    );
                    let mut expected = vec![("limit", normalized)];
                    if let Some(name) = cursor {
                        expected.push((name, MIDDLE));
                    }
                    let seen = http.seen();
                    assert_eq!(
                        seen.len(),
                        case as usize + 1,
                        "one wire page per invocation"
                    );
                    assert_wire(&seen[case as usize], &channel, &expected);
                    case += 1;
                }
            }
            drop(worker);
        }
        .boxed_local()
    })
    .await;
}

// Catches accepting conflicting bounds or validating only after sending HTTP.
#[tokio::test]
async fn worker_rejects_all_conflicting_cursor_combinations_before_http() {
    with_fixture(|db| {
        async move {
            let http = Http::start(vec![]).await;
            let worker = http.worker(db.pool(), guard());
            for (before, after, around) in [
                (true, true, false),
                (true, false, true),
                (false, true, true),
                (true, true, true),
            ] {
                let mut req = request(CHANNEL, 2);
                req.before = before.then(|| MIDDLE.to_owned());
                req.after = after.then(|| MIDDLE.to_owned());
                req.around = around.then(|| MIDDLE.to_owned());
                assert!(
                    worker.run_page(req).await.is_err(),
                    "conflicting cursor options must fail"
                );
                assert!(http.seen().is_empty(), "invalid request reached HTTP");
                assert!(
                    checkpoint(db.pool()).await.is_empty(),
                    "invalid request wrote a frontier"
                );
            }
            drop(worker);
        }
        .boxed_local()
    })
    .await;
}

// Catches test-only extraction, body persistence, duplicate inserts, and fetched
// length being incorrectly added to the durable unique-message count on replay.
#[tokio::test]
async fn worker_persists_metadata_and_replays_without_inflating_counts() {
    with_fixture(|db| {
        async move {
            let overlap = json!([
                message(MIDDLE, "2026-10-01T12:00:01Z"),
                message(OLDEST, "2026-10-01T12:00:00Z")
            ]);
            let http = Http::start(vec![ok(first_page()), ok(overlap.clone()), ok(overlap)]).await;
            let worker = http.worker(db.pool(), guard());
            for (fetched, inserted, count, frontier) in
                [(2, 2, 2, MIDDLE), (2, 1, 3, OLDEST), (2, 0, 3, OLDEST)]
            {
                let page = worker
                    .run_page(request(CHANNEL, 2))
                    .await
                    .expect("production persistence");
                assert_eq!(
                    (page.fetched, page.inserted, page.message_count),
                    (fetched, inserted, count)
                );
                assert_eq!(page.cursor.as_deref(), Some(frontier));
            }
            let (rows, count) = stored(db.pool()).await;
            assert_eq!(count, 3);
            assert_eq!(
                rows,
                [
                    (OLDEST, CHANNEL, AUTHOR, "2026-10-01T12:00:00Z"),
                    (MIDDLE, CHANNEL, AUTHOR, "2026-10-01T12:00:01Z"),
                    (NEWEST, CHANNEL, AUTHOR, "2026-10-01T12:00:02Z"),
                ]
                .map(|(id, channel, author, timestamp)| (
                    id.to_owned(),
                    channel.to_owned(),
                    author.to_owned(),
                    timestamp.to_owned()
                ))
            );
            let persisted: Vec<String> = sqlx::query_scalar(
                "SELECT to_jsonb(m)::text FROM backfill_history_messages m
         UNION ALL SELECT to_jsonb(c)::text FROM backfill_history_counts c
         UNION ALL SELECT to_jsonb(f)::text FROM backfill_cursors f",
            )
            .fetch_all(db.pool())
            .await
            .expect("inspect persisted history state");
            assert!(
                persisted.iter().all(|row| !row.contains(BODY)),
                "raw body leaked into production storage"
            );
            let seen = http.seen();
            assert_eq!(seen.len(), 3);
            assert_wire(&seen[0], CHANNEL, &[("limit", "2")]);
            assert_wire(&seen[1], CHANNEL, &[("before", MIDDLE), ("limit", "2")]);
            assert_wire(&seen[2], CHANNEL, &[("before", OLDEST), ("limit", "2")]);
            drop(worker);
        }
        .boxed_local()
    })
    .await;
}

// Catches in-memory-only frontiers, resetting to newest on restart, and moving
// the cursor/count on the empty page that terminates catch-up.
#[tokio::test]
async fn restarted_worker_resumes_durable_frontier_and_preserves_it_on_empty_page() {
    with_fixture(|db| {
        async move {
            let first_http = Http::start(vec![ok(first_page())]).await;
            let first_pool = db.independent_pool().await.expect("first worker pool");
            let first = first_http.worker(&first_pool, guard());
            let page = first
                .run_page(request(CHANNEL, 2))
                .await
                .expect("first page");
            assert_eq!((page.fetched, page.inserted, page.message_count), (2, 2, 2));
            assert_eq!(page.cursor.as_deref(), Some(MIDDLE));
            assert!(
                !checkpoint(db.pool()).await.is_empty(),
                "frontier must be durable"
            );
            drop(first);
            first_pool.close().await;
            drop(first_http);

            let next_http = Http::start(vec![ok(last_page()), ok(json!([]))]).await;
            let next_pool = db.independent_pool().await.expect("fresh worker pool");
            let next = next_http.worker(&next_pool, guard());
            let page = next
                .run_page(request(CHANNEL, 2))
                .await
                .expect("restart page");
            assert_eq!((page.fetched, page.inserted, page.message_count), (1, 1, 3));
            assert_eq!(page.cursor.as_deref(), Some(OLDEST));
            let frontier = checkpoint(db.pool()).await;
            let empty = next
                .run_page(request(CHANNEL, 2))
                .await
                .expect("empty terminal page");
            assert_eq!(
                (empty.fetched, empty.inserted, empty.message_count),
                (0, 0, 3)
            );
            assert_eq!(empty.cursor.as_deref(), Some(OLDEST));
            assert_eq!(
                checkpoint(db.pool()).await,
                frontier,
                "empty page must not advance checkpoint"
            );
            assert_eq!(stored(db.pool()).await.1, 3);
            let seen = next_http.seen();
            assert_eq!(seen.len(), 2);
            assert_wire(&seen[0], CHANNEL, &[("before", MIDDLE), ("limit", "2")]);
            assert_wire(&seen[1], CHANNEL, &[("before", OLDEST), ("limit", "2")]);
            drop(next);
            next_pool.close().await;
        }
        .boxed_local()
    })
    .await;
}

// Catches a checkpoint advanced before a failed fetch, and a restart that loses
// the failed page. The 403 is a real client result, not a fake worker error.
#[tokio::test]
async fn failed_http_page_keeps_frontier_and_is_retrieved_after_restart() {
    with_fixture(|db| {
        async move {
            let http = Http::start(vec![ok(first_page()), denied(403), ok(last_page())]).await;
            let accounting = guard();
            let worker = http.worker(db.pool(), accounting.clone());
            worker
                .run_page(request(CHANNEL, 2))
                .await
                .expect("seed through worker");
            let frontier = checkpoint(db.pool()).await;
            let before = stored(db.pool()).await;
            assert!(worker.run_page(request(CHANNEL, 2)).await.is_err());
            assert_eq!(checkpoint(db.pool()).await, frontier);
            assert_eq!(stored(db.pool()).await, before);
            assert_eq!(accounting.snapshot().invalid_requests_total, 1);
            drop(worker);
            let pool = db.independent_pool().await.expect("restarted worker pool");
            let worker = http.worker(&pool, guard());
            let page = worker
                .run_page(request(CHANNEL, 2))
                .await
                .expect("retry unpersisted page");
            assert_eq!((page.fetched, page.inserted, page.message_count), (1, 1, 3));
            assert_eq!(page.cursor.as_deref(), Some(OLDEST));
            let seen = http.seen();
            assert_eq!(seen.len(), 3);
            assert_wire(&seen[1], CHANNEL, &[("before", MIDDLE), ("limit", "2")]);
            assert_wire(&seen[2], CHANNEL, &[("before", MIDDLE), ("limit", "2")]);
            drop(worker);
            pool.close().await;
        }
        .boxed_local()
    })
    .await;
}

// Catches separately committed message rows/count/checkpoint. The count write
// fails after extraction; every earlier write from that page must roll back.
#[tokio::test]
async fn failed_count_persistence_rolls_back_page_and_frontier_before_restart() {
    with_fixture(|db| async move {
    let http = Http::start(vec![ok(first_page()), ok(last_page()), ok(last_page())]).await;
    let worker = http.worker(db.pool(), guard());
    worker
        .run_page(request(CHANNEL, 2))
        .await
        .expect("seed through worker");
    let frontier = checkpoint(db.pool()).await;
    let before = stored(db.pool()).await;
    sqlx::query("ALTER TABLE backfill_history_counts ADD CONSTRAINT fixture_refuse_new_count CHECK (message_count <= 2)")
        .execute(db.pool()).await.expect("inject persistence failure in owned fixture");
    assert!(
        worker.run_page(request(CHANNEL, 2)).await.is_err(),
        "persistence failure must reach caller"
    );
    assert_eq!(
        checkpoint(db.pool()).await,
        frontier,
        "failed page advanced frontier"
    );
    assert_eq!(
        stored(db.pool()).await,
        before,
        "failed page partially committed"
    );
    sqlx::query("ALTER TABLE backfill_history_counts DROP CONSTRAINT fixture_refuse_new_count")
        .execute(db.pool())
        .await
        .expect("remove fixture fault");
    drop(worker);
    let pool = db.independent_pool().await.expect("fresh worker pool");
    let worker = http.worker(&pool, guard());
    let page = worker
        .run_page(request(CHANNEL, 2))
        .await
        .expect("persist refetched page");
    assert_eq!((page.fetched, page.inserted, page.message_count), (1, 1, 3));
    assert_eq!(page.cursor.as_deref(), Some(OLDEST));
    assert_eq!(stored(db.pool()).await.0.len(), 3);
    let seen = http.seen();
    assert_eq!(seen.len(), 3);
    assert_wire(&seen[1], CHANNEL, &[("before", MIDDLE), ("limit", "2")]);
    assert_wire(&seen[2], CHANNEL, &[("before", MIDDLE), ("limit", "2")]);
    drop(worker);
    pool.close().await;
    }.boxed_local()).await;
}

// Catches bypassing the shared rate-limit client and retrying immediately, or
// checkpointing the page before the successful response is persisted.
#[tokio::test]
async fn server_retry_after_defers_same_page_and_accounts_for_the_user_429() {
    with_fixture(|db| {
        async move {
            let (rest, gate) = MockRest::start_gated(
                vec![ok(first_page()), limited("user"), ok(last_page())],
                denied(500),
            )
            .await;
            let http = Http { rest };
            let accounting = guard();
            let worker = http.worker(db.pool(), accounting.clone());
            worker
                .run_page(request(CHANNEL, 2))
                .await
                .expect("first page");
            let frontier = checkpoint(db.pool()).await;
            // Hold the successful response while observing state. Join within the test
            // future so a timeout/panic cancels the worker rather than detaching it.
            let (page, (waiting_frontier, waiting_count)) =
                tokio::time::timeout(Duration::from_secs(10), async {
                    tokio::join!(worker.run_page(request(CHANNEL, 2)), async {
                        gate.wait_for_request().await;
                        let waiting_frontier = checkpoint(db.pool()).await;
                        let waiting_count = stored(db.pool()).await.1;
                        gate.release();
                        (waiting_frontier, waiting_count)
                    })
                })
                .await
                .expect("bounded retry and state observation");
            assert_eq!(waiting_frontier, frontier, "429 advanced durable frontier");
            assert_eq!(waiting_count, 2);
            let page = page.expect("retried production page");
            assert_eq!((page.fetched, page.inserted, page.message_count), (1, 1, 3));
            assert_eq!(page.cursor.as_deref(), Some(OLDEST));
            assert_eq!(accounting.snapshot().invalid_requests_total, 1);
            let seen = http.seen();
            assert_eq!(seen.len(), 3);
            assert_wire(&seen[1], CHANNEL, &[("before", MIDDLE), ("limit", "2")]);
            assert_wire(&seen[2], CHANNEL, &[("before", MIDDLE), ("limit", "2")]);
            assert!(
                seen[2].received_at.duration_since(seen[1].received_at)
                    >= Duration::from_millis(400),
                "Retry-After was not honored"
            );
        }
        .boxed_local()
    })
    .await;
}

// Catches bypassed invalid-request accounting and treating shared resource
// limits as user errors. Counter deltas, not mutable quota constants, are pinned.
#[tokio::test]
async fn production_client_accounts_for_401_but_excludes_shared_scope_429() {
    with_fixture(|db| {
        async move {
            let denied = Http::start(vec![denied(401)]).await;
            let accounting = guard();
            let worker = denied.worker(db.pool(), accounting.clone());
            assert!(worker.run_page(request(CHANNEL, 2)).await.is_err());
            assert_eq!(accounting.snapshot().invalid_requests_total, 1);
            assert_eq!(denied.seen().len(), 1);
            drop(worker);
            let limited = Http::start(vec![limited("shared"), ok(json!([]))]).await;
            let accounting = guard();
            let worker = limited.worker(db.pool(), accounting.clone());
            let page = tokio::time::timeout(
                Duration::from_secs(10),
                worker.run_page(request(CHANNEL, 2)),
            )
            .await
            .expect("bounded shared retry")
            .expect("shared retry page");
            assert_eq!((page.fetched, page.inserted, page.message_count), (0, 0, 0));
            assert_eq!(accounting.snapshot().invalid_requests_total, 0);
            let seen = limited.seen();
            assert_eq!(seen.len(), 2);
            assert!(
                seen[1].received_at.duration_since(seen[0].received_at)
                    >= Duration::from_millis(400)
            );
            drop(worker);
        }
        .boxed_local()
    })
    .await;
}
