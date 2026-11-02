//! Failing acceptance for the backfill event-users / reaction-users / pins
//! catch-up (TOG-22943, paired failing-tests card for the TOG-22951 build).
//!
//! Test files ONLY: this target compiles green but every test FAILS until the
//! builder lands the three family fetchers plus their tables. Current failure
//! modes (all intended):
//!
//! * the REST paths below are refused without I/O by `raw_get_route`
//!   (`unsupported GET path`): event-users
//!   (`/guilds/{g}/scheduled-events/{e}/users`), reaction-users
//!   (`/channels/{c}/messages/{m}/reactions/{emoji}`) and the pins listing
//!   (`/channels/{c}/messages/pins`) have no route yet, so every
//!   `get_json_observed` pagination read returns `Err`;
//! * the family tables (`backfill_event_users`, `backfill_reaction_users`,
//!   `backfill_pins`) do not exist yet (`relation ... does not exist`), so
//!   every persist/read step fails.
//!
//! What the tests prove (TOG-22852 AC1/AC2/AC4 for these three families):
//!
//! 1. event-users pagination (max 100), reaction-users pagination and the pins
//!    listing all run **through the rate-limit client** (`get_json_observed`,
//!    with observation evidence on every page);
//! 2. blocklisted users (TOG-22850 blocklist, modelled here as a fixture set
//!    the builder must wire to the real lookup) are skipped for per-user rows
//!    while aggregate counts keep counting;
//! 3. persisted rows hold ids/timestamps/enums only (no message bodies, no
//!    custom-status text);
//! 4. resume continues from persisted `backfill_cursors` per family (the real
//!    `two_bot_store::backfill_cursors` wrapper, already landed).
//!
//! Suggested builder contract (mirrors the `backfill_cursors` card convention:
//! the builder implements exactly this SQL so wrapper and tests agree):
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS backfill_event_users (
//!   guild_id    TEXT NOT NULL,
//!   event_id    TEXT NOT NULL,
//!   user_id     TEXT NOT NULL,
//!   observed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
//!   PRIMARY KEY (guild_id, event_id, user_id)
//! );
//! CREATE TABLE IF NOT EXISTS backfill_reaction_users (
//!   channel_id  TEXT NOT NULL,
//!   message_id  TEXT NOT NULL,
//!   emoji       TEXT NOT NULL,
//!   user_id     TEXT NOT NULL,
//!   observed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
//!   PRIMARY KEY (channel_id, message_id, emoji, user_id)
//! );
//! CREATE TABLE IF NOT EXISTS backfill_pins (
//!   channel_id  TEXT NOT NULL,
//!   message_id  TEXT PRIMARY KEY,
//!   pinned_at   TIMESTAMPTZ,
//!   observed_at TIMESTAMPTZ NOT NULL DEFAULT now()
//! );
//! ```
//!
//! Endpoint facts verified 2026-10-12 (no numbers from memory; every limit the
//! implementation must honor arrives in fixtures, and tests assert `limit`
//! stays within the documented maximum):
//!
//! * event users: `GET /guilds/{guild.id}/scheduled-events/{event.id}/users`,
//!   `limit` up to maximum 100 (default 100), `before`/`after` pagination,
//!   ascending by `user_id`.
//!   Source: <https://discord.com/developers/docs/resources/guild-scheduled-event#get-guild-scheduled-event-users>
//!   (mirror: <https://github.com/awesomestickz/discord-api-docs/blob/HEAD/developers/resources/guild-scheduled-event.mdx>).
//! * reaction users: `GET /channels/{channel.id}/messages/{message.id}/reactions/{emoji}`,
//!   `limit` 1-100, `after` cursor.
//!   Source: <https://discord.com/developers/docs/resources/channel#get-reactions>.
//! * pins: `GET /channels/{channel.id}/pins` (flat array; limit raised
//!   50 -> 250) and the newer `GET /channels/{id}/messages/pins`
//!   (`{"items": [...], "has_more": bool}`). This target drives the newer
//!   shape and accepts the flat array as a fallback.
//!   Source: Discord channel/pins references via web search 2026-10-12.
//! * rate limits: per-route `X-RateLimit-*` buckets, shared global pacing and
//!   `Retry-After`/`X-RateLimit-Reset-After` sleep-then-retry.
//!   Source: <https://docs.discord.com/developers/topics/rate-limits>
//!   (2026-10-12, as pinned by `crates/discord/tests/backfill_ratelimit.rs`).
//!
//! Only agent-testdb or a CI service container is admitted, with the
//! agent_test identity. DATABASE_URL is never consulted. Loopback fixtures
//! only: no database beyond the disposable test DB, never staging or
//! production.
//!
//! Run: `cargo test -p two-bot-store --locked --test backfill_event_users_reactions_pins -- --ignored`
//! with `TEST_DATABASE_URL` pointed at the authorized test database.

use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Pool, Postgres};
use two_bot_discord::{
    ratelimit_guard::{GuardConfig, RateLimitGuard},
    ActionExecutor,
};
use two_bot_store::{migrate, DB_POOL_MAX};

