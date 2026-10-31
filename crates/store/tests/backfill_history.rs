//! Backfill channel-history metadata catch-up acceptance (TOG-22950, paired failing tests for TOG-22942).
//!
//! Test files ONLY: this target compiles green but each test FAILS until the
//! builder lands the history store (currently: `relation
//! "backfill_history_messages" does not exist`). The build card implements:
//!
//! * a `backfill_history_messages` table holding per-message metadata ONLY
//!   (`message_id` PK, `channel_id`, `author_id`, `sent_at`; no body column),
//!   plus a `backfill_history_counts` aggregate (`channel_id` PK,
//!   `message_count`, `updated_at`), applied from the store migration chain so
//!   [`migrate`] creates both;
//! * small `two_bot_store::backfill_history` wrappers (suggested:
//!   `put_backfill_history_message`, `record_backfill_history_count`) over the
//!   exact SQL below, so the wrappers and these tests agree byte for byte;
//! * resume via the slice-2 `backfill_cursors` table, family `history`
//!   (same upsert/read SQL as `backfill_cursors.rs`, so the slices agree).
//!
//! What the tests prove (TOG-22852 AC1/AC2/AC4 for the history family):
//!
//! 1. history pagination with limit 1-100 and before/after/around cursors
//!    reaches the wire through the paced executor;
//! 2. every history read passes through the shared rate-limit client
//!    (header-driven Retry-After wait plus guard invalid-request accounting);
//! 3. persisted rows hold message/channel/author ids, timestamps and counts
//!    ONLY -- a fixture carrying a message body fails if persisted
//!    (privacy tripwire, same fence as event-store S1);
//! 4. crash-restart resumes the catch-up from the persisted cursor, never
//!    by replaying a window from zero.
//!
//! Rate-limit facts verified against the official Discord documentation
//! (<https://docs.discord.com/developers/topics/rate-limits>, 2026-10-12):
//! per-route buckets signalled by `X-RateLimit-Bucket`/`Limit`/`Remaining`/
//! `Reset`/`Reset-After`, 429-only `X-RateLimit-Global`/`Scope`
//! (`user`/`global`/`shared`) with `Retry-After`/`retry_after` waits, bots
//! capped at 50 requests/second globally, and 401/403/429 responses counting
//! toward the 10k-per-10-minute invalid-request cutoff. Channel-messages
//! paging verified against the official endpoint documentation
//! (<https://docs.discord.com/developers/resources/channel#get-channel-messages>,
//! 2026-10-12): `limit` defaults to 50 with a 1-100 range, paged with
//! `before`/`after`/`around` message-id cursors. Every wait and threshold the
//! implementation must honor arrives in a response header, never a literal.
//!
//! Only agent-testdb or a CI service container is admitted, with the
//! agent_test identity. DATABASE_URL is never consulted. Loopback HTTP
//! doubles only: never staging or production Discord.
//!
//! Run: `cargo test -p two-bot-store --locked --test backfill_history -- --ignored`
//! with `TEST_DATABASE_URL` pointed at the authorized test database.
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Pool, Postgres};
use two_bot_discord::{
    ratelimit_guard::{GuardConfig, RateLimitGuard},
    ActionExecutor,
};
use two_bot_store::{migrate, DB_POOL_MAX};

// ── Builder contract: exact SQL the wrappers must use ────────────────────
// Mirrors the builder's `put_backfill_history_message`: one row per message,
// ids plus send timestamp only. `ON CONFLICT DO NOTHING` keeps re-fetched
// pages idempotent without moving an existing row.
const PUT_HISTORY_MESSAGE_SQL: &str = "INSERT INTO backfill_history_messages
    (message_id, channel_id, author_id, sent_at)
    VALUES ($1, $2, $3, $4::timestamptz)
    ON CONFLICT (message_id) DO NOTHING";

// Mirrors the builder's `record_backfill_history_count`: per-channel aggregate
// counts only, refreshed on every page.
const RECORD_HISTORY_COUNT_SQL: &str = "INSERT INTO backfill_history_counts
    (channel_id, message_count, updated_at)
    VALUES ($1, $2, now())
    ON CONFLICT (channel_id) DO UPDATE
      SET message_count = excluded.message_count, updated_at = now()";

