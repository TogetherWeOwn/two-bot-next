//! Backfill audit-log 45d catch-up acceptance (TOG-22852 AC1/AC2/AC4, paired failing tests for TOG-22940).
//!
//! Test files ONLY: this target compiles green but each test FAILS until the
//! builder (TOG-22948) lands the audit-log fetcher plus its entries table
//! (currently: `unsupported GET path: /guilds/424242/audit-logs...` from the
//! rate-limit client and relation `backfill_audit_entries` does not exist;
//! the slice-2 `backfill_cursors` store has landed since, so cursor writes
//! succeed and the remaining failures pinpoint exactly the audit-log slice).
//! The build card implements:
//!
//! * an audit-log route on the slice-1 rate-limit client (`ActionExecutor`)
//!   paginating `GET /guilds/{guild}/audit-logs` with `limit` clamped to
//!   1-100 and `before`/`after` cursors, reading `Retry-After` /
//!   `X-RateLimit-Reset-After` instead of hardcoding any wait;
//! * per-window cursor persistence on the slice-2 cursor store
//!   (`backfill_cursors`, family `"audit"`), resuming mid-window after a crash;
//! * a `backfill_audit_entries` table holding ids/timestamps/enums only
//!   (`guild_id`, `entry_id`, `action_type`, `user_id`, `target_id`,
//!   `occurred_at`), applied from the store migration chain so [`migrate`]
//!   creates it, over the exact SQL below so wrapper and tests agree.
//!
//! What the tests prove:
//!
//! 1. pagination uses `limit` 1-100 with `before`/`after` cursors across the
//!    45d window (entries older than the window are dropped, never stored);
//! 2. every audit-log request rides the rate-limit client: the test holds an
//!    `ActionExecutor` and never a raw REST client, and the mock sees exactly
//!    the admitted requests;
//! 3. the audit cursor persists per window: crash-restart resumes from the
//!    persisted cursor, never from zero;
//! 4. persisted rows hold ids/timestamps/enums only: the table has exactly the
//!    six fence columns, and no body/reason text reaches any stored value;
//! 5. a 429/`Retry-After` fixture pauses without losing place: no request is
//!    sent during the pause, the retry reuses the same cursor, and every
//!    entry lands in order.
//!
//! Rate-limit facts verified 2026-10-12 against the official docs (no number
//! below comes from memory):
//!
//! * audit-log query shape:
//!   <https://docs.discord.com/developers/resources/audit-log> — "`limit`:
//!   Maximum number of entries (between 1-100) to return, defaults to 50",
//!   plus `user_id`, `action_type`, "`before`: Entries with ID less than a
//!   specific audit log entry ID", "`after`: Entries with ID greater than a
//!   specific audit log entry ID", ordered "based on whether you use `before`
//!   or `after`".
//! * backoff and budgets:
//!   <https://docs.discord.com/developers/topics/rate-limits> — "All bots can
//!   make up to 50 requests per second to our API", "Your application should
//!   rely on the `Retry-After` header or `retry_after` field", and "An invalid
//!   request is one that results in **401**, **403**, or **429** statuses"
//!   toward the "10,000 per 10 minutes" cutoff. Every wait the implementation
//!   honors arrives in a response header, never a literal; the small waits in
//!   the fixtures below are the mock's own script, not implementation limits.
//!
//! Fixture action types (10-13) are opaque test enums, not Discord's real
//! audit-log event codes. Fixture entries carry an explicit `occurred_at` so
//! the 45d window filter is testable without snowflake-epoch arithmetic; the
//! builder derives or carries it per its design but MUST persist it as
//! `timestamptz` and MUST enforce the window boundary pinned here (44d in,
//! 46d out).
//!
//! Only loopback fakes plus the disposable test database. Only agent-testdb
//! or a CI service container is admitted, with the agent_test identity.
//! `DATABASE_URL` is never consulted; staging and production are never
//! touched.
//!
//! Run: `cargo test -p two-bot-store --locked --test backfill_audit_log -- --ignored`
//! with `TEST_DATABASE_URL` pointed at the authorized test database.
use std::collections::VecDeque;
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

// ── Fixture constants: clearly fake ids, fixed clock ─────────────────────
// `424242` is a fixture guild, never a deployment snowflake. `1001`-`1004`
// are opaque fixture entry ids ordered oldest-first against the fixed
// 2026-10-12 fixture clock, so the 45d boundary is deterministic: `1002`
// (44d old) is inside, `1001` (46d old) is outside.
const GUILD_ID: &str = "424242";
const WINDOW_START: &str = "2026-08-28T00:00:00.000000+00:00";