// ── Fixture ids (numeric snowflakes: the executor route parser needs u64) ──

const GUILD_ID: &str = "2222";
const EVENT_ID: &str = "3333";
const CHANNEL_ID: &str = "4444";
const MESSAGE_ID: &str = "5555";
const EMOJI: &str = "fixture-emoji";

const USER_A: &str = "111111111111111111";
const USER_B: &str = "222222222222222222";
const USER_C: &str = "333333333333333333";
const BLOCKED_USER: &str = "999999999999999999";

// ── Lean loopback double (as in backfill_ratelimit.rs) ────────────────────
// Scripted HTTP server: replays a response queue while recording every
// request's method + full path (query included) on the mock side.

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
}

#[derive(Debug, Clone)]
struct SeenRequest {
    method: String,
    path: String,
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
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
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
    seen.lock()
        .expect("seen")
        .push(SeenRequest { method, path });
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

// ── Paged fetchers through the rate-limit client ───────────────────────────
// Every page goes through `get_json_observed` (paced + observed), never a raw
// socket. `limit=100` is the documented event-users maximum (verified
// 2026-10-12); reaction pages use the same ceiling, inside their 1-100 range.

fn executor_for(origin: &str) -> ActionExecutor {
    let guard = Arc::new(RateLimitGuard::new(GuardConfig::default()).expect("guard"));
    ActionExecutor::with_proxy_and_guard(
        "fixture-only-backfill-token".to_owned(),
        Some(origin.to_owned()),
        guard,
    )
    .expect("fixture executor")
}

fn event_users_path(after: Option<&str>) -> String {
    match after {
        Some(cursor) => {
            format!("/guilds/{GUILD_ID}/scheduled-events/{EVENT_ID}/users?limit=100&after={cursor}")
        }
        None => format!("/guilds/{GUILD_ID}/scheduled-events/{EVENT_ID}/users?limit=100"),
    }
}

fn reaction_users_path(after: Option<&str>) -> String {
    match after {
        Some(cursor) => format!(
            "/channels/{CHANNEL_ID}/messages/{MESSAGE_ID}/reactions/{EMOJI}?limit=100&after={cursor}"
        ),
        None => format!("/channels/{CHANNEL_ID}/messages/{MESSAGE_ID}/reactions/{EMOJI}?limit=100"),
    }
}

fn pins_path() -> String {
    format!("/channels/{CHANNEL_ID}/messages/pins")
}

fn event_user_id(row: &serde_json::Value) -> Option<String> {
    row.get("user_id")
        .and_then(|v| v.as_str())
        .or_else(|| {
            row.get("user")
                .and_then(|u| u.get("id"))
                .and_then(|v| v.as_str())
        })
        .map(|s| s.to_owned())
}

/// Fetch every event-user page through the rate-limit client. Loops until an
/// empty page; each page must carry observation evidence.
async fn fetch_all_event_users(exec: &ActionExecutor) -> Result<Vec<String>, String> {
    let mut ids = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..10 {
        let path = event_users_path(after.as_deref());
        let observed = exec
            .get_json_observed(&path)
            .await
            .map_err(|detail| format!("event-users read failed for {path}: {detail}"))?;
        let (data, evidence) =
            observed.ok_or_else(|| format!("event-users read returned no evidence for {path}"))?;
        assert!(
            !evidence.is_empty(),
            "rate-limit client must carry observation evidence"
        );
        let page: Vec<serde_json::Value> =
            serde_json::from_value(data).map_err(|e| format!("event-users page decode: {e}"))?;
        if page.is_empty() {
            return Ok(ids);
        }
        for row in &page {
            ids.push(
                event_user_id(row)
                    .ok_or_else(|| format!("event-users row lacks user id: {row}"))?,
            );
        }
        after = ids.last().cloned();
    }
    Err("event-users pagination did not terminate".to_owned())
}

async fn fetch_all_reaction_users(exec: &ActionExecutor) -> Result<Vec<String>, String> {
    let mut ids = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..10 {
        let path = reaction_users_path(after.as_deref());
        let observed = exec
            .get_json_observed(&path)
            .await
            .map_err(|detail| format!("reaction-users read failed for {path}: {detail}"))?;
        let (data, evidence) = observed
            .ok_or_else(|| format!("reaction-users read returned no evidence for {path}"))?;
        assert!(
            !evidence.is_empty(),
            "rate-limit client must carry observation evidence"
        );
        let page: Vec<serde_json::Value> =
            serde_json::from_value(data).map_err(|e| format!("reaction-users page decode: {e}"))?;
        if page.is_empty() {
            return Ok(ids);
        }
        for row in &page {
            ids.push(
                row.get("id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| format!("reaction-users row lacks id: {row}"))?
                    .to_owned(),
            );
        }
        after = ids.last().cloned();
    }
    Err("reaction-users pagination did not terminate".to_owned())
}

/// A fetched pin: Discord message id plus its optional `pinned_at` instant.
/// No body, no content — metadata only.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FetchedPin {
    message_id: String,
    pinned_at: Option<String>,
}

fn parse_pins_page(data: &serde_json::Value) -> Result<(Vec<FetchedPin>, bool), String> {
    // New shape first: {"items": [...], "has_more": bool}.
    if let Some(obj) = data.as_object() {
        if let Some(items) = obj.get("items") {
            let has_more = obj
                .get("has_more")
                .and_then(|v| v.as_bool())
                .ok_or_else(|| format!("pins page lacks has_more: {data}"))?;
            let raw: Vec<serde_json::Value> = serde_json::from_value(items.clone())
                .map_err(|e| format!("pins items decode: {e}"))?;
            let mut out = Vec::new();
            for item in &raw {
                // New shape nests under "message"; accept a flat row too.
                let msg = item.get("message").unwrap_or(item);
                out.push(FetchedPin {
                    message_id: msg
                        .get("id")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| format!("pins row lacks id: {item}"))?
                        .to_owned(),
                    pinned_at: msg
                        .get("pinned_at")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_owned()),
                });
            }
            return Ok((out, has_more));
        }
    }
    // Old shape fallback: a flat array of message objects.
    if let Some(raw) = data.as_array() {
        let mut out = Vec::new();
        for item in raw {
            out.push(FetchedPin {
                message_id: item
                    .get("id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| format!("pins row lacks id: {item}"))?
                    .to_owned(),
                pinned_at: item
                    .get("pinned_at")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_owned()),
            });
        }
        return Ok((out, false));
    }
    Err(format!("pins page has unknown shape: {data}"))
}