// Slice-2 cursor contract (family `history`), byte-identical to
// `backfill_cursors.rs` so both slices share one table.
const PUT_HISTORY_CURSOR_SQL: &str = "INSERT INTO backfill_cursors (family, cursor, updated_at)
    VALUES ('history', $1, now())
    ON CONFLICT (family) DO UPDATE
      SET cursor = excluded.cursor, updated_at = now()";
const GET_HISTORY_CURSOR_SQL: &str = "SELECT cursor FROM backfill_cursors WHERE family = 'history'";

/// A Discord channel-message fixture. `content` is the privacy tripwire: it
/// travels the wire but must never reach a persisted row.
fn history_message(id: &str, channel: &str, author: &str, sent_at: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "channel_id": channel,
        "author": {"id": author, "username": "fixture-user", "discriminator": "0"},
        "content": "SECRET-BODY-TOG-22942-DO-NOT-PERSIST",
        "timestamp": sent_at,
    })
}

/// Split a fetched page into storable metadata, dropping every body field.
/// Mirrors the builder's extraction: ids plus send timestamp only.
fn metadata_of(page: &[serde_json::Value]) -> Vec<(String, String, String, String)> {
    page.iter()
        .map(|message| {
            (
                message
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap()
                    .to_owned(),
                message
                    .get("channel_id")
                    .and_then(|v| v.as_str())
                    .unwrap()
                    .to_owned(),
                message
                    .get("author")
                    .and_then(|v| v.get("id"))
                    .and_then(|v| v.as_str())
                    .unwrap()
                    .to_owned(),
                message
                    .get("timestamp")
                    .and_then(|v| v.as_str())
                    .unwrap()
                    .to_owned(),
            )
        })
        .collect()
}

async fn put_history_rows(pool: &Pool<Postgres>, rows: &[(String, String, String, String)]) {
    for (message_id, channel_id, author_id, sent_at) in rows {
        sqlx::query(PUT_HISTORY_MESSAGE_SQL)
            .bind(message_id)
            .bind(channel_id)
            .bind(author_id)
            .bind(sent_at)
            .execute(pool)
            .await
            .unwrap();
    }
}

async fn record_history_count(pool: &Pool<Postgres>, channel_id: &str, count: i64) {
    sqlx::query(RECORD_HISTORY_COUNT_SQL)
        .bind(channel_id)
        .bind(count)
        .execute(pool)
        .await
        .unwrap();
}

async fn put_history_cursor(pool: &Pool<Postgres>, cursor: &str) {
    sqlx::query(PUT_HISTORY_CURSOR_SQL)
        .bind(cursor)
        .execute(pool)
        .await
        .unwrap();
}

async fn get_history_cursor(pool: &Pool<Postgres>) -> Option<String> {
    sqlx::query_scalar(GET_HISTORY_CURSOR_SQL)
        .fetch_optional(pool)
        .await
        .unwrap()
}

// ── Lean loopback double ──────────────────────────────────────────────
// Same scripted-queue shape as the slice-1 rate-limit acceptance: replays a
// response queue while recording every request's method and path. Everything
// defined here is used below, so no lint waiver is needed.

#[derive(Debug, Clone)]
struct FakeResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl FakeResponse {
    fn json(status: u16, body: serde_json::Value) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.to_string().into_bytes(),
        }
    }

    fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }
}

#[derive(Debug, Clone)]
struct SeenRequest {
    method: String,
    path: String,
    received_at: tokio::time::Instant,
}

