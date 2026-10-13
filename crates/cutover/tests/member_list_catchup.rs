//! Failing acceptance for the member-list catch-up worker (TOG-22941).
//!
//! Paired failing-tests card for the member-list slice of the backfill worker
//! (TOG-22852). Test files ONLY: this target must FAIL until the builder lands
//! the `two_bot_cutover::member_catchup` module. It pins the contract the build
//! card implements:
//!
//! * `fetch_member_page`: one `GET /guilds/{id}/members` page through the
//!   shared [`RestClient`](two_bot_cutover::rest::RestClient), with a caller
//!   limit of 1-1000 and the `after` cursor carried verbatim onto the wire;
//! * `apply_member_page`: blocklisted users (TOG-22850 opt-out) are skipped for
//!   per-user rows while the aggregate count keeps counting them;
//! * `MemberRow`: ids/timestamps/enums only (privacy fence, same rule as the
//!   event-store S1 slice: no message bodies, no custom-status text);
//! * `store_member_cursor` / `load_member_cursor`: resume continues from the
//!   persisted cursor, never from zero.
//!
//! Fakes only: loopback TCP fixtures stand in for Discord, and the three
//! database tests use isolated schemas on agent-testdb (or the CI service
//! container) with the agent_test identity. Never staging or production.
//!
//! Rate-limit facts verified against the official Discord documentation
//! (https://docs.discord.com/developers/topics/rate-limits, 2026-10-12):
//! per-route buckets via `X-RateLimit-*`, global bots cap at 50 requests per
//! second, 429s wait on `Retry-After`/`retry_after`, and 401/403/429 responses
//! count toward the 10k-per-10-minute invalid-request cutoff. Every number the
//! implementation must honor arrives in a response header, never a literal.
//! Member pagination bounds (limit 1-1000, `after` cursor) are per this card's
//! acceptance text and the in-repo `RestClient::fetch_all_members` shape
//! (limit 1000 + `after`), which the new page call parameterizes.
//!
//! Run offline: `cargo test -p two-bot-cutover --locked --test member_list_catchup`
//! Run DB: `cargo test -p two-bot-cutover --locked --test member_list_catchup -- --ignored`
//!
//! The fixture guild `2222` is synthetic. Never the live guild.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Pool, Postgres};
use twilight_model::id::marker::GuildMarker;
use twilight_model::id::Id;
use two_bot_cutover::member_catchup::{
    apply_member_page, fetch_member_page, load_member_cursor, store_member_cursor, MemberRow,
};
use two_bot_cutover::rest::RestClient;

/// Synthetic guild for every fixture below. Not the live guild.
const GUILD: u64 = 2222;
const GUILD_STR: &str = "2222";

fn guild() -> Id<GuildMarker> {
    Id::new(GUILD)
}

fn member_json(id: &str, pending: bool) -> String {
    format!(
        r#"{{"user":{{"id":"{id}","username":"u","discriminator":"0","avatar":null}},"roles":[],"joined_at":"2024-01-01T00:00:00.000000+00:00","pending":{pending},"deaf":false,"mute":false,"flags":0}}"#
    )
}

fn page_json(ids: &[(&str, bool)]) -> String {
    format!(
        "[{}]",
        ids.iter()
            .map(|(id, pending)| member_json(id, *pending))
            .collect::<Vec<_>>()
            .join(",")
    )
}

/// Loopback member list: routes on the `after` query value, records every
/// request line for wire assertions. Serves at most the scripted pages, then
/// empty pages (end of list).
struct MemberMock {
    origin: String,
    seen: Arc<Mutex<Vec<String>>>,
    count: Arc<AtomicUsize>,
}