async fn fetch_all_pins(exec: &ActionExecutor) -> Result<Vec<FetchedPin>, String> {
    let path = pins_path();
    let mut pins = Vec::new();
    for _ in 0..10 {
        let observed = exec
            .get_json_observed(&path)
            .await
            .map_err(|detail| format!("pins read failed for {path}: {detail}"))?;
        let (data, evidence) =
            observed.ok_or_else(|| format!("pins read returned no evidence for {path}"))?;
        assert!(
            !evidence.is_empty(),
            "rate-limit client must carry observation evidence"
        );
        let (page, has_more) = parse_pins_page(&data)?;
        pins.extend(page);
        if !has_more {
            return Ok(pins);
        }
    }
    Err("pins listing did not terminate".to_owned())
}

// ── Persist helpers (mirror the builder's SQL byte for byte) ───────────────
// Privacy fence: ids/timestamps/enums only. No message bodies, no
// custom-status text, no payload columns.

async fn put_event_user(pool: &Pool<Postgres>, user_id: &str) {
    sqlx::query(
        "INSERT INTO backfill_event_users (guild_id, event_id, user_id, observed_at)
         VALUES ($1, $2, $3, now())
         ON CONFLICT (guild_id, event_id, user_id) DO NOTHING",
    )
    .bind(GUILD_ID)
    .bind(EVENT_ID)
    .bind(user_id)
    .execute(pool)
    .await
    .unwrap();
}