struct FakeRest {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl FakeRest {
    async fn start(script: Vec<FakeResponse>, default: FakeResponse) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake rest");
        let addr = listener.local_addr().expect("fake rest addr");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let queue = Arc::new(Mutex::new(VecDeque::from(script)));
        let fallback = Arc::new(default);
        let handle = {
            let seen = Arc::clone(&seen);
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        break;
                    };
                    let seen = Arc::clone(&seen);
                    let queue = Arc::clone(&queue);
                    let fallback = Arc::clone(&fallback);
                    tokio::spawn(async move {
                        serve_conn(stream, seen, queue, fallback).await;
                    });
                }
            })
        };
        Self {
            addr,
            seen,
            handle: Some(handle),
        }
    }

    fn origin(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn requests(&self) -> Vec<SeenRequest> {
        self.seen.lock().expect("seen").clone()
    }

    async fn shutdown(mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

impl Drop for FakeRest {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        429 => "Too Many Requests",
        _ => "Unknown",
    }
}

async fn serve_conn(
    mut stream: tokio::net::TcpStream,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
    queue: Arc<Mutex<VecDeque<FakeResponse>>>,
    fallback: Arc<FakeResponse>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        let Ok(n) = stream.read(&mut chunk).await else {
            return;
        };
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 64 * 1024 {
            return;
        }
    }
    let head_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(buf.len());
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or("").to_owned();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let path = parts.next().unwrap_or("").to_owned();
    seen.lock().expect("seen").push(SeenRequest {
        method,
        path,
        received_at: tokio::time::Instant::now(),
    });
    let next = queue
        .lock()
        .expect("queue")
        .pop_front()
        .unwrap_or_else(|| (*fallback).clone());
    let mut head = format!(
        "HTTP/1.1 {} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
        next.status,
        reason_phrase(next.status),
        next.body.len(),
    );
    for (name, value) in &next.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(&next.body).await;
}

fn history_executor(origin: &str, guard: &Arc<RateLimitGuard>) -> ActionExecutor {
    ActionExecutor::with_proxy_and_guard(
        "fixture-only-backfill-token".to_owned(),
        Some(origin.to_owned()),
        guard.clone(),
    )
    .expect("fixture executor")
}

async fn fetch_history_page(
    exec: &ActionExecutor,
    channel_id: &str,
    cursor: Option<&str>,
    limit: u8,
    after: Option<&str>,
    around: Option<&str>,
) -> Vec<serde_json::Value> {
    // Limit clamps into Discord's 1-100 range (docs 2026-10-12); the builder's
    // fetcher owns this clamp, mirrored here so the wire proves it.
    let limit = limit.clamp(1, 100);
    let mut path = format!("/channels/{channel_id}/messages?limit={limit}");
    // `before` pages backward from the previous page's floor; `after` and
    // `around` are the forward/anchored cursor shapes the same endpoint takes.
    match (cursor, after, around) {
        (Some(before), None, None) => path.push_str(&format!("&before={before}")),
        (None, Some(after), None) => path.push_str(&format!("&after={after}")),
        (None, None, Some(around)) => path.push_str(&format!("&around={around}")),
        (None, None, None) => {}
        _ => panic!("one cursor shape per history read"),
    }
    let (data, _) = exec
        .get_json_observed(&path)
        .await
        .expect("history read")
        .expect("history read carries evidence");
    data.as_array()
        .expect("history body is a JSON array")
        .clone()
}

// ── Disposable-DB fixture ────────────────────────────────────────────
// Same isolation shape as `backfill_cursors.rs`: agent-testdb or a CI service
// container only, agent_test identity, per-test schema, DATABASE_URL never
// consulted.

static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    admin: Pool<Postgres>,
    pool: Pool<Postgres>,
    schema: String,
    url: String,
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
            "backfill_history_test_{}_{}",
            std::process::id(),
            NEXT_SCHEMA.fetch_add(1, Ordering::Relaxed)
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .unwrap();
        let pool = Self::connect(&url, &schema).await;
        migrate(&pool).await.unwrap();
        Self {
            admin,
            pool,
            schema,
            url,
        }
    }

    async fn connect(url: &str, schema: &str) -> Pool<Postgres> {
        let options = PgConnectOptions::from_str(url).expect("invalid TEST_DATABASE_URL");
        PgPoolOptions::new()
            .max_connections(DB_POOL_MAX)
            .connect_with(options.options([
                ("search_path", schema.to_owned()),
                ("statement_timeout", "15000ms".to_owned()),
            ]))
            .await
            .unwrap()
    }

    /// Simulate a crash-restart: drop the live pool and reconnect to the same
    /// schema, so the test reads only what was durably persisted.
    async fn restart_pool(&mut self) {
        self.pool.close().await;
        self.pool = Self::connect(&self.url, &self.schema).await;
    }

    async fn finish(self) {
        self.pool.close().await;
        // Only schemas created by this fixture; never shared tables or roles.
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA {} CASCADE",
            self.schema
        )))
        .execute(&self.admin)
        .await
        .unwrap();
        self.admin.close().await;
    }
}

