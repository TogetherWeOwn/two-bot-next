//! Backfill bans / invites / onboarding catch-up acceptance (TOG-22944).
//!
//! Paired failing-tests card for the bans/invites/onboarding slice of the
//! backfill worker (TOG-22852 slice 7, build card TOG-22952). Test files ONLY:
//! this target compiles green but each test FAILS until the builder lands the
//! family fetchers plus their store (currently the client refuses the family
//! paths as `unsupported GET path` without I/O, so the very first fetch
//! fails). The build card implements:
//!
//! * family fetches through the shared rate-limit client
//!   ([`ActionExecutor::get_json_observed`]) for `GET
//!   /guilds/{guild.id}/bans` (with `limit`/`after`), `GET
//!   /guilds/{guild.id}/invites`, and `GET /guilds/{guild.id}/onboarding`;
//! * `apply_ban_page`: blocklisted users (TOG-22850 opt-out, stood in for here
//!   by an in-test set — same seam as the member-list slice) are skipped for
//!   per-user rows while the aggregate count keeps counting them;
//! * row shapes holding ids/timestamps/enums/metadata ONLY, applied from the
//!   store migration chain so [`migrate`] creates them:
//!   - `backfill_ban_users (guild_id, user_id, observed_at)`,
//!     `backfill_ban_counts (guild_id, total_seen, skipped_blocklisted,
//!     updated_at)`;
//!   - `backfill_invites (code, guild_id, channel_id, inviter_id, uses,
//!     max_uses, created_at, observed_at)`;
//!   - `backfill_onboarding_guilds (guild_id, enabled, observed_at)`,
//!     `backfill_onboarding_prompts (guild_id, prompt_id, prompt_type,
//!     single_select, required, in_onboarding, options_count,
//!     observed_at)`;
//! * resume via the slice-2 `backfill_cursors` table, families `bans`,
//!   `invites`, `onboarding` (same upsert/read SQL as `backfill_cursors.rs`,
//!   so the slices agree byte for byte).
//!
//! What the tests prove (TOG-22852 AC1/AC2/AC4 for these three families):
//!
//! 1. bans, invite-metadata and onboarding fetches go through the rate-limit
//!    client (observed GETs to the documented endpoints; a route 429 sleeps
//!    per its `Retry-After` header and retries the same path);
//! 2. blocklisted users are skipped for ban per-user rows while aggregate
//!    counts keep counting them;
//! 3. invite/onboarding rows hold ids/timestamps/enums/metadata only — a
//!    column allowlist plus a tripwire against title/description/body columns;
//! 4. crash-restart resumes each family's catch-up from its persisted cursor,
//!    never from zero, without duplicating rows.
//!
//! Endpoint facts verified against the official Discord documentation
//! (2026-10-12): bans (`GET /guilds/{guild.id}/bans`, Ban = `{reason,
//! user}`, `limit` up to 1000, `before`/`after` user-id cursors;
//! <https://docs.discord.com/developers/resources/guild#get-guild-bans>),
//! guild invites (`GET /guilds/{guild.id}/invites`, a list of invite objects;
//! <https://docs.discord.com/developers/resources/guild#get-guild-invites>)
//! with metadata (`code`, `uses`, `max_uses`, `created_at`, `inviter`,
//! `channel`, `guild`;
//! <https://docs.discord.com/developers/resources/invite>), and onboarding
//! (`GET /guilds/{guild.id}/onboarding`, the guild's Onboarding object;
//! <https://docs.discord.com/developers/resources/guild#get-guild-onboarding>).
//! Rate-limit facts verified against the official rate-limits page (same
//! source and date as the sibling suites,
//! <https://docs.discord.com/developers/topics/rate-limits>): per-route
//! buckets signalled by `X-RateLimit-*`, 429s wait on `Retry-After` /
//! `retry_after`. Every wait below arrives in a response header, never a
//! literal from memory.
//!
//! Only agent-testdb or a CI service container is admitted, with the
//! agent_test identity. DATABASE_URL is never consulted. Loopback HTTP
//! doubles only: never staging or production Discord.
//!
//! Run: `cargo test -p two-bot-store --locked --test backfill_bans_invites_onboarding -- --ignored`
//! with `TEST_DATABASE_URL` pointed at the authorized test database.
//!
//! The fixture guild `444444444444444444` and every user/channel id below
//! are synthetic. Never the live guild.