async fn put_reaction_user(pool: &Pool<Postgres>, user_id: &str) {
    sqlx::query(
        "INSERT INTO backfill_reaction_users (channel_id, message_id, emoji, user_id, observed_at)
         VALUES ($1, $2, $3, $4, now())
         ON CONFLICT (channel_id, message_id, emoji, user_id) DO NOTHING",
    )
    .bind(CHANNEL_ID)
    .bind(MESSAGE_ID)
    .bind(EMOJI)
    .bind(user_id)
    .execute(pool)
    .await
    .unwrap();
}

async fn put_pin(pool: &Pool<Postgres>, pin: &FetchedPin) {
    sqlx::query(
        "INSERT INTO backfill_pins (channel_id, message_id, pinned_at, observed_at)
         VALUES ($1, $2, $3::timestamptz, now())
         ON CONFLICT (message_id) DO NOTHING",
    )
    .bind(CHANNEL_ID)
    .bind(pin.message_id.as_str())
    .bind(pin.pinned_at.as_deref())
    .execute(pool)
    .await
    .unwrap();
}

// ── Disposable-DB fixture (as in backfill_cursors.rs) ──────────────────────

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
            "backfill_users_pins_test_{}_{}",
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

    async fn restart_pool(&mut self) {
        self.pool.close().await;
        self.pool = Self::connect(&self.url, &self.schema).await;
    }

    async fn finish(self) {
        self.pool.close().await;
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

// ── Fixture pages ──────────────────────────────────────────────────────────

fn event_user_row(id: &str) -> serde_json::Value {
    serde_json::json!({
        "user_id": id,
        "user": {"id": id},
        "guild_scheduled_event_id": EVENT_ID,
    })
}

fn reaction_user_row(id: &str) -> serde_json::Value {
    serde_json::json!({"id": id, "username": format!("fixture-{id}")})
}

fn pin_item(id: &str, pinned_at: &str) -> serde_json::Value {
    serde_json::json!({"message": {"id": id, "pinned_at": pinned_at}})
}

fn limit_of(path: &str) -> Option<u64> {
    path.split('?')
        .nth(1)?
        .split('&')
        .find_map(|pair| pair.strip_prefix("limit=")?.parse::<u64>().ok())
}

// (1a) Event-users pagination runs through the rate-limit client with max 100.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL; quarantined red, tracked by TOG-22951"]
async fn event_users_paginate_through_rate_limit_client_with_max_100() {
    let mock = FakeRest::start(
        vec![
            FakeResponse::json(
                200,
                serde_json::json!([event_user_row(USER_A), event_user_row(USER_B)]),
            ),
            FakeResponse::json(200, serde_json::json!([event_user_row(USER_C)])),
            FakeResponse::json(200, serde_json::json!([])),
        ],
        FakeResponse::json(200, serde_json::json!([])),
    )
    .await;
    let exec = executor_for(&mock.origin());

    let ids = fetch_all_event_users(&exec)
        .await
        .expect("event-users catch-up through the rate-limit client");

    assert_eq!(
        ids,
        vec![USER_A.to_owned(), USER_B.to_owned(), USER_C.to_owned()]
    );

    // Pagination proof: three paced reads, every one capped at the documented
    // maximum, follow-ups carrying the last-seen id as `after`.
    let seen = mock.requests();
    assert_eq!(seen.len(), 3, "event-users must paginate across pages");
    assert!(seen.iter().all(|r| r.method == "GET"), "reads only");
    for request in &seen {
        let limit = limit_of(&request.path)
            .unwrap_or_else(|| panic!("event-users read lacks limit: {}", request.path));
        assert!(
            limit <= 100,
            "event-users read exceeds the documented max 100: {}",
            request.path
        );
        assert!(
            request.path.contains(&format!(
                "/guilds/{GUILD_ID}/scheduled-events/{EVENT_ID}/users"
            )),
            "event-users read hit an unexpected route: {}",
            request.path
        );
    }
    assert!(
        seen[1].path.contains(&format!("after={USER_B}")),
        "second page must resume after the first page: {}",
        seen[1].path
    );
    assert!(
        seen[2].path.contains(&format!("after={USER_C}")),
        "terminating read must resume after the last id: {}",
        seen[2].path
    );

    // End-to-end: the fetched ids persist as metadata-only rows.
    let f = Fixture::new().await;
    for id in &ids {
        put_event_user(&f.pool, id).await;
    }
    let stored: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM backfill_event_users WHERE guild_id = $1 AND event_id = $2",
    )
    .bind(GUILD_ID)
    .bind(EVENT_ID)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(stored, 3);
    f.finish().await;
    mock.shutdown().await;
}