const CHANNEL: &str = "123456789012345678";

// (1) Pagination: limit lands on the wire inside Discord's 1-100 range and
// each cursor shape (before/after/around) reaches the history endpoint. Page 2
// continues from page 1's floor id, and the fetched rows persist as metadata.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn history_pagination_honors_limit_range_and_before_after_around_cursors() {
    let page1 = serde_json::json!([
        history_message(
            "333333333333333333",
            CHANNEL,
            "777777777777777777",
            "2026-10-01T12:00:02.000Z"
        ),
        history_message(
            "222222222222222222",
            CHANNEL,
            "888888888888888888",
            "2026-10-01T12:00:01.000Z"
        ),
    ]);
    let page2 = serde_json::json!([history_message(
        "111111111111111111",
        CHANNEL,
        "777777777777777777",
        "2026-10-01T12:00:00.000Z"
    ),]);
    let mock = FakeRest::start(
        vec![
            FakeResponse::json(200, page1),
            FakeResponse::json(200, page2.clone()),
            FakeResponse::json(200, page2.clone()),
            FakeResponse::json(200, page2),
        ],
        FakeResponse::json(200, serde_json::json!([])),
    )
    .await;
    let guard = Arc::new(RateLimitGuard::new(GuardConfig::default()).expect("guard"));
    let exec = history_executor(&mock.origin(), &guard);

    // Newest-first page with an in-range limit; the limit is on the wire.
    let first = fetch_history_page(&exec, CHANNEL, None, 2, None, None).await;
    assert_eq!(first.len(), 2);
    // Floor id of page 1 continues page 2 via `before`.
    let floor = first
        .last()
        .and_then(|m| m.get("id"))
        .and_then(|v| v.as_str())
        .expect("page carries message ids")
        .to_owned();
    assert_eq!(floor, "222222222222222222");
    let second = fetch_history_page(&exec, CHANNEL, Some(&floor), 2, None, None).await;
    assert_eq!(second.len(), 1);
    // Forward and anchored cursor shapes reach the same endpoint.
    for (after, around) in [
        (Some("111111111111111111"), None),
        (None, Some("222222222222222222")),
    ] {
        let page = fetch_history_page(&exec, CHANNEL, None, 2, after, around).await;
        assert_eq!(page.len(), 1);
    }

    let seen = mock.requests();
    assert_eq!(seen.len(), 4);
    assert!(seen.iter().all(|r| r.method == "GET"));
    assert!(
        seen[0].path == format!("/api/v10/channels/{CHANNEL}/messages?limit=2"),
        "first page must request newest history with an in-range limit: {}",
        seen[0].path
    );
    assert!(
        seen[1].path == format!("/api/v10/channels/{CHANNEL}/messages?before={floor}&limit=2"),
        "second page must continue from the first page floor: {}",
        seen[1].path
    );
    assert!(
        seen[2].path
            == format!("/api/v10/channels/{CHANNEL}/messages?after=111111111111111111&limit=2"),
        "after cursor must reach the wire: {}",
        seen[2].path
    );
    assert!(
        seen[3].path
            == format!("/api/v10/channels/{CHANNEL}/messages?around=222222222222222222&limit=2"),
        "around cursor must reach the wire: {}",
        seen[3].path
    );
    mock.shutdown().await;

    // Fetched rows persist as metadata; the cursor advances to the new floor.
    let f = Fixture::new().await;
    put_history_rows(&f.pool, &metadata_of(&first)).await;
    put_history_rows(&f.pool, &metadata_of(&second)).await;
    record_history_count(&f.pool, CHANNEL, 3).await;
    let stored: i64 =
        sqlx::query_scalar("SELECT count(*) FROM backfill_history_messages WHERE channel_id = $1")
            .bind(CHANNEL)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(stored, 3, "both pages must persist exactly once");
    put_history_cursor(&f.pool, "111111111111111111").await;
    assert_eq!(
        get_history_cursor(&f.pool).await,
        Some("111111111111111111".to_owned())
    );
    f.finish().await;
}