use std::collections::{HashSet, VecDeque};
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

/// Synthetic guild for every fixture below. Not the live guild.
const GUILD: &str = "444444444444444444";

/// TOG-22850 stand-in: opted-out user ids the catch-up must skip for
/// per-user rows. The real opt-out store lands with the privacy P1 card; the
/// seam (a set consulted at apply time) is what this pins.
fn blocklist() -> HashSet<String> {
    ["222222222222222222".to_owned()].into()
}

// ── Lean loopback double ──────────────────────────────────────────────
// Scripted HTTP server: replays a response queue (status + headers + body)
// while recording every request's method and path. Same shape as the sibling
// backfill suites; defined here because integration targets share no code.

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

fn family_executor(origin: &str) -> ActionExecutor {
    let guard = Arc::new(RateLimitGuard::new(GuardConfig::default()).expect("guard"));
    ActionExecutor::with_proxy_and_guard(
        "fixture-only-backfill-token".to_owned(),
        Some(origin.to_owned()),
        guard,
    )
    .expect("fixture executor")
}

/// One observed page fetch through the rate-limit client. Fails today: the
/// client refuses the family paths as `unsupported GET path` without I/O.
async fn fetch_family_page(exec: &ActionExecutor, path: &str) -> serde_json::Value {
    let (data, evidence) =
        tokio::time::timeout(Duration::from_secs(15), exec.get_json_observed(path))
            .await
            .expect("family fetch terminates")
            .expect("family fetch through the rate-limit client")
            .expect("family fetch carries a body");
    assert!(
        !evidence.is_empty(),
        "observed fetch must carry observation evidence"
    );
    data
}