impl MemberMock {
    fn start(script: Vec<(String, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind member mock");
        let origin = format!("http://{}", listener.local_addr().expect("addr"));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let count = Arc::new(AtomicUsize::new(0));
        let (seen_task, count_task) = (seen.clone(), count.clone());
        std::thread::spawn(move || {
            let mut script = script.into_iter();
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = [0u8; 8192];
                let n = s.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).into_owned();
                let line = head.lines().next().unwrap_or_default().to_owned();
                seen_task.lock().expect("seen").push(line.clone());
                count_task.fetch_add(1, Ordering::SeqCst);
                let after = line
                    .split("after=")
                    .nth(1)
                    .and_then(|r| r.split(['&', ' ']).next())
                    .unwrap_or("")
                    .to_owned();
                let body = script
                    .find(|(want, _)| *want == after)
                    .map(|(_, body)| body)
                    .unwrap_or_else(|| "[]".to_owned());
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Self {
            origin,
            seen,
            count,
        }
    }

    fn requests(&self) -> Vec<String> {
        self.seen.lock().expect("seen").clone()
    }

    fn query_param(request_line: &str, name: &str) -> Option<String> {
        let query = request_line.split_whitespace().nth(1)?;
        let query = query.split_once('?')?.1;
        for pair in query.split('&') {
            let (k, v) = pair.split_once('=')?;
            if k == name {
                return Some(v.to_owned());
            }
        }
        None
    }
}

fn row(id: &str, pending: Option<bool>) -> MemberRow {
    MemberRow {
        member_id: id.to_owned(),
        joined_at: Some("2024-01-01T00:00:00.000000+00:00".to_owned()),
        pending,
        is_bot: false,
    }
}

// (1) Pagination honors a caller limit of 1-1000 and advances the `after`
// cursor: page one carries an in-range limit with no cursor, page two carries
// the first page's last id as `after`. Limits outside 1-1000 are refused.
#[tokio::test]
async fn member_pages_carry_limit_one_to_one_thousand_and_advance_after() {
    let mock = MemberMock::start(vec![
        (
            String::new(),
            page_json(&[("111111111111111111", false), ("222222222222222222", false)]),
        ),
        (
            "222222222222222222".to_owned(),
            page_json(&[("333333333333333333", false)]),
        ),
    ]);
    let rest = RestClient::with_proxy("t".into(), Some(mock.origin.clone()));

    let first = fetch_member_page(&rest, guild(), 2, None)
        .await
        .expect("first page");
    assert_eq!(first.aggregate_count, 2);
    assert_eq!(
        first.next_after.as_deref(),
        Some("222222222222222222"),
        "cursor advances to the page's last id"
    );

    let after = first.next_after.clone();
    let second = fetch_member_page(&rest, guild(), 2, after)
        .await
        .expect("second page");
    assert_eq!(second.aggregate_count, 1);

    let seen = mock.requests();
    assert_eq!(seen.len(), 2, "exactly one wire request per page");
    for line in &seen {
        let limit: u16 = MemberMock::query_param(line, "limit")
            .expect("every member request carries limit")
            .parse()
            .expect("limit is numeric");
        assert!(
            (1..=1000).contains(&limit),
            "limit out of the 1-1000 contract: {limit}"
        );
    }
    assert!(
        MemberMock::query_param(&seen[0], "after").is_none(),
        "first page has no cursor"
    );
    assert_eq!(
        MemberMock::query_param(&seen[1], "after").as_deref(),
        Some("222222222222222222"),
        "second page continues past page one"
    );

    assert!(
        fetch_member_page(&rest, guild(), 0, None).await.is_err(),
        "limit 0 must be refused"
    );
    assert!(
        fetch_member_page(&rest, guild(), 1001, None).await.is_err(),
        "limit above 1000 must be refused"
    );
}

// (2) Every member request passes through the rate-limit client: the client's
// own request counter matches the wire count exactly, so no page bypassed it.
#[tokio::test]
async fn every_member_request_passes_through_the_rate_limit_client() {
    let mock = MemberMock::start(vec![
        (
            String::new(),
            page_json(&[("111111111111111111", false), ("222222222222222222", false)]),
        ),
        (
            "222222222222222222".to_owned(),
            page_json(&[("333333333333333333", false)]),
        ),
    ]);
    let rest = RestClient::with_proxy("t".into(), Some(mock.origin.clone()));

    let mut after: Option<String> = None;
    let mut pages = 0;
    loop {
        let page = fetch_member_page(&rest, guild(), 1000, after)
            .await
            .expect("page reads");
        pages += 1;
        after = page.next_after.clone();
        if after.is_none() {
            break;
        }
    }
    assert_eq!(pages, 3, "two scripted pages plus the empty terminator");
    assert_eq!(
        mock.count.load(Ordering::SeqCst),
        3,
        "two scripted pages plus the empty terminator hit the wire"
    );
    assert_eq!(
        rest.requests(),
        mock.count.load(Ordering::SeqCst) as u64,
        "client counter and wire count must agree: every request went through the client"
    );
    assert!(
        mock.requests()
            .iter()
            .all(|l| l.contains("/guilds/") && l.contains("/members")),
        "every request hit the member-list route"
    );
}

// ── Disposable-DB fixtures (agent-testdb only) ──────────────────────────
// Isolated schema per test, fixed table names the catch-up module owns:
// `member_catchup_members` (per-user rows), `member_catchup_cursors`
// (resume state) and the shared `backfill_cursors` frontier the progress
// route reads. Shapes mirror cutover migrations 0434 (`member_catchup_*`)
// and 0433 (`backfill_cursors`); keep them in sync. Dropped on finish;
// never shared tables.

struct CatchupDb {
    admin: Pool<Postgres>,
    pool: PgPool,
    schema: String,
}

impl CatchupDb {
    async fn new(tag: &str) -> Self {
        let options = PgConnectOptions::new()
            .host("agent-testdb")
            .port(5432)
            .username("agent_test")
            .password("")
            .database("agent_test");
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await
            .expect("agent-testdb connect failed");
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let schema = format!("member_catchup_{tag}_{}_{nonce}", std::process::id());
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .expect("create schema");
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options.options([("search_path", schema.clone())]))
            .await
            .expect("schema pool");
        sqlx::raw_sql(
            "CREATE TABLE member_catchup_members (
               guild_id TEXT NOT NULL, member_id TEXT NOT NULL,
               joined_at timestamptz, pending BOOLEAN,
               is_bot BOOLEAN NOT NULL DEFAULT FALSE,
               updated_at timestamptz NOT NULL DEFAULT now(),
               PRIMARY KEY (guild_id, member_id));
             CREATE TABLE member_catchup_cursors (
               guild_id TEXT PRIMARY KEY,
               after_cursor TEXT NOT NULL CHECK (length(after_cursor) > 0 AND after_cursor <> '0'),
               updated_at timestamptz NOT NULL DEFAULT now());
             CREATE TABLE backfill_cursors (
               family TEXT PRIMARY KEY,
               cursor TEXT NOT NULL CHECK (length(cursor) > 0 AND cursor <> '0'),
               updated_at timestamptz NOT NULL DEFAULT now())",
        )
        .execute(&pool)
        .await
        .expect("create catch-up tables");
        Self {
            admin,
            pool,
            schema,
        }
    }