// (1b) Reaction-users pagination runs through the rate-limit client.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL; quarantined red, tracked by TOG-22951"]
async fn reaction_users_paginate_through_rate_limit_client() {
    let mock = FakeRest::start(
        vec![
            FakeResponse::json(
                200,
                serde_json::json!([reaction_user_row(USER_A), reaction_user_row(USER_B)]),
            ),
            FakeResponse::json(200, serde_json::json!([reaction_user_row(USER_C)])),
            FakeResponse::json(200, serde_json::json!([])),
        ],
        FakeResponse::json(200, serde_json::json!([])),
    )
    .await;
    let exec = executor_for(&mock.origin());

    let ids = fetch_all_reaction_users(&exec)
        .await
        .expect("reaction-users catch-up through the rate-limit client");

    assert_eq!(
        ids,
        vec![USER_A.to_owned(), USER_B.to_owned(), USER_C.to_owned()]
    );

    let seen = mock.requests();
    assert_eq!(seen.len(), 3, "reaction-users must paginate across pages");
    assert!(seen.iter().all(|r| r.method == "GET"), "reads only");
    for request in &seen {
        let limit = limit_of(&request.path)
            .unwrap_or_else(|| panic!("reaction-users read lacks limit: {}", request.path));
        assert!(
            limit <= 100,
            "reaction-users read exceeds the documented 1-100 range: {}",
            request.path
        );
        assert!(
            request.path.contains(&format!(
                "/channels/{CHANNEL_ID}/messages/{MESSAGE_ID}/reactions/"
            )),
            "reaction-users read hit an unexpected route: {}",
            request.path
        );
    }
    assert!(
        seen[1].path.contains(&format!("after={USER_B}")),
        "second page must resume after the first page: {}",
        seen[1].path
    );

    let f = Fixture::new().await;
    for id in &ids {
        put_reaction_user(&f.pool, id).await;
    }
    let stored: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM backfill_reaction_users WHERE channel_id = $1 AND message_id = $2",
    )
    .bind(CHANNEL_ID)
    .bind(MESSAGE_ID)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(stored, 3);
    f.finish().await;
    mock.shutdown().await;
}