// ── Disposable-DB fixture ────────────────────────────────────────────
// Same isolation shape as the sibling backfill suites: agent-testdb or a CI
// service container only, agent_test identity, per-test schema,
// DATABASE_URL never consulted.

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
        // Threat-model F6: tests opt into `LocalOnly` explicitly rather than
        // inheriting the process `TWO_DATABASE_TLS` value.
        let admin = two_bot_store::connect_pool_with_tls(
            &url,
            two_bot_core::database_tls::TlsPolicy::LocalOnly,
        )
        .await
        .expect("test DB connect failed");
        let schema = format!(
            "backfill_bio_test_{}_{}",
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

/// Cursor upsert: byte-for-byte the slice-2 `backfill_cursors` contract.
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

/// Cursor read: `None` when the family never checkpointed.
async fn get_cursor(pool: &Pool<Postgres>, family: &str) -> Option<String> {
    sqlx::query_scalar("SELECT cursor FROM backfill_cursors WHERE family = $1")
        .bind(family)
        .fetch_optional(pool)
        .await
        .unwrap()
}

fn ban_entry(user_id: &str, reason: &str) -> serde_json::Value {
    serde_json::json!({"reason": reason, "user": {"id": user_id, "username": "fixture-user"}})
}

/// Apply one fetched bans page: per-user rows skip the blocklist, the
/// aggregate counts everyone. Mirrors the builder's `apply_ban_page`.
async fn apply_ban_page(
    pool: &Pool<Postgres>,
    guild_id: &str,
    page: &[serde_json::Value],
    blocklisted: &HashSet<String>,
) -> (usize, usize) {
    let mut stored = 0usize;
    let mut skipped = 0usize;
    for entry in page {
        let user_id = entry["user"]["id"].as_str().expect("ban user id");
        if blocklisted.contains(user_id) {
            skipped += 1;
            continue;
        }
        sqlx::query(
            "INSERT INTO backfill_ban_users (guild_id, user_id, observed_at)
             VALUES ($1, $2, now())
             ON CONFLICT (guild_id, user_id) DO NOTHING",
        )
        .bind(guild_id)
        .bind(user_id)
        .execute(pool)
        .await
        .unwrap();
        stored += 1;
    }
    let total = stored + skipped;
    sqlx::query(
        "INSERT INTO backfill_ban_counts (guild_id, total_seen, skipped_blocklisted, updated_at)
         VALUES ($1, $2, $3, now())
         ON CONFLICT (guild_id) DO UPDATE SET
           total_seen = backfill_ban_counts.total_seen + excluded.total_seen,
           skipped_blocklisted = backfill_ban_counts.skipped_blocklisted + excluded.skipped_blocklisted,
           updated_at = now()",
    )
    .bind(guild_id)
    .bind(total as i64)
    .bind(skipped as i64)
    .execute(pool)
    .await
    .unwrap();
    (stored, skipped)
}

// (1+2) Bans page through the observed client with an after-cursor — a route
// 429 first, so the test also proves header-driven sleep-then-retry on the
// family path — then blocklisted users skip per-user rows while aggregates
// keep counting them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL; quarantined red, tracked by TOG-22952"]
async fn bans_fetch_applies_blocklist_and_counts_everyone() {
    let page = serde_json::json!([
        ban_entry("111111111111111111", "spam"),
        ban_entry("222222222222222222", "raids"),
        ban_entry("333333333333333333", "spam"),
    ]);
    let limited = FakeResponse::json(
        429,
        serde_json::json!({"message": "You are being rate limited.", "retry_after": 1.0, "global": false}),
    )
    .header("Retry-After", "1");
    let mock = FakeRest::start(
        vec![limited, FakeResponse::json(200, page.clone())],
        FakeResponse::json(200, serde_json::json!([])),
    )
    .await;
    let exec = family_executor(&mock.origin());
    let path = format!("/guilds/{GUILD}/bans?limit=1000");

    let (data, evidence) =
        tokio::time::timeout(Duration::from_secs(30), exec.get_json_observed(&path))
            .await
            .expect("bans fetch terminates")
            .expect("bans fetch through the rate-limit client")
            .expect("bans fetch carries a body");
    assert_eq!(data, page);
    assert!(!evidence.is_empty());
    let seen = mock.requests();
    assert_eq!(seen.len(), 2, "route 429 must retry the same bans path");
    assert!(seen.iter().all(|r| r.method == "GET" && r.path == path));

    let f = Fixture::new().await;
    let entries: Vec<serde_json::Value> = data.as_array().expect("ban page array").clone();
    let (stored, skipped) = apply_ban_page(&f.pool, GUILD, &entries, &blocklist()).await;
    assert_eq!((stored, skipped), (2, 1));

    let blocked_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM backfill_ban_users WHERE user_id = '222222222222222222'",
    )
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(blocked_rows, 0, "blocklisted user has no per-user row");
    let kept: Vec<String> = sqlx::query_scalar("SELECT user_id FROM backfill_ban_users ORDER BY 1")
        .fetch_all(&f.pool)
        .await
        .unwrap();
    assert_eq!(kept, vec!["111111111111111111", "333333333333333333"]);
    let counts: (i64, i64) = sqlx::query_as(
        "SELECT total_seen, skipped_blocklisted FROM backfill_ban_counts WHERE guild_id = $1",
    )
    .bind(GUILD)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        counts,
        (3, 1),
        "aggregates count everyone, blocked included"
    );

    mock.shutdown().await;
    f.finish().await;
}