    /// Simulate a crash-restart: drop the live pool and reconnect to the same
    /// schema, so the test reads only what was durably persisted.
    async fn restart_pool(&mut self) {
        self.pool.close().await;
        let options = PgConnectOptions::new()
            .host("agent-testdb")
            .port(5432)
            .username("agent_test")
            .password("")
            .database("agent_test");
        self.pool = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options.options([("search_path", self.schema.clone())]))
            .await
            .expect("reconnect after restart");
    }

    async fn finish(self) {
        self.pool.close().await;
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA {} CASCADE",
            self.schema
        )))
        .execute(&self.admin)
        .await
        .expect("drop schema");
        self.admin.close().await;
    }
}

// (3) Blocklisted users are skipped for per-user rows while the aggregate
// count keeps counting them (TOG-22850 opt-out honored at the catch-up seam).
#[tokio::test]
#[ignore = "requires agent-testdb; run explicitly with --ignored"]
async fn blocklisted_members_skip_per_user_rows_but_count_in_aggregates() {
    let f = CatchupDb::new("blocklist").await;
    let page = two_bot_cutover::member_catchup::MemberCatchupPage {
        members: vec![
            row("111111111111111111", Some(false)),
            row("222222222222222222", Some(false)),
            row("333333333333333333", None),
        ],
        aggregate_count: 3,
        next_after: Some("333333333333333333".to_owned()),
    };
    let blocklist: HashSet<String> = ["222222222222222222".to_owned()].into();

    let report = apply_member_page(&f.pool, GUILD_STR, &page, &blocklist)
        .await
        .expect("apply page");
    assert_eq!(
        report.aggregate_count, 3,
        "aggregates keep counting everyone"
    );
    assert_eq!(
        report.per_user_rows, 2,
        "blocklisted user has no per-user row"
    );

    let stored: Vec<String> =
        sqlx::query_scalar("SELECT member_id FROM member_catchup_members ORDER BY 1")
            .fetch_all(&f.pool)
            .await
            .expect("read rows");
    assert_eq!(
        stored,
        vec![
            "111111111111111111".to_owned(),
            "333333333333333333".to_owned()
        ],
        "only non-blocklisted members persist"
    );
    f.finish().await;
}