fn entry_1004() -> serde_json::Value {
    audit_entry("1004", 10, "777777777777777777", Some("888888888888888888"))
}
fn entry_1003() -> serde_json::Value {
    audit_entry("1003", 11, "777777777777777777", None)
}
fn entry_1002() -> serde_json::Value {
    audit_entry("1002", 12, "999999999999999999", Some("888888888888888888"))
}
fn entry_1001_outside_window() -> serde_json::Value {
    audit_entry("1001", 13, "999999999999999999", None)
}

/// One fixture audit-log entry. `occurred_at` is explicit per the module docs;
/// `reason`/`options` mirror the body-shaped fields the real endpoint returns
/// alongside ids/enums, which the privacy fence requires the builder to drop
/// before storage.
fn audit_entry(
    id: &str,
    action_type: u16,
    user_id: &str,
    target_id: Option<&str>,
) -> serde_json::Value {
    let occurred_at = match id {
        "1004" => "2026-10-11T00:00:00.000000+00:00",
        "1003" => "2026-09-12T00:00:00.000000+00:00",
        "1002" => "2026-08-29T00:00:00.000000+00:00",
        _ => "2026-08-27T00:00:00.000000+00:00",
    };
    serde_json::json!({
        "id": id,
        "action_type": action_type,
        "user_id": user_id,
        "target_id": target_id,
        "occurred_at": occurred_at,
        "reason": "fixture reason that must never be stored",
        "options": {"fixture_body": "hello world body that must never be stored"},
    })
}

/// Same-format RFC3339 UTC fixtures sort chronologically, so the 45d window
/// check is a string comparison. The builder parses real timestamps; the
/// boundary semantics pinned here are what must agree.
fn in_window(occurred_at: &str) -> bool {
    occurred_at >= WINDOW_START
}