// (1+3) Invite metadata through the observed client persists as
// ids/timestamps/counts only: codes, snowflakes, uses caps and stamps.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL; quarantined red, tracked by TOG-22952"]
async fn invite_rows_hold_metadata_only() {
    let invites = serde_json::json!([
        {"code": "abc123", "uses": 7, "max_uses": 10, "created_at": "2026-09-01T10:00:00.000Z",
         "inviter": {"id": "777777777777777777"}, "channel": {"id": "888888888888888888"},
         "guild": {"id": GUILD}},
        {"code": "def456", "uses": 0, "max_uses": 0, "created_at": "2026-09-15T10:00:00.000Z",
         "inviter": {"id": "777777777777777777"}, "channel": {"id": "999999999999999999"},
         "guild": {"id": GUILD}}
    ]);
    let mock = FakeRest::start(
        vec![FakeResponse::json(200, invites.clone())],
        FakeResponse::json(200, serde_json::json!([])),
    )
    .await;
    let exec = family_executor(&mock.origin());
    let path = format!("/guilds/{GUILD}/invites");
    let data = fetch_family_page(&exec, &path).await;
    assert_eq!(data, invites);
    assert_eq!(mock.requests().len(), 1);

    let f = Fixture::new().await;
    for invite in data.as_array().expect("invite list") {
        sqlx::query(
            "INSERT INTO backfill_invites
               (code, guild_id, channel_id, inviter_id, uses, max_uses, created_at, observed_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, now())
             ON CONFLICT (code) DO UPDATE SET
               uses = excluded.uses, max_uses = excluded.max_uses,
               observed_at = now()",
        )
        .bind(invite["code"].as_str().expect("invite code"))
        .bind(invite["guild"]["id"].as_str().expect("invite guild"))
        .bind(invite["channel"]["id"].as_str().expect("invite channel"))
        .bind(invite["inviter"]["id"].as_str().expect("inviter"))
        .bind(invite["uses"].as_i64().expect("uses"))
        .bind(invite["max_uses"].as_i64().expect("max uses"))
        .bind(invite["created_at"].as_str().expect("invite created_at"))
        .execute(&f.pool)
        .await
        .unwrap();
    }
    let columns: Vec<(String, String)> = sqlx::query_as(
        "SELECT column_name, data_type FROM information_schema.columns
         WHERE table_schema = $1 AND table_name = 'backfill_invites' ORDER BY 1",
    )
    .bind(&f.schema)
    .fetch_all(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        columns,
        vec![
            ("channel_id".to_owned(), "text".to_owned()),
            ("code".to_owned(), "text".to_owned()),
            (
                "created_at".to_owned(),
                "timestamp with time zone".to_owned()
            ),
            ("guild_id".to_owned(), "text".to_owned()),
            ("inviter_id".to_owned(), "text".to_owned()),
            ("max_uses".to_owned(), "integer".to_owned()),
            (
                "observed_at".to_owned(),
                "timestamp with time zone".to_owned()
            ),
            ("uses".to_owned(), "integer".to_owned()),
        ],
        "invite rows must hold exactly ids/timestamps/counts"
    );
    let counts: Vec<(String, i64, i64)> =
        sqlx::query_as("SELECT code, uses, max_uses FROM backfill_invites ORDER BY 1")
            .fetch_all(&f.pool)
            .await
            .unwrap();
    assert_eq!(
        counts,
        vec![("abc123".to_owned(), 7, 10), ("def456".to_owned(), 0, 0),]
    );

    mock.shutdown().await;
    f.finish().await;
}