// (4) Persisted member rows hold ids/timestamps/enums only: the row type
// serializes to exactly member_id/joined_at/pending/is_bot, and the table
// carries exactly those columns. No bodies, no status text, no payloads.
#[tokio::test]
#[ignore = "requires agent-testdb; run explicitly with --ignored"]
async fn persisted_member_rows_hold_ids_timestamps_enums_only() {
    let f = CatchupDb::new("fence").await;
    let written = row("444444444444444444", Some(false));
    let mut keys: Vec<String> = serde_json::to_value(&written)
        .expect("row serializes")
        .as_object()
        .expect("row is an object")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "is_bot".to_owned(),
            "joined_at".to_owned(),
            "member_id".to_owned(),
            "pending".to_owned()
        ],
        "row shape is ids/timestamps/enums only"
    );

    let page = two_bot_cutover::member_catchup::MemberCatchupPage {
        members: vec![written],
        aggregate_count: 1,
        next_after: None,
    };
    apply_member_page(&f.pool, GUILD_STR, &page, &HashSet::new())
        .await
        .expect("apply page");

    let columns: Vec<(String, String)> = sqlx::query_as(
        "SELECT column_name, data_type FROM information_schema.columns
         WHERE table_schema = current_schema()
           AND table_name = 'member_catchup_members' ORDER BY 1",
    )
    .fetch_all(&f.pool)
    .await
    .expect("columns");
    assert_eq!(
        columns,
        vec![
            ("guild_id".to_owned(), "text".to_owned()),
            ("is_bot".to_owned(), "boolean".to_owned()),
            (
                "joined_at".to_owned(),
                "timestamp with time zone".to_owned()
            ),
            ("member_id".to_owned(), "text".to_owned()),
            ("pending".to_owned(), "boolean".to_owned()),
            (
                "updated_at".to_owned(),
                "timestamp with time zone".to_owned()
            ),
        ],
        "member rows must hold exactly ids/timestamps/enums"
    );
    f.finish().await;
}

// (5) Resume continues from the persisted cursor: after a crash-restart the
// next page request carries the stored `after`, never a replay from zero.
#[tokio::test]
#[ignore = "requires agent-testdb; run explicitly with --ignored"]
async fn resume_continues_from_the_persisted_cursor() {
    let mut f = CatchupDb::new("resume").await;
    store_member_cursor(&f.pool, GUILD_STR, "222222222222222222")
        .await
        .expect("store cursor");
    f.restart_pool().await;
    assert_eq!(
        load_member_cursor(&f.pool, GUILD_STR)
            .await
            .expect("load cursor")
            .as_deref(),
        Some("222222222222222222"),
        "restart must resume from the persisted cursor, never zero"
    );
    // The members frontier is checkpointed where the progress route reads
    // it: `GET /ops/backfill/progress` serves `backfill_cursors`, never the
    // per-guild cursor table.
    let frontier: Option<String> =
        sqlx::query_scalar("SELECT cursor FROM backfill_cursors WHERE family = 'members'")
            .fetch_optional(&f.pool)
            .await
            .expect("frontier read");
    assert_eq!(
        frontier.as_deref(),
        Some("222222222222222222"),
        "storing the resume cursor must checkpoint the members frontier"
    );

    let mock = MemberMock::start(vec![(
        "222222222222222222".to_owned(),
        page_json(&[("333333333333333333", false)]),
    )]);
    let rest = RestClient::with_proxy("t".into(), Some(mock.origin.clone()));
    let after = load_member_cursor(&f.pool, GUILD_STR)
        .await
        .expect("reload cursor");
    let page = fetch_member_page(&rest, guild(), 1000, after)
        .await
        .expect("resumed page");
    assert_eq!(page.aggregate_count, 1);

    let seen = mock.requests();
    assert_eq!(seen.len(), 1, "resume issues exactly one page read");
    assert_eq!(
        MemberMock::query_param(&seen[0], "after").as_deref(),
        Some("222222222222222222"),
        "resumed read continues past the persisted cursor"
    );
    assert!(
        MemberMock::query_param(&seen[0], "after").is_some(),
        "resume must never replay from zero"
    );
    f.finish().await;
}