// (1c) Pins listing runs through the rate-limit client and persists metadata.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL; quarantined red, tracked by TOG-22951"]
async fn pins_list_through_rate_limit_client_persists_metadata_only() {
    let mock = FakeRest::start(
        vec![
            FakeResponse::json(
                200,
                serde_json::json!({
                    "items": [
                        pin_item("666666666666666666", "2026-10-01T00:00:00Z"),
                        pin_item("777777777777777777", "2026-10-02T00:00:00Z"),
                    ],
                    "has_more": true,
                }),
            ),
            FakeResponse::json(
                200,
                serde_json::json!({
                    "items": [pin_item("888888888888888888", "2026-10-03T00:00:00Z")],
                    "has_more": false,
                }),
            ),
        ],
        FakeResponse::json(200, serde_json::json!({"items": [], "has_more": false})),
    )
    .await;
    let exec = executor_for(&mock.origin());

    let pins = fetch_all_pins(&exec)
        .await
        .expect("pins catch-up through the rate-limit client");

    assert_eq!(
        pins,
        vec![
            FetchedPin {
                message_id: "666666666666666666".to_owned(),
                pinned_at: Some("2026-10-01T00:00:00Z".to_owned()),
            },
            FetchedPin {
                message_id: "777777777777777777".to_owned(),
                pinned_at: Some("2026-10-02T00:00:00Z".to_owned()),
            },
            FetchedPin {
                message_id: "888888888888888888".to_owned(),
                pinned_at: Some("2026-10-03T00:00:00Z".to_owned()),
            },
        ]
    );

    let seen = mock.requests();
    assert_eq!(seen.len(), 2, "pins must follow has_more across pages");
    assert!(seen.iter().all(|r| r.method == "GET"), "reads only");
    assert!(
        seen.iter().all(|r| r.path.contains("/messages/pins")),
        "pins reads hit an unexpected route: {:?}",
        seen.iter().map(|r| &r.path).collect::<Vec<_>>()
    );

    // Metadata only: ids + instants persist; there is nowhere for a body to go
    // (the schema test below pins the exact columns).
    let f = Fixture::new().await;
    for pin in &pins {
        put_pin(&f.pool, pin).await;
    }
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT message_id, channel_id FROM backfill_pins WHERE channel_id = $1 ORDER BY 1",
    )
    .bind(CHANNEL_ID)
    .fetch_all(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![
            ("666666666666666666".to_owned(), CHANNEL_ID.to_owned()),
            ("777777777777777777".to_owned(), CHANNEL_ID.to_owned()),
            ("888888888888888888".to_owned(), CHANNEL_ID.to_owned()),
        ]
    );
    f.finish().await;
    mock.shutdown().await;
}

