//! Protected behavioral acceptance: acknowledge rank/leaderboard reads
//! before touching storage, and complete acknowledged storage failures with
//! one sanitized actionable reply (R1).
//!
//! Baseline: `leveling_runtime.rs` awaits `profile`/`seasonal_leaderboard`
//! before its initial callback, and `command_runtime.rs` logs the store error
//! and abandons the interaction. On that baseline every ordering, failure and
//! ownership test in this file FAILS behaviorally: no acknowledgement precedes
//! the read, failures send nothing, and a rejected or uncertain callback still
//! costs a store read. The two ignored disposable-DB tests fail the same way
//! (immediate type-4 reply, no completion edit).
//!
//! Builder contract (this file is preserved verbatim; see the manifest on the
//! acceptance card):
//!
//! - Send the initial acknowledgement BEFORE either store read: rank defers
//!   ephemerally (type 5, flags 64), leaderboard defers publicly (type 5,
//!   flags not 64). Rank stays lifetime-based, leaderboard stays scoped to
//!   the current season; no scoring, season or reward change.
//! - Complete an acknowledged storage failure with exactly one edit
//!   (PATCH `.../messages/@original`) carrying a sanitized actionable
//!   unavailable reply: non-empty, at most 2000 chars, tells the member to
//!   try again (the repo's `"; try again."` recovery convention), leaks no
//!   store/token/host detail, and keeps mention suppression on the wire.
//! - Keep one reply owner: a rejected or uncertain (timeout/transport)
//!   acknowledgement performs ZERO store reads and sends nothing further —
//!   no second initial callback, no uncertain-send retry. A successful
//!   acknowledgement is completed by editing, never by a second callback.
//! - Prove it through the real routed handler (`CommandRuntime::on_interaction`
//!   with the shared router); the pending-read seam below is the only
//!   injected double. Preserve the guild/application/feature fences and make
//!   no new publication (no PUT) or metric claims.
//!
//! Skills used: test-driven-development, verification-before-completion.

use super::*;
use two_bot_core::leveling::{
    leaderboard_reply_for_season, level_for_xp, rank_text, season_id_for_ms, LeaderboardEntry,
};
use two_bot_testsupport::TestDatabase;

/// Controllable pending-read seam: a fake Postgres endpoint that counts
/// accepted connections, stamps the first one, holds every connection open
/// without speaking the wire protocol (so the first store read pends), and
/// fails every held read on shutdown.
struct FakePg {
    port: u16,
    connects: Arc<AtomicU64>,
    first_connect_at: Arc<Mutex<Option<std::time::Instant>>>,
    release: Arc<tokio::sync::Notify>,
    handle: JoinHandle<()>,
}

impl FakePg {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fake pg listen");
        let port = listener.local_addr().expect("fake pg addr").port();
        let connects = Arc::new(AtomicU64::new(0));
        let first_connect_at = Arc::new(Mutex::new(None));
        let release = Arc::new(tokio::sync::Notify::new());
        let handle = tokio::spawn({
            let connects = connects.clone();
            let first_connect_at = first_connect_at.clone();
            let release = release.clone();
            async move {
                loop {
                    let Ok((socket, _)) = listener.accept().await else {
                        return;
                    };
                    connects.fetch_add(1, Ordering::Relaxed);
                    first_connect_at
                        .lock()
                        .expect("first connect stamp")
                        .get_or_insert_with(std::time::Instant::now);
                    let release = release.clone();
                    tokio::spawn(async move {
                        let _held = socket;
                        release.notified().await;
                    });
                }
            }
        });
        Self {
            port,
            connects,
            first_connect_at,
            release,
            handle,
        }
    }

    fn url(&self) -> String {
        format!(
            "postgres://ack_acceptance@127.0.0.1:{}/ack_acceptance",
            self.port
        )
    }

    fn connects(&self) -> u64 {
        self.connects.load(Ordering::Relaxed)
    }

    fn first_connect_at(&self) -> Option<std::time::Instant> {
        *self.first_connect_at.lock().expect("first connect read")
    }

    /// Fail every held read, then stop accepting.
    async fn shutdown(&self) {
        self.release.notify_waiters();
        self.handle.abort();
    }
}

fn pending_pool(url: &str) -> PgPool {
    PgPoolOptions::new()
        .min_connections(0)
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(20))
        .connect_lazy(url)
        .expect("lazy acceptance pool")
}