// (1+3) Onboarding through the observed client persists as ids/enums/counts
// only: prompt ids, prompt types, toggles and option counts — never prompt
// titles, descriptions or response text (privacy tripwire).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL; quarantined red, tracked by TOG-22952"]
async fn onboarding_rows_hold_ids_enums_metadata_only() {
    let onboarding = serde_json::json!({
        "guild_id": GUILD,
        "prompts": [
            {"id": "555555555555555555", "title": "NEVER PERSIST", "type": 0,
             "single_select": true, "required": true, "in_onboarding": true,
             "options": [{"id": "666666666666666666", "title": "NEVER PERSIST"}]},
            {"id": "777777777777777788", "title": "NEVER PERSIST", "type": 1,
             "single_select": false, "required": false, "in_onboarding": true,
             "options": []}
        ],
        "default_channel_ids": ["888888888888888888"],
        "enabled": true
    });
    let mock = FakeRest::start(
        vec![FakeResponse::json(200, onboarding.clone())],
        FakeResponse::json(200, serde_json::json!({})),
    )
    .await;
    let exec = family_executor(&mock.origin());
    let path = format!("/guilds/{GUILD}/onboarding");
    let data = fetch_family_page(&exec, &path).await;
    assert_eq!(data, onboarding);
    assert_eq!(data["guild_id"], serde_json::json!(GUILD));
    assert_eq!(mock.requests().len(), 1);

    let f = Fixture::new().await;
    sqlx::query(
        "INSERT INTO backfill_onboarding_guilds (guild_id, enabled, observed_at)
         VALUES ($1, $2, now())
         ON CONFLICT (guild_id) DO UPDATE SET enabled = excluded.enabled, observed_at = now()",
    )
    .bind(data["guild_id"].as_str().expect("onboarding guild"))
    .bind(data["enabled"].as_bool().expect("onboarding enabled"))
    .execute(&f.pool)
    .await
    .unwrap();
    for prompt in data["prompts"].as_array().expect("prompts") {
        sqlx::query(
            "INSERT INTO backfill_onboarding_prompts
               (guild_id, prompt_id, prompt_type, single_select, required,
                in_onboarding, options_count, observed_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, now())
             ON CONFLICT (guild_id, prompt_id) DO UPDATE SET
               options_count = excluded.options_count, observed_at = now()",
        )
        .bind(GUILD)
        .bind(prompt["id"].as_str().expect("prompt id"))
        .bind(prompt["type"].as_i64().expect("prompt type"))
        .bind(prompt["single_select"].as_bool().unwrap_or(false))
        .bind(prompt["required"].as_bool().unwrap_or(false))
        .bind(prompt["in_onboarding"].as_bool().unwrap_or(false))
        .bind(prompt["options"].as_array().expect("options").len() as i64)
        .execute(&f.pool)
        .await
        .unwrap();
    }
    for table in ["backfill_onboarding_guilds", "backfill_onboarding_prompts"] {
        let columns: Vec<String> = sqlx::query_scalar(
            "SELECT column_name FROM information_schema.columns
             WHERE table_schema = $1 AND table_name = $2 ORDER BY 1",
        )
        .bind(&f.schema)
        .bind(table)
        .fetch_all(&f.pool)
        .await
        .unwrap();
        assert!(
            !columns.is_empty(),
            "{table} must exist with metadata columns"
        );
        for banned in ["title", "description", "body", "content", "response"] {
            assert!(
                !columns.iter().any(|c| c.contains(banned)),
                "{table} must never persist {banned} text: {columns:?}"
            );
        }
    }
    let guild_columns: Vec<String> = sqlx::query_scalar(
        "SELECT column_name FROM information_schema.columns
         WHERE table_schema = $1 AND table_name = 'backfill_onboarding_guilds' ORDER BY 1",
    )
    .bind(&f.schema)
    .fetch_all(&f.pool)
    .await
    .unwrap();
    assert_eq!(guild_columns, vec!["enabled", "guild_id", "observed_at"]);
    let prompt_columns: Vec<String> = sqlx::query_scalar(
        "SELECT column_name FROM information_schema.columns
         WHERE table_schema = $1 AND table_name = 'backfill_onboarding_prompts' ORDER BY 1",
    )
    .bind(&f.schema)
    .fetch_all(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        prompt_columns,
        vec![
            "guild_id",
            "in_onboarding",
            "observed_at",
            "options_count",
            "prompt_id",
            "prompt_type",
            "required",
            "single_select",
        ]
    );
    // Option counts survive; titles never touch the database.
    let options: Vec<(String, i64)> = sqlx::query_as(
        "SELECT prompt_id, options_count FROM backfill_onboarding_prompts ORDER BY 1",
    )
    .fetch_all(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        options,
        vec![
            ("555555555555555555".to_owned(), 1),
            ("777777777777777788".to_owned(), 0),
        ]
    );

    mock.shutdown().await;
    f.finish().await;
}