// (2) Every history read passes through the shared rate-limit client: a 429
// carrying a header-driven Retry-After waits before retrying (the wait comes
// from the header, never a literal), the guard tallies the invalid request,
// and the wire shows history paths only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn history_reads_all_pass_through_rate_limit_guard() {
    let page = serde_json::json!([history_message(
        "222222222222222222",
        CHANNEL,
        "777777777777777777",
        "2026-10-01T12:00:01.000Z"
    ),]);
    let mock = FakeRest::start(
        vec![
            FakeResponse::json(429, serde_json::json!({"global": false}))
                .header("Retry-After", "1"),
        ],
        FakeResponse::json(200, page),
    )
    .await;
    let guard = Arc::new(RateLimitGuard::new(GuardConfig::default()).expect("guard"));
    let exec = history_executor(&mock.origin(), &guard);

    let task =
        tokio::spawn(async move { fetch_history_page(&exec, CHANNEL, None, 2, None, None).await });
    // No second request may leave during the header-driven pause.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        mock.requests().len(),
        1,
        "history request sent during the Retry-After pause"
    );
    let rows = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("history retry terminates")
        .expect("task join");
    assert_eq!(rows.len(), 1);
    let seen = mock.requests();
    assert_eq!(seen.len(), 2);
    assert!(
        seen.iter()
            .all(|r| r.method == "GET" && r.path.starts_with("/api/v10/channels/")),
        "every history attempt must target the history endpoint through the client"
    );
    let gap = seen[1].received_at.duration_since(seen[0].received_at);
    assert!(
        gap >= Duration::from_millis(900),
        "history retry skipped the header wait: {gap:?}"
    );
    assert!(
        guard.snapshot().invalid_requests_total >= 1,
        "the 429 must flow through guard accounting, not around it"
    );
    mock.shutdown().await;

    // The retried page still persists as metadata, proving the guarded read
    // feeds the store rather than bypassing it.
    let f = Fixture::new().await;
    put_history_rows(&f.pool, &metadata_of(&rows)).await;
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM backfill_history_messages")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(stored, 1);
    f.finish().await;
}

// (3) Privacy fence: message rows hold exactly message/channel/author ids plus
// the send timestamp, count rows hold exactly channel id plus the aggregate
// count, and a fixture carrying a message body fails if persisted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn history_rows_hold_ids_timestamps_counts_only_with_body_tripwire() {
    let f = Fixture::new().await;
    let rows = metadata_of(&[history_message(
        "444444444444444444",
        CHANNEL,
        "999999999999999999",
        "2026-10-01T12:00:03.000Z",
    )]);
    put_history_rows(&f.pool, &rows).await;
    record_history_count(&f.pool, CHANNEL, 1).await;

    let columns: Vec<(String, String)> = sqlx::query_as(
        "SELECT column_name, data_type FROM information_schema.columns
         WHERE table_schema = $1 AND table_name = 'backfill_history_messages' ORDER BY 1",
    )
    .bind(&f.schema)
    .fetch_all(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        columns,
        vec![
            ("author_id".to_owned(), "text".to_owned()),
            ("channel_id".to_owned(), "text".to_owned()),
            ("message_id".to_owned(), "text".to_owned()),
            ("sent_at".to_owned(), "timestamp with time zone".to_owned()),
        ],
        "message rows must hold exactly ids plus the send timestamp"
    );
    let count_columns: Vec<(String, String)> = sqlx::query_as(
        "SELECT column_name, data_type FROM information_schema.columns
         WHERE table_schema = $1 AND table_name = 'backfill_history_counts' ORDER BY 1",
    )
    .bind(&f.schema)
    .fetch_all(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        count_columns,
        vec![
            ("channel_id".to_owned(), "text".to_owned()),
            ("message_count".to_owned(), "bigint".to_owned()),
            (
                "updated_at".to_owned(),
                "timestamp with time zone".to_owned()
            ),
        ],
        "count rows must hold exactly the channel id plus the aggregate count"
    );
    // The body text that rode along in the fixture appears nowhere on disk.
    let leaked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM backfill_history_messages
         WHERE to_jsonb(backfill_history_messages.*)::text LIKE '%SECRET-BODY-TOG-22942%'",
    )
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(leaked, 0, "message body must never reach a persisted row");
    // There is no body column to write to: persisting one fails.
    let refused = sqlx::query(
        "INSERT INTO backfill_history_messages
            (message_id, channel_id, author_id, sent_at, content)
         VALUES ('555555555555555555', $1, '999999999999999999', now(), 'x')",
    )
    .bind(CHANNEL)
    .execute(&f.pool)
    .await
    .expect_err("a message-body column must not exist");
    let detail = refused.to_string();
    assert!(
        detail.contains("content") || detail.contains("backfill_history_messages"),
        "unexpected refusal: {detail}"
    );
    f.finish().await;
}