fn runtime_with_pool(pool: PgPool, origin: String) -> Arc<CommandRuntime> {
    let executor =
        ActionExecutor::with_proxy("test-token".to_owned(), Some(origin)).expect("mock executor");
    CommandRuntime::new(
        pool,
        executor,
        router_with_commands(gates(false, false)),
        GUILD,
        false,
    )
}

fn callbacks(mock: &MockRest) -> Vec<RestRequest> {
    mock.requests()
        .into_iter()
        .filter(|r| r.method == "POST" && r.path.ends_with("/callback"))
        .collect()
}

fn edits(mock: &MockRest) -> Vec<RestRequest> {
    mock.requests()
        .into_iter()
        .filter(|r| r.method == "PATCH" && r.path.ends_with("/messages/@original"))
        .collect()
}

async fn wait_for_callback(mock: &MockRest, timeout: Duration) -> Vec<RestRequest> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let found = callbacks(mock);
        if !found.is_empty() || tokio::time::Instant::now() >= deadline {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_read(pg: &FakePg, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    while pg.connects() == 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn assert_rank_defer(body: &[u8]) {
    let json: serde_json::Value = serde_json::from_slice(body).expect("defer is json");
    assert_eq!(json["type"], 5, "rank acknowledges with a defer");
    assert_eq!(json["data"]["flags"], 64, "rank defer stays ephemeral");
}

fn assert_leaderboard_defer(body: &[u8]) {
    let json: serde_json::Value = serde_json::from_slice(body).expect("defer is json");
    assert_eq!(json["type"], 5, "leaderboard acknowledges with a defer");
    assert_ne!(
        json["data"]["flags"], 64,
        "leaderboard defer stays public, got {json}"
    );
}

/// The acknowledged-failure completion: one sanitized actionable unavailable
/// reply, mention-suppressed on the wire, leaking no store detail.
fn assert_unavailable_edit(body: &[u8], token: &str) {
    let reply: serde_json::Value = serde_json::from_slice(body).expect("edit is json");
    let content = reply["content"]
        .as_str()
        .expect("completion edit carries text content");
    assert!(
        !content.is_empty() && content.len() <= 2000,
        "bounded reply, got {content:?}"
    );
    assert!(
        content.to_lowercase().contains("try again"),
        "actionable recovery copy (repo \"try again\" convention), got {content:?}"
    );
    for marker in [
        "postgres",
        "sqlx",
        "127.0.0.1",
        "ack_acceptance",
        "agent_test",
        token,
    ] {
        assert!(
            !content.to_lowercase().contains(&marker.to_lowercase()),
            "completion leaks no store detail ({marker}), got {content:?}"
        );
    }
    assert_eq!(
        reply["allowed_mentions"]["parse"],
        serde_json::json!([]),
        "completion edit suppresses mentions"
    );
}

fn assert_no_publication(mock: &MockRest) {
    assert!(
        mock.requests().iter().all(|r| r.method != "PUT"),
        "no publication writes on the rank/leaderboard path"
    );
}

/// `/rank` for `member_id` with an explicit display name, mirroring the
/// member-option JSON surgery of the shared routed test.
fn rank_for(member_id: u64, global_name: Option<&str>) -> Interaction {
    let base = slash("rank", Some(CHANNEL), Vec::new());
    let mut value = serde_json::to_value(&base).expect("interaction json");
    value["data"]["options"] =
        serde_json::json!([{"name": "member", "type": 6, "value": member_id.to_string()}]);
    let mut target = user(member_id, false);
    target.global_name = global_name.map(str::to_owned);
    value["data"]["resolved"] = serde_json::json!({"users": {member_id.to_string(): target}});
    serde_json::from_value(value).expect("rank interaction")
}

async fn join_worker(handle: JoinHandle<()>) {
    tokio::time::timeout(Duration::from_secs(20), handle)
        .await
        .expect("handler joins after the read resolves")
        .expect("handler never panics");
}

#[tokio::test]
async fn rank_defers_before_delayed_store_read_then_completes_unavailable_via_edit() {
    let pg = FakePg::start().await;
    let (mock, origin) = MockRest::start(vec![204, 200]).await;
    let runtime = runtime_with_pool(pending_pool(&pg.url()), origin);
    let interaction = slash("rank", Some(CHANNEL), Vec::new());
    let worker = tokio::spawn({
        let runtime = runtime.clone();
        let interaction = interaction.clone();
        async move { runtime.on_interaction(&interaction).await }
    });
    let observed = wait_for_callback(&mock, Duration::from_secs(5)).await;
    // The read starts (the seam holds it, so it cannot complete) while the
    // acknowledgement is already on the wire; nothing may complete first.
    wait_for_read(&pg, Duration::from_secs(5)).await;
    let edits_before_release = edits(&mock).len();
    let first_connect = pg.first_connect_at();
    pg.shutdown().await;
    join_worker(worker).await;

    assert_eq!(
        observed.len(),
        1,
        "exactly one initial acknowledgement while the read pends"
    );
    assert_rank_defer(&observed[0].body);
    let first_connect = first_connect.expect("the store read started");
    assert!(
        observed[0].received_at <= first_connect,
        "acknowledgement precedes the first store read"
    );
    assert_eq!(
        edits_before_release, 0,
        "no completion before the read resolves"
    );
    let done = edits(&mock);
    assert_eq!(
        done.len(),
        1,
        "one completion edit, never a second initial callback"
    );
    assert_eq!(callbacks(&mock).len(), 1, "still exactly one callback");
    assert_eq!(
        done[0].path, "/api/v10/webhooks/1111/sticky-test-token/messages/@original",
        "completion edits the interaction's own application response"
    );
    assert_unavailable_edit(&done[0].body, &interaction.token);
    assert_no_publication(&mock);
    drop(runtime);
    mock.shutdown().await;
}

#[tokio::test]
async fn leaderboard_defers_publicly_before_delayed_store_read_then_completes_unavailable_via_edit()
{
    let pg = FakePg::start().await;
    let (mock, origin) = MockRest::start(vec![204, 200]).await;
    let runtime = runtime_with_pool(pending_pool(&pg.url()), origin);
    let interaction = slash("leaderboard", Some(CHANNEL), Vec::new());
    let worker = tokio::spawn({
        let runtime = runtime.clone();
        let interaction = interaction.clone();
        async move { runtime.on_interaction(&interaction).await }
    });
    let observed = wait_for_callback(&mock, Duration::from_secs(5)).await;
    wait_for_read(&pg, Duration::from_secs(5)).await;
    let edits_before_release = edits(&mock).len();
    let first_connect = pg.first_connect_at();
    pg.shutdown().await;
    join_worker(worker).await;

    assert_eq!(
        observed.len(),
        1,
        "exactly one initial acknowledgement while the read pends"
    );
    assert_leaderboard_defer(&observed[0].body);
    let first_connect = first_connect.expect("the store read started");
    assert!(
        observed[0].received_at <= first_connect,
        "acknowledgement precedes the first store read"
    );
    assert_eq!(
        edits_before_release, 0,
        "no completion before the read resolves"
    );
    let done = edits(&mock);
    assert_eq!(
        done.len(),
        1,
        "one completion edit, never a second initial callback"
    );
    assert_eq!(callbacks(&mock).len(), 1, "still exactly one callback");
    assert_unavailable_edit(&done[0].body, &interaction.token);
    assert_no_publication(&mock);
    drop(runtime);
    mock.shutdown().await;
}

#[tokio::test]
async fn rank_immediate_store_error_still_defers_then_completes_unavailable_via_edit() {
    let (mock, origin) = MockRest::start(vec![204, 200]).await;
    // Refused endpoint: the read fails at once, like `runtime_without_db`.
    let pool = PgPoolOptions::new()
        .min_connections(0)
        .max_connections(1)
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("lazy pool");
    let runtime = runtime_with_pool(pool, origin);
    let interaction = slash("rank", Some(CHANNEL), Vec::new());
    runtime.on_interaction(&interaction).await;

    let seen = callbacks(&mock);
    assert_eq!(
        seen.len(),
        1,
        "the failed read still gets an acknowledgement first"
    );
    assert_rank_defer(&seen[0].body);
    let done = edits(&mock);
    assert_eq!(
        done.len(),
        1,
        "the acknowledged failure completes instead of abandoning the interaction"
    );
    assert_unavailable_edit(&done[0].body, &interaction.token);
    assert_no_publication(&mock);
    drop(runtime);
    mock.shutdown().await;
}

#[tokio::test]
async fn leaderboard_immediate_store_error_still_defers_then_completes_unavailable_via_edit() {
    let (mock, origin) = MockRest::start(vec![204, 200]).await;
    let pool = PgPoolOptions::new()
        .min_connections(0)
        .max_connections(1)
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("lazy pool");
    let runtime = runtime_with_pool(pool, origin);
    let interaction = slash("leaderboard", Some(CHANNEL), Vec::new());
    runtime.on_interaction(&interaction).await;

    let seen = callbacks(&mock);
    assert_eq!(
        seen.len(),
        1,
        "the failed read still gets an acknowledgement first"
    );
    assert_leaderboard_defer(&seen[0].body);
    let done = edits(&mock);
    assert_eq!(
        done.len(),
        1,
        "the acknowledged failure completes instead of abandoning the interaction"
    );
    assert_unavailable_edit(&done[0].body, &interaction.token);
    assert_no_publication(&mock);
    drop(runtime);
    mock.shutdown().await;
}

async fn rejected_defer_case(name: &str) {
    let pg = FakePg::start().await;
    // Confirmed HTTP rejection: no mutation happened, so no read may follow.
    let (mock, origin) = MockRest::start(vec![400]).await;
    let runtime = runtime_with_pool(pending_pool(&pg.url()), origin);
    runtime
        .on_interaction(&slash(name, Some(CHANNEL), Vec::new()))
        .await;
    pg.shutdown().await;

    assert_eq!(
        callbacks(&mock).len(),
        1,
        "one acknowledgement attempt, never retried"
    );
    assert!(
        edits(&mock).is_empty(),
        "a rejected acknowledgement is never completed"
    );
    assert_eq!(
        pg.connects(),
        0,
        "a rejected acknowledgement performs zero store reads"
    );
    assert_no_publication(&mock);
    drop(runtime);
    mock.shutdown().await;
}

async fn uncertain_defer_case(name: &str) {
    let pg = FakePg::start().await;
    // The callback response never arrives inside the receipt budget: the
    // acknowledgement is uncertain, so no read may follow and nothing may
    // be sent again.
    let (mock, origin) = MockRest::start_script(vec![RestResponse {
        status: 200,
        body: None,
        delay: Duration::from_secs(30),
    }])
    .await;
    let runtime = runtime_with_pool(pending_pool(&pg.url()), origin);
    tokio::time::timeout(
        Duration::from_secs(30),
        runtime.on_interaction(&slash(name, Some(CHANNEL), Vec::new())),
    )
    .await
    .expect("uncertain defer still resolves");
    pg.shutdown().await;

    assert_eq!(
        callbacks(&mock).len(),
        1,
        "one acknowledgement attempt, never an uncertain-send retry"
    );
    assert!(
        edits(&mock).is_empty(),
        "an uncertain acknowledgement is never completed"
    );
    assert_eq!(
        pg.connects(),
        0,
        "an uncertain acknowledgement performs zero store reads"
    );
    assert_no_publication(&mock);
    drop(runtime);
    mock.shutdown().await;
}

#[tokio::test]
async fn rejected_rank_defer_performs_zero_store_reads() {
    rejected_defer_case("rank").await;
}

#[tokio::test]
async fn rejected_leaderboard_defer_performs_zero_store_reads() {
    rejected_defer_case("leaderboard").await;
}

#[tokio::test]
async fn uncertain_rank_defer_performs_zero_store_reads_without_retry() {
    uncertain_defer_case("rank").await;
}

#[tokio::test]
async fn uncertain_leaderboard_defer_performs_zero_store_reads_without_retry() {
    uncertain_defer_case("leaderboard").await;
}

// ---------------------------------------------------------------------------
// Ignored disposable-DB success path (mock REST only; never staging/prod)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires disposable agent-testdb and mock REST only"]
async fn rank_success_completes_via_edit_with_lifetime_values() {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("explicit test bootstrap URL");
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("migrated test database; no credential fallback");
    let (mock, origin) = MockRest::start(vec![204, 200]).await;
    let executor = ActionExecutor::with_proxy("mock-only-token".into(), Some(origin)).unwrap();
    let runtime = CommandRuntime::new(
        db.pool().clone(),
        executor,
        router_with_commands(gates(false, false)),
        GUILD,
        false,
    );
    // Lifetime value the reply must render ...
    sqlx::query("INSERT INTO member_levels (guild_id, member_id, xp, message_xp, voice_xp, imported_xp, updated_at) VALUES ($1, $2, $3, $4, $5, $6, NOW())")
        .bind(GUILD_S)
        .bind("88")
        .bind(1500i64)
        .bind(1000i64)
        .bind(500i64)
        .bind(0i64)
        .execute(db.pool())
        .await
        .unwrap();
    // ... with a seasonal decoy that must NOT leak into the lifetime reply.
    let season = season_id_for_ms(now_millis_for_test());
    sqlx::query("INSERT INTO member_season_xp (guild_id, season_id, member_id, seasonal_xp, updated_at) VALUES ($1, $2, $3, $4, NOW())")
        .bind(GUILD_S)
        .bind(&season)
        .bind("88")
        .bind(50i64)
        .execute(db.pool())
        .await
        .unwrap();
    let hostile = "<@123> @everyone\n**bold** `code`";
    runtime.on_interaction(&rank_for(88, Some(hostile))).await;

    let seen = callbacks(&mock);
    assert_eq!(seen.len(), 1, "the successful read is acknowledged first");
    assert_rank_defer(&seen[0].body);
    let done = edits(&mock);
    assert_eq!(done.len(), 1, "the successful read completes via one edit");
    let reply: serde_json::Value = serde_json::from_slice(&done[0].body).expect("edit is json");
    assert_eq!(
        reply["content"],
        serde_json::Value::String(rank_text(hostile, level_for_xp(1500), Some(1), 1, 1500)),
        "rank renders the lifetime profile with the hostile name neutralized"
    );
    assert_eq!(
        reply["allowed_mentions"]["parse"],
        serde_json::json!([]),
        "rank completion suppresses mentions"
    );
    assert_no_publication(&mock);
    drop(runtime);
    mock.shutdown().await;
    db.close().await.expect("drop own disposable database");
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb and mock REST only"]
async fn leaderboard_success_completes_via_edit_with_current_season_values() {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("explicit test bootstrap URL");
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("migrated test database; no credential fallback");
    let (mock, origin) = MockRest::start(vec![204, 200]).await;
    let executor = ActionExecutor::with_proxy("mock-only-token".into(), Some(origin)).unwrap();
    let runtime = CommandRuntime::new(
        db.pool().clone(),
        executor,
        router_with_commands(gates(false, false)),
        GUILD,
        false,
    );
    // Lifetime decoys in REVERSE order: a lifetime fallback would rank 77
    // first, while the current-season board must rank 88 first.
    let season = season_id_for_ms(now_millis_for_test());
    for member in 77..89 {
        let seasonal = i64::from(member);
        let lifetime = i64::from(89 - (member - 77));
        sqlx::query("INSERT INTO member_levels (guild_id, member_id, xp, message_xp, voice_xp, imported_xp, updated_at) VALUES ($1, $2, $3, $4, 0, 0, NOW())")
            .bind(GUILD_S)
            .bind(member.to_string())
            .bind(lifetime)
            .bind(lifetime)
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO member_season_xp (guild_id, season_id, member_id, seasonal_xp, updated_at) VALUES ($1, $2, $3, $4, NOW())")
            .bind(GUILD_S)
            .bind(&season)
            .bind(member.to_string())
            .bind(seasonal)
            .execute(db.pool())
            .await
            .unwrap();
    }
    runtime
        .on_interaction(&slash("leaderboard", Some(CHANNEL), Vec::new()))
        .await;

    let seen = callbacks(&mock);
    assert_eq!(seen.len(), 1, "the successful read is acknowledged first");
    assert_leaderboard_defer(&seen[0].body);
    let done = edits(&mock);
    assert_eq!(done.len(), 1, "the successful read completes via one edit");
    let expected: Vec<LeaderboardEntry> = (79_u64..89)
        .rev()
        .enumerate()
        .map(|(index, member)| LeaderboardEntry {
            member_id: member.to_string(),
            xp: member,
            level: level_for_xp(member),
            rank: index as u64 + 1,
        })
        .collect();
    let reply: serde_json::Value = serde_json::from_slice(&done[0].body).expect("edit is json");
    assert_eq!(
        reply["content"],
        serde_json::Value::String(leaderboard_reply_for_season(&expected, &season).content),
        "leaderboard renders the current season, never lifetime totals"
    );
    assert_eq!(
        reply["allowed_mentions"]["parse"],
        serde_json::json!([]),
        "leaderboard completion suppresses mentions so <@id> rows never ping"
    );
    assert_no_publication(&mock);
    drop(runtime);
    mock.shutdown().await;
    db.close().await.expect("drop own disposable database");
}