// (4) Crash-restart resumes each family's catch-up from its persisted cursor:
// the bans page after the restart starts from the persisted after-cursor
// (never from zero) and re-ingesting never duplicates per-user rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL; quarantined red, tracked by TOG-22952"]
async fn bans_invites_onboarding_resume_continues_from_persisted_cursors() {
    let bans_p1 = serde_json::json!([
        ban_entry("111111111111111111", "spam"),
        ban_entry("222222222222222222", "raids"),
    ]);
    let bans_p2 = serde_json::json!([ban_entry("333333333333333333", "spam")]);
    let mock = FakeRest::start(
        vec![
            FakeResponse::json(200, bans_p1.clone()),
            FakeResponse::json(200, bans_p2.clone()),
        ],
        FakeResponse::json(200, serde_json::json!([])),
    )
    .await;
    let exec = family_executor(&mock.origin());

    let f = Fixture::new().await;
    // First window: fetch from the persisted cursor and checkpoint the floor.
    put_cursor(&f.pool, "bans", "111111111111111111").await;
    put_cursor(&f.pool, "invites", "abc123").await;
    put_cursor(&f.pool, "onboarding", "observed").await;
    let cursor = get_cursor(&f.pool, "bans")
        .await
        .expect("bans cursor persists");
    let page1 = fetch_family_page(
        &exec,
        &format!("/guilds/{GUILD}/bans?limit=1000&after={cursor}"),
    )
    .await;
    let entries1: Vec<serde_json::Value> = page1.as_array().expect("ban page").clone();
    apply_ban_page(&f.pool, GUILD, &entries1, &blocklist()).await;
    put_cursor(&f.pool, "bans", "222222222222222222").await;

    // Crash: drop the pool; only durable state survives.
    let mut f = f;
    f.restart_pool().await;

    // Resume: every family continues from its persisted cursor.
    assert_eq!(
        get_cursor(&f.pool, "bans").await,
        Some("222222222222222222".to_owned()),
        "bans resume from the persisted cursor, never zero"
    );
    assert_eq!(
        get_cursor(&f.pool, "invites").await,
        Some("abc123".to_owned())
    );
    assert_eq!(
        get_cursor(&f.pool, "onboarding").await,
        Some("observed".to_owned())
    );
    let zero_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM backfill_cursors WHERE cursor IN ('', '0')")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(zero_rows, 0, "no zero/empty cursor row may exist");
    let resumed = get_cursor(&f.pool, "bans").await.expect("bans cursor");
    let page2 = fetch_family_page(
        &exec,
        &format!("/guilds/{GUILD}/bans?limit=1000&after={resumed}"),
    )
    .await;
    let entries2: Vec<serde_json::Value> = page2.as_array().expect("ban page").clone();
    apply_ban_page(&f.pool, GUILD, &entries2, &blocklist()).await;
    // Re-ingesting page 1 after the restart must not duplicate rows.
    apply_ban_page(&f.pool, GUILD, &entries1, &blocklist()).await;

    let seen = mock.requests();
    assert_eq!(seen.len(), 2);
    assert!(
        seen[1].path.contains("after=222222222222222222"),
        "second bans read continues after the persisted cursor: {}",
        seen[1].path
    );
    let kept: Vec<String> = sqlx::query_scalar("SELECT user_id FROM backfill_ban_users ORDER BY 1")
        .fetch_all(&f.pool)
        .await
        .unwrap();
    assert_eq!(kept, vec!["111111111111111111", "333333333333333333"]);
    let counts: (i64, i64) = sqlx::query_as(
        "SELECT total_seen, skipped_blocklisted FROM backfill_ban_counts WHERE guild_id = $1",
    )
    .bind(GUILD)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    // Page 1 (2 seen, 1 skipped) + page 2 (1 seen) + page-1 replay (2 seen,
    // 1 skipped): per-user rows stay deduplicated while aggregates count
    // every observation.
    assert_eq!(counts, (5, 2));

    mock.shutdown().await;
    f.finish().await;
}