// (2) Blocklisted users are skipped for per-user rows; aggregates keep counting.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL; quarantined red, tracked by TOG-22951"]
async fn blocklisted_users_skipped_for_per_user_rows_while_aggregates_keep_counting() {
    // Fixture blocklist modelling the TOG-22850 opt-out: the builder must wire
    // this predicate to the real blocklist lookup. The aggregate (total seen)
    // counts everyone; only per-user row writes skip the blocked id.
    let blocklisted: HashSet<String> = [BLOCKED_USER.to_owned()].into_iter().collect();

    let mock = FakeRest::start(
        vec![
            FakeResponse::json(
                200,
                serde_json::json!([event_user_row(USER_A), event_user_row(BLOCKED_USER)]),
            ),
            FakeResponse::json(200, serde_json::json!([])),
        ],
        FakeResponse::json(200, serde_json::json!([])),
    )
    .await;
    let exec = executor_for(&mock.origin());
    let event_ids = fetch_all_event_users(&exec)
        .await
        .expect("event-users catch-up for the blocklist case");

    let reaction_mock = FakeRest::start(
        vec![
            FakeResponse::json(
                200,
                serde_json::json!([reaction_user_row(USER_B), reaction_user_row(BLOCKED_USER)]),
            ),
            FakeResponse::json(200, serde_json::json!([])),
        ],
        FakeResponse::json(200, serde_json::json!([])),
    )
    .await;
    let reaction_exec = executor_for(&reaction_mock.origin());
    let reaction_ids = fetch_all_reaction_users(&reaction_exec)
        .await
        .expect("reaction-users catch-up for the blocklist case");

    // Aggregates count every id the API returned, blocked included.
    assert_eq!(event_ids.len(), 2);
    assert_eq!(reaction_ids.len(), 2);

    let f = Fixture::new().await;
    let mut event_aggregate = 0usize;
    for id in &event_ids {
        event_aggregate += 1;
        if blocklisted.contains(id) {
            continue;
        }
        put_event_user(&f.pool, id).await;
    }
    let mut reaction_aggregate = 0usize;
    for id in &reaction_ids {
        reaction_aggregate += 1;
        if blocklisted.contains(id) {
            continue;
        }
        put_reaction_user(&f.pool, id).await;
    }
    assert_eq!(
        event_aggregate, 2,
        "event aggregate must count the blocked user"
    );
    assert_eq!(
        reaction_aggregate, 2,
        "reaction aggregate must count the blocked user"
    );

    let event_rows: Vec<String> = sqlx::query_scalar(
        "SELECT user_id FROM backfill_event_users WHERE guild_id = $1 AND event_id = $2 ORDER BY 1",
    )
    .bind(GUILD_ID)
    .bind(EVENT_ID)
    .fetch_all(&f.pool)
    .await
    .unwrap();
    assert_eq!(event_rows, vec![USER_A.to_owned()]);
    let reaction_rows: Vec<String> = sqlx::query_scalar(
        "SELECT user_id FROM backfill_reaction_users WHERE channel_id = $1 AND message_id = $2 ORDER BY 1",
    )
    .bind(CHANNEL_ID)
    .bind(MESSAGE_ID)
    .fetch_all(&f.pool)
    .await
    .unwrap();
    assert_eq!(reaction_rows, vec![USER_B.to_owned()]);

    f.finish().await;
    mock.shutdown().await;
    reaction_mock.shutdown().await;
}

// (3) Persisted rows hold ids/timestamps/enums only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL; quarantined red, tracked by TOG-22951"]
async fn persisted_rows_hold_ids_timestamps_enums_only() {
    let f = Fixture::new().await;
    put_event_user(&f.pool, USER_A).await;
    put_reaction_user(&f.pool, USER_B).await;
    put_pin(
        &f.pool,
        &FetchedPin {
            message_id: "666666666666666666".to_owned(),
            pinned_at: Some("2026-10-01T00:00:00Z".to_owned()),
        },
    )
    .await;

    // Exact column pins: any body/status-text/payload column fails this.
    for (table, expected) in [
        (
            "backfill_event_users",
            vec![
                ("event_id".to_owned(), "text".to_owned()),
                ("guild_id".to_owned(), "text".to_owned()),
                (
                    "observed_at".to_owned(),
                    "timestamp with time zone".to_owned(),
                ),
                ("user_id".to_owned(), "text".to_owned()),
            ],
        ),
        (
            "backfill_reaction_users",
            vec![
                ("channel_id".to_owned(), "text".to_owned()),
                ("emoji".to_owned(), "text".to_owned()),
                ("message_id".to_owned(), "text".to_owned()),
                (
                    "observed_at".to_owned(),
                    "timestamp with time zone".to_owned(),
                ),
                ("user_id".to_owned(), "text".to_owned()),
            ],
        ),
        (
            "backfill_pins",
            vec![
                ("channel_id".to_owned(), "text".to_owned()),
                ("message_id".to_owned(), "text".to_owned()),
                (
                    "observed_at".to_owned(),
                    "timestamp with time zone".to_owned(),
                ),
                (
                    "pinned_at".to_owned(),
                    "timestamp with time zone".to_owned(),
                ),
            ],
        ),
    ] {
        let columns: Vec<(String, String)> = sqlx::query_as(
            "SELECT column_name, data_type FROM information_schema.columns
             WHERE table_schema = $1 AND table_name = $2 ORDER BY 1",
        )
        .bind(&f.schema)
        .bind(table)
        .fetch_all(&f.pool)
        .await
        .unwrap();
        assert_eq!(columns, expected, "{table} must hold ids/timestamps only");
    }

    // Round-trips are opaque ids, never bodies.
    let user: String = sqlx::query_scalar(
        "SELECT user_id FROM backfill_event_users WHERE guild_id = $1 AND event_id = $2",
    )
    .bind(GUILD_ID)
    .bind(EVENT_ID)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(user, USER_A);
    f.finish().await;
}