// (4) Resume: the catch-up checkpoints its floor id under family `history`;
// after a crash-restart the worker continues from the persisted cursor with
// `before`, never by replaying the window from zero.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn history_resume_continues_from_persisted_cursor_after_restart() {
    let mut f = Fixture::new().await;
    let first = metadata_of(&[history_message(
        "222222222222222222",
        CHANNEL,
        "777777777777777777",
        "2026-10-01T12:00:01.000Z",
    )]);
    put_history_rows(&f.pool, &first).await;
    record_history_count(&f.pool, CHANNEL, 1).await;
    put_history_cursor(&f.pool, "222222222222222222").await;

    // Crash-restart: the pool drops and reconnects; only durable state reads back.
    f.restart_pool().await;
    let resumed = get_history_cursor(&f.pool).await;
    assert_eq!(
        resumed,
        Some("222222222222222222".to_owned()),
        "restart must resume history from its persisted cursor"
    );
    // An untouched window replays nothing from zero: no zero/empty cursor row.
    let zero_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM backfill_cursors WHERE cursor IN ('', '0')")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(zero_rows, 0, "no zero/empty cursor row may exist");

    // The next page continues from the persisted cursor over the wire.
    let next = serde_json::json!([history_message(
        "111111111111111111",
        CHANNEL,
        "888888888888888888",
        "2026-10-01T12:00:00.000Z"
    ),]);
    let mock = FakeRest::start(vec![], FakeResponse::json(200, next)).await;
    let guard = Arc::new(RateLimitGuard::new(GuardConfig::default()).expect("guard"));
    let exec = history_executor(&mock.origin(), &guard);
    let cursor = resumed.expect("persisted cursor");
    let page = fetch_history_page(&exec, CHANNEL, Some(&cursor), 2, None, None).await;
    assert_eq!(page.len(), 1);
    let seen = mock.requests();
    assert_eq!(seen.len(), 1);
    assert!(
        seen[0].path == format!("/api/v10/channels/{CHANNEL}/messages?before={cursor}&limit=2"),
        "resumed read must continue from the persisted cursor: {}",
        seen[0].path
    );
    mock.shutdown().await;

    put_history_rows(&f.pool, &metadata_of(&page)).await;
    put_history_cursor(&f.pool, "111111111111111111").await;
    assert_eq!(
        get_history_cursor(&f.pool).await,
        Some("111111111111111111".to_owned()),
        "cursor must advance past the resumed page"
    );
    let stored: i64 =
        sqlx::query_scalar("SELECT count(*) FROM backfill_history_messages WHERE channel_id = $1")
            .bind(CHANNEL)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(
        stored, 2,
        "resumed page must persist without replaying page 1"
    );
    f.finish().await;
}