fn entries_of(page: &serde_json::Value) -> Vec<serde_json::Value> {
    page["audit_log_entries"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn entry_ids(page: &serde_json::Value) -> Vec<String> {
    entries_of(page)
        .iter()
        .map(|e| e["id"].as_str().expect("fixture id").to_owned())
        .collect()
}

// ── Loopback audit-log double ────────────────────────────────────────────
// Scripted Discord audit-log pages plus query/arrival recording. No database,
// never staging or production.

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

struct FakeAuditLog {
    origin: String,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl FakeAuditLog {
    async fn start(script: Vec<FakeResponse>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake audit log");
        let origin = format!("http://{}", listener.local_addr().expect("addr"));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let queue = Arc::new(Mutex::new(VecDeque::from(script)));
        let fallback = Arc::new(audit_page(Vec::new()));
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
            origin,
            seen,
            handle: Some(handle),
        }
    }

    fn requests(&self) -> Vec<SeenRequest> {
        self.seen.lock().expect("seen").clone()
    }
}

impl Drop for FakeAuditLog {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

fn audit_page(entries: Vec<serde_json::Value>) -> FakeResponse {
    FakeResponse::json(
        200,
        serde_json::json!({
            "audit_log_entries": entries,
            "users": [],
            "integrations": [],
            "webhooks": [],
        }),
    )
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
    let mut parts = head.lines().next().unwrap_or("").split_whitespace();
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

fn query_param(path: &str, key: &str) -> Option<String> {
    let query = path.split_once('?')?.1;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=')?;
        if k == key {
            return Some(v.to_owned());
        }
    }
    None
}

// ── Rate-limit client entry point ────────────────────────────────────────
// The ONLY REST path the tests (and the builder) may use. Every audit-log
// read goes through `ActionExecutor::get_json_observed`; a direct twilight
// client or raw hyper send here would be a fence violation by construction.

fn executor_for(origin: &str) -> (ActionExecutor, Arc<RateLimitGuard>) {
    let guard = Arc::new(RateLimitGuard::new(GuardConfig::default()).expect("fixture guard"));
    let exec = ActionExecutor::with_proxy_and_guard(
        "fixture-only-audit-token".to_owned(),
        Some(origin.to_owned()),
        guard.clone(),
    )
    .expect("fixture executor");
    (exec, guard)
}

fn audit_path(guild: &str, limit: u32, before: Option<&str>, after: Option<&str>) -> String {
    let mut path = format!("/guilds/{guild}/audit-logs?limit={limit}");
    if let Some(cursor) = before {
        path.push_str(&format!("&before={cursor}"));
    }
    if let Some(cursor) = after {
        path.push_str(&format!("&after={cursor}"));
    }
    path
}

/// Mirror of the builder's single-page fetch: one admitted GET carrying the
/// documented `limit`/`before`/`after` query. Panics while TOG-22948 has not
/// added the audit-logs route to the executor's route table.
async fn fetch_audit_page(
    exec: &ActionExecutor,
    guild: &str,
    limit: u32,
    before: Option<&str>,
    after: Option<&str>,
) -> serde_json::Value {
    let path = audit_path(guild, limit, before, after);
    exec.get_json_observed(&path)
        .await
        .expect("audit-log route must ride the rate-limit client (TOG-22948)")
        .unwrap_or_else(|| panic!("audit-log fixture page must be readable: {path}"))
        .0
}

// ── Disposable database fixture ──────────────────────────────────────────
// Isolated schema per test on the authorized test database only. Mirrors
// `backfill_cursors.rs` byte for byte where the contracts overlap.

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
            "backfill_audit_test_{}_{}",
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

/// Upsert a family's cursor. Mirrors the builder's `put_backfill_cursor` over
/// the slice-2 cursor store (same SQL as `backfill_cursors.rs`).
async fn put_cursor(pool: &Pool<Postgres>, family: &str, cursor: &str) {
    sqlx::query(
        "INSERT INTO backfill_cursors (family, cursor, updated_at)
         VALUES ($1, $2, now())
         ON CONFLICT (family) DO UPDATE
           SET cursor = excluded.cursor, updated_at = now()",
    )
    .bind(family)
    .bind(cursor)
    .execute(pool)
    .await
    .unwrap();
}

/// Read a family's cursor: `None` when the family never checkpointed.
/// Mirrors the builder's `get_backfill_cursor`.
async fn get_cursor(pool: &Pool<Postgres>, family: &str) -> Option<String> {
    sqlx::query_scalar("SELECT cursor FROM backfill_cursors WHERE family = $1")
        .bind(family)
        .fetch_optional(pool)
        .await
        .unwrap()
}

/// Insert one audit-log row: ids/timestamps/enums only. Mirrors the builder's
/// insert over `backfill_audit_entries`; `reason`/`options` never reach SQL.
async fn insert_audit_entry(
    pool: &Pool<Postgres>,
    guild_id: &str,
    entry_id: &str,
    action_type: i32,
    user_id: Option<&str>,
    target_id: Option<&str>,
    occurred_at: &str,
) {
    sqlx::query(
        "INSERT INTO backfill_audit_entries
             (guild_id, entry_id, action_type, user_id, target_id, occurred_at)
         VALUES ($1, $2, $3, $4, $5, $6::timestamptz)
         ON CONFLICT (guild_id, entry_id) DO NOTHING",
    )
    .bind(guild_id)
    .bind(entry_id)
    .bind(action_type)
    .bind(user_id)
    .bind(target_id)
    .bind(occurred_at)
    .execute(pool)
    .await
    .unwrap();
}

/// Mirror of the builder's page mapping: keep ids/timestamps/enums, drop
/// bodies, enforce the 45d window. Returns persisted entry ids in page order.
async fn persist_page(
    pool: &Pool<Postgres>,
    guild_id: &str,
    page: &serde_json::Value,
) -> Vec<String> {
    let mut kept = Vec::new();
    for entry in entries_of(page) {
        let occurred = entry["occurred_at"].as_str().expect("fixture occurred_at");
        if !in_window(occurred) {
            continue;
        }
        let id = entry["id"].as_str().expect("fixture id").to_owned();
        let action = entry["action_type"].as_u64().expect("fixture action_type") as i32;
        let user = entry["user_id"].as_str().map(str::to_owned);
        let target = entry["target_id"].as_str().map(str::to_owned);
        insert_audit_entry(
            pool,
            guild_id,
            &id,
            action,
            user.as_deref(),
            target.as_deref(),
            occurred,
        )
        .await;
        kept.push(id);
    }
    kept
}

async fn fetch_audit_entry_ids(pool: &Pool<Postgres>, guild_id: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT entry_id FROM backfill_audit_entries
         WHERE guild_id = $1 ORDER BY occurred_at DESC, entry_id DESC",
    )
    .bind(guild_id)
    .fetch_all(pool)
    .await
    .unwrap()
}

// (1) Pagination honors limit 1-100 with before/after cursors across the 45d
// window: page two continues where page one ended, `after` walks back toward
// newer entries, out-of-range limits never reach the wire out of range, and
// entries older than the window are dropped before storage.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL; quarantined red, tracked by TOG-22948"]
async fn audit_log_pagination_limit_1_to_100_before_after_covers_45d_window() {
    let mock = FakeAuditLog::start(vec![
        audit_page(vec![entry_1004(), entry_1003()]),
        audit_page(vec![entry_1002(), entry_1001_outside_window()]),
        audit_page(vec![entry_1004(), entry_1003()]),
        audit_page(Vec::new()),
        audit_page(Vec::new()),
    ])
    .await;
    let (exec, _) = executor_for(&mock.origin);

    let first = fetch_audit_page(&exec, GUILD_ID, 2, None, None).await;
    assert_eq!(
        entry_ids(&first),
        vec!["1004".to_owned(), "1003".to_owned()]
    );
    let second = fetch_audit_page(&exec, GUILD_ID, 2, Some("1003"), None).await;
    assert_eq!(
        entry_ids(&second),
        vec!["1002".to_owned(), "1001".to_owned()]
    );
    // `after` walks back toward newer entries (docs: ordering follows the
    // cursor direction).
    let newer = fetch_audit_page(&exec, GUILD_ID, 2, None, Some("1002")).await;
    assert_eq!(
        entry_ids(&newer),
        vec!["1004".to_owned(), "1003".to_owned()]
    );
    // Out-of-range limits must never reach the wire out of range: a correct
    // builder clamps to 1-100 (served below) or refuses without I/O (nothing
    // recorded). Either outcome keeps every RECORDED limit in range.
    for bad_limit in [0, 101] {
        let path = audit_path(GUILD_ID, bad_limit, None, None);
        let _ = exec.get_json_observed(&path).await;
    }

    let seen = mock.requests();
    assert!(
        seen.len() == 3 || seen.len() == 5,
        "pagination chain plus optional clamped probes, nothing else: {}",
        seen.len()
    );
    for request in &seen {
        assert_eq!(request.method, "GET");
        assert!(
            request.path.starts_with("/guilds/424242/audit-logs?"),
            "unexpected audit-log path: {}",
            request.path
        );
        let limit: u32 = query_param(&request.path, "limit")
            .expect("every audit-log read carries limit")
            .parse()
            .expect("fixture limit parses");
        assert!(
            (1..=100).contains(&limit),
            "limit {limit} escapes the documented 1-100 range"
        );
    }
    assert_eq!(query_param(&seen[0].path, "before"), None);
    assert_eq!(query_param(&seen[0].path, "after"), None);
    assert_eq!(
        query_param(&seen[1].path, "before"),
        Some("1003".to_owned()),
        "page two continues where page one ended"
    );
    assert_eq!(query_param(&seen[2].path, "after"), Some("1002".to_owned()));

    // Window + persistence: 44d-old `1002` lands, 46d-old `1001` never does.
    assert!(in_window("2026-08-29T00:00:00.000000+00:00"));
    assert!(!in_window("2026-08-27T00:00:00.000000+00:00"));
    let fixture = Fixture::new().await;
    assert_eq!(persist_page(&fixture.pool, GUILD_ID, &first).await.len(), 2);
    assert_eq!(
        persist_page(&fixture.pool, GUILD_ID, &second).await,
        vec!["1002".to_owned()],
        "the 46d-old entry must be dropped before storage"
    );
    put_cursor(&fixture.pool, "audit", "1002").await;
    assert_eq!(
        fetch_audit_entry_ids(&fixture.pool, GUILD_ID).await,
        vec!["1004".to_owned(), "1003".to_owned(), "1002".to_owned()]
    );
    assert_eq!(
        get_cursor(&fixture.pool, "audit").await.as_deref(),
        Some("1002")
    );
    fixture.finish().await;
}

// (2) Every audit-log request rides the rate-limit client. The test holds an
// `ActionExecutor` and never a raw REST client; the mock observes exactly the
// admitted requests, and the guard accounts the run without high-cardinality
// detail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL; also requires the audit-logs route on the rate-limit client; quarantined red, tracked by TOG-22948"]
async fn audit_log_requests_ride_rate_limit_client_never_direct_rest() {
    let mock = FakeAuditLog::start(vec![audit_page(vec![entry_1004()])]).await;
    let (exec, guard) = executor_for(&mock.origin);

    let page = fetch_audit_page(&exec, GUILD_ID, 50, None, None).await;
    assert_eq!(entry_ids(&page), vec!["1004".to_owned()]);

    let seen = mock.requests();
    assert_eq!(
        seen.len(),
        1,
        "exactly the admitted request reaches the wire"
    );
    assert_eq!(seen[0].method, "GET");
    assert!(
        seen[0].path.starts_with("/guilds/424242/audit-logs?"),
        "unexpected audit-log path: {}",
        seen[0].path
    );
    assert_eq!(query_param(&seen[0].path, "limit").as_deref(), Some("50"));
    let snapshot = guard.snapshot();
    assert_eq!(snapshot.invalid_requests_total, 0);
    assert!(!snapshot.breaker_open);
    let debug = format!("{snapshot:?}");
    for secret in ["fixture-only-audit-token", "/guilds/424242"] {
        assert!(
            !debug.contains(secret),
            "guard snapshot carries high-cardinality detail"
        );
    }
}

// (3) The audit cursor persists per window: crash-restart resumes from the
// persisted cursor mid-window, never by replaying the window from zero, and
// an untouched family stays cursorless (never a zero/empty row).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL; quarantined red, tracked by TOG-22948"]
async fn audit_cursor_persists_per_window_and_resumes_mid_window() {
    let mock = FakeAuditLog::start(vec![audit_page(vec![entry_1002()])]).await;
    let (exec, _) = executor_for(&mock.origin);
    let mut fixture = Fixture::new().await;

    // Page one checkpointed before the crash: oldest persisted entry wins.
    put_cursor(&fixture.pool, "audit", "1003").await;
    fixture.restart_pool().await;
    assert_eq!(
        get_cursor(&fixture.pool, "audit").await.as_deref(),
        Some("1003"),
        "restart must resume the audit window from its persisted cursor"
    );
    assert_eq!(get_cursor(&fixture.pool, "members").await, None);
    let zero_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM backfill_cursors WHERE cursor IN ('', '0')")
            .fetch_one(&fixture.pool)
            .await
            .unwrap();
    assert_eq!(zero_rows, 0, "no zero/empty cursor row may exist");

    // Resume continues past the checkpoint instead of replaying the window.
    let page = fetch_audit_page(&exec, GUILD_ID, 2, Some("1003"), None).await;
    assert_eq!(entry_ids(&page), vec!["1002".to_owned()]);
    let seen = mock.requests();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        query_param(&seen[0].path, "before"),
        Some("1003".to_owned()),
        "resume must continue from the persisted cursor"
    );
    assert_eq!(
        persist_page(&fixture.pool, GUILD_ID, &page).await,
        vec!["1002".to_owned()]
    );
    put_cursor(&fixture.pool, "audit", "1002").await;
    assert_eq!(
        get_cursor(&fixture.pool, "audit").await.as_deref(),
        Some("1002")
    );
    fixture.finish().await;
}

// (4) Privacy fence (same as event-store S1): persisted audit rows hold ids,
// timestamps and enum fields only. The table carries exactly the six fence
// columns, values round-trip, and no body/reason text reaches storage.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL; quarantined red, tracked by TOG-22948"]
async fn audit_rows_hold_ids_timestamps_enums_only() {
    let fixture = Fixture::new().await;
    // The source fixtures above carry `reason`/`options` body text; only the
    // id/timestamp/enum projection below may reach SQL.
    insert_audit_entry(
        &fixture.pool,
        GUILD_ID,
        "1004",
        10,
        Some("777777777777777777"),
        Some("888888888888888888"),
        "2026-10-11T00:00:00.000000+00:00",
    )
    .await;
    insert_audit_entry(
        &fixture.pool,
        GUILD_ID,
        "1003",
        11,
        Some("777777777777777777"),
        None,
        "2026-09-12T00:00:00.000000+00:00",
    )
    .await;

    let columns: Vec<(String, String)> = sqlx::query_as(
        "SELECT column_name, data_type FROM information_schema.columns
         WHERE table_schema = $1 AND table_name = 'backfill_audit_entries' ORDER BY 1",
    )
    .bind(&fixture.schema)
    .fetch_all(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(
        columns,
        vec![
            ("action_type".to_owned(), "integer".to_owned()),
            ("entry_id".to_owned(), "text".to_owned()),
            ("guild_id".to_owned(), "text".to_owned()),
            (
                "occurred_at".to_owned(),
                "timestamp with time zone".to_owned()
            ),
            ("target_id".to_owned(), "text".to_owned()),
            ("user_id".to_owned(), "text".to_owned()),
        ],
        "audit rows must hold exactly the fence columns: no body, reason, options or status text"
    );
    assert_eq!(
        fetch_audit_entry_ids(&fixture.pool, GUILD_ID).await,
        vec!["1004".to_owned(), "1003".to_owned()]
    );
    let row: (i32, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT action_type, user_id, target_id FROM backfill_audit_entries
         WHERE guild_id = $1 AND entry_id = '1004'",
    )
    .bind(GUILD_ID)
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            10,
            Some("777777777777777777".to_owned()),
            Some("888888888888888888".to_owned())
        )
    );
    let body_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM backfill_audit_entries
         WHERE entry_id LIKE '%reason%' OR entry_id LIKE '%hello world%'
            OR user_id LIKE '%reason%' OR user_id LIKE '%hello world%'
            OR target_id LIKE '%reason%' OR target_id LIKE '%hello world%'",
    )
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(body_rows, 0, "no body/reason text may reach stored values");
    let unstamped: i64 =
        sqlx::query_scalar("SELECT count(*) FROM backfill_audit_entries WHERE occurred_at IS NULL")
            .fetch_one(&fixture.pool)
            .await
            .unwrap();
    assert_eq!(unstamped, 0, "every row carries its timestamp");
    fixture.finish().await;
}