// (4) Resume continues from persisted cursors per family.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL; quarantined red, tracked by TOG-22951"]
async fn resume_continues_from_persisted_cursors_per_family() {
    use two_bot_store::backfill_cursors::{get_backfill_cursor, put_backfill_cursor};

    let mut f = Fixture::new().await;
    put_backfill_cursor(&f.pool, "event_users", USER_A)
        .await
        .expect("checkpoint event_users");
    put_backfill_cursor(&f.pool, "reactions", USER_B)
        .await
        .expect("checkpoint reactions");
    put_backfill_cursor(&f.pool, "pins", "666666666666666666")
        .await
        .expect("checkpoint pins");

    // Crash-restart: only durably persisted cursors survive.
    f.restart_pool().await;
    assert_eq!(
        get_backfill_cursor(&f.pool, "event_users").await.unwrap(),
        Some(USER_A.to_owned())
    );

    // The resumed event-users fetch starts after the persisted cursor: the
    // fake serves the post-cursor page, and the first request must carry it.
    let mock = FakeRest::start(
        vec![
            FakeResponse::json(200, serde_json::json!([event_user_row(USER_C)])),
            FakeResponse::json(200, serde_json::json!([])),
        ],
        FakeResponse::json(200, serde_json::json!([])),
    )
    .await;
    let exec = executor_for(&mock.origin());
    let cursor = get_backfill_cursor(&f.pool, "event_users")
        .await
        .unwrap()
        .expect("event_users must have a persisted cursor");
    assert_eq!(cursor, USER_A);

    // Resume manually from the cursor: the first read carries `after=<cursor>`.
    let path = event_users_path(Some(&cursor));
    assert!(path.contains(&format!("after={USER_A}")));
    let observed = exec
        .get_json_observed(&path)
        .await
        .expect("resumed read through the rate-limit client")
        .expect("resumed read carries evidence");
    let page: Vec<serde_json::Value> =
        serde_json::from_value(observed.0).expect("resumed page decodes");
    assert_eq!(page.len(), 1);
    assert_eq!(event_user_id(&page[0]).as_deref(), Some(USER_C));

    // Checkpoint the new cursor; the other families are untouched.
    put_backfill_cursor(&f.pool, "event_users", USER_C)
        .await
        .expect("advance event_users");
    assert_eq!(
        get_backfill_cursor(&f.pool, "event_users").await.unwrap(),
        Some(USER_C.to_owned()),
        "restart must resume event_users from the new cursor, never zero"
    );
    assert_eq!(
        get_backfill_cursor(&f.pool, "reactions").await.unwrap(),
        Some(USER_B.to_owned()),
        "advancing event_users must not clobber reactions"
    );
    assert_eq!(
        get_backfill_cursor(&f.pool, "pins").await.unwrap(),
        Some("666666666666666666".to_owned()),
        "advancing event_users must not clobber pins"
    );

    let seen = mock.requests();
    assert_eq!(seen.len(), 1);
    assert!(
        seen[0].path.contains(&format!("after={USER_A}")),
        "resumed fetch must start from the persisted cursor: {}",
        seen[0].path
    );

    f.finish().await;
    mock.shutdown().await;
}