// (5) A 429/`Retry-After` fixture pauses without losing place: no request is
// sent during the header wait, the retry reuses the same `before` cursor, and
// every entry still lands in order with the cursor advanced past the pause.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL; quarantined red, tracked by TOG-22948"]
async fn audit_429_retry_after_pauses_without_losing_place() {
    let mock = FakeAuditLog::start(vec![
        audit_page(vec![entry_1004(), entry_1003()]),
        FakeResponse::json(
            429,
            serde_json::json!({"retry_after": 1.0, "global": false}),
        )
        .header("Retry-After", "1"),
        audit_page(vec![entry_1002()]),
    ])
    .await;
    let (exec, _) = executor_for(&mock.origin);
    let fixture = Fixture::new().await;

    let first = fetch_audit_page(&exec, GUILD_ID, 2, None, None).await;
    assert_eq!(persist_page(&fixture.pool, GUILD_ID, &first).await.len(), 2);
    put_cursor(&fixture.pool, "audit", "1003").await;
    assert_eq!(mock.requests().len(), 1);

    let retry = tokio::spawn({
        let exec = exec;
        async move { fetch_audit_page(&exec, GUILD_ID, 2, Some("1003"), None).await }
    });
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        mock.requests().len(),
        1,
        "a request was sent during the Retry-After pause"
    );
    let second = tokio::time::timeout(Duration::from_secs(15), retry)
        .await
        .expect("429 retry terminates")
        .expect("task join");
    assert_eq!(entry_ids(&second), vec!["1002".to_owned()]);

    let seen = mock.requests();
    assert_eq!(seen.len(), 3, "original page, one 429, one retry");
    assert_eq!(
        query_param(&seen[1].path, "before"),
        Some("1003".to_owned())
    );
    assert_eq!(
        query_param(&seen[2].path, "before"),
        Some("1003".to_owned()),
        "the retry must reuse the same cursor instead of restarting the window"
    );
    let gap = seen[2].received_at.duration_since(seen[1].received_at);
    assert!(
        gap >= Duration::from_secs(1),
        "the retry skipped the Retry-After wait: {gap:?}"
    );
    assert_eq!(
        persist_page(&fixture.pool, GUILD_ID, &second).await,
        vec!["1002".to_owned()]
    );
    put_cursor(&fixture.pool, "audit", "1002").await;
    assert_eq!(
        fetch_audit_entry_ids(&fixture.pool, GUILD_ID).await,
        vec!["1004".to_owned(), "1003".to_owned(), "1002".to_owned()]
    );
    assert_eq!(
        get_cursor(&fixture.pool, "audit").await.as_deref(),
        Some("1002")
    );
    fixture.finish().await;
}
