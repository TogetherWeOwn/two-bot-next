#![cfg(test)]

//! Dispatch-failure cause acceptance: a failed leveling drain must name its
//! SQLSTATE, and schema-gap drains must skip like retention instead of
//! holding the cursor.
//!
//! Contract under test (postmortem `docs/production-incident-2026-10-15-postmortem.md`
//! open gap): the worker synthesizes `InvalidArgument` for drain failures,
//! discarding the real database error, so `gateway_failed` carries
//! `error_code=none` and a missing-table drain crash-loops like 2026-10-15.
//! The implementation must:
//!
//! - propagate the underlying SQLSTATE into the surfaced drain error, so
//!   `db_log::error_code` names the code on non-gap holds;
//! - skip `42P01`/`42501` (per `is_schema_gap`) leveling-drain failures with
//!   a warn naming the code plus the `two_bot_gateway_drain_skips_total`
//!   counter, and advance the cursor (retention parity);
//! - keep holding the cursor on non-gap failures, with the cause attached;
//! - keep every failure log sanitized (no SQL text, no secret echo).
//!
//! Only the leveling drain has a database cause to propagate: the interaction,
//! onboarding-job and missing-ticket failures carry no SQLSTATE, so they keep
//! their typed literal with `error_code=none` (existing in-module pins cover
//! those messages). The skip warn must contain the SQLSTATE plus the words
//! `leveling` and `skip`/`skipped`; the alert rule must be documented in
//! `docs/metrics.md` next to the counter.

use super::*;
use tokio::sync::watch;
use twilight_gateway::Event;
use two_bot_testsupport::TestDatabase;

const CAUSE_GUILD: &str = "22";
const CAUSE_GUILD_ID: u64 = 22;
const CAUSE_MEMBER: &str = "44";
const CAUSE_CHANNEL: &str = "66";
/// Marker embedded in the fault constraint's name. The raw Postgres error
/// carries it, so any log line containing it proves an unsanitized leak.
const LEAK_MARKER: &str = "chk_dispatch_cause_must_not_log_7f3a";
/// Counter the implementation must add, document and increment once per
/// skipped schema-gap drain.
const SKIP_COUNTER: &str = "two_bot_gateway_drain_skips_total";

const METRICS_DOC: &str = include_str!("../../../../docs/metrics.md");

/// Serializes the process-global drain-skip counter windows. Both schema-gap
/// tests hold this from before their baseline through their final assertion,
/// so a sibling increment can never satisfy a missing-increment mutant.
static DRAIN_SKIP_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn database() -> Option<TestDatabase> {
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure dispatch-cause gateway fixtures"
            );
            eprintln!(
                "SKIP dispatch-cause gateway database fixture: TWO_TEST_DATABASE_URL not set"
            );
            return None;
        }
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("guarded dispatch-cause gateway fixture"),
    )
}

struct Peer {
    url: String,
    packets: mpsc::Sender<Value>,
    auth: mpsc::Receiver<Value>,
    task: JoinHandle<()>,
}

impl Peer {
    async fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let (packets, mut input) = mpsc::channel::<Value>(16);
        let (auth_tx, auth) = mpsc::channel(2);
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (_, mut ws) = ServerBuilder::new().accept(stream).await.unwrap();
            ws.send(Message::text(
                json!({"op":10,"d":{"heartbeat_interval":45_000}}).to_string(),
            ))
            .await
            .unwrap();
            loop {
                tokio::select! {
                    packet = input.recv() => match packet {
                        Some(packet) => { if ws.send(Message::text(packet.to_string())).await.is_err() { break; } }
                        None => break,
                    },
                    message = ws.next() => {
                        let Some(Ok(message)) = message else { break };
                        if message.is_close() { break; }
                        if !message.is_text() { continue; }
                        let packet: Value = serde_json::from_str(message.as_text().unwrap()).unwrap();
                        let op = packet["op"].as_u64();
                        if op == Some(1)
                            && ws
                                .send(Message::text("{\"op\":11,\"d\":null}".to_owned()))
                                .await
                                .is_err()
                        {
                            break;
                        }
                        if matches!(op, Some(2 | 6)) && auth_tx.send(packet).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Self {
            url,
            packets,
            auth,
            task,
        }
    }

    async fn authentication(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(10), self.auth.recv())
            .await
            .expect("gateway handshake deadline")
            .expect("gateway authenticates")
    }

    async fn send(&self, packet: Value) {
        self.packets.send(packet).await.unwrap();
    }

    async fn close(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

/// Award-queueing message body, from the in-module drain-failure fixture
/// (`gateway_worker_leveling_drain_failure_records_typed_error_without_commit`).
fn message_d() -> Value {
    json!({
        "id": "4000000000000000001",
        "guild_id": CAUSE_GUILD,
        "channel_id": CAUSE_CHANNEL,
        "author": {"id": CAUSE_MEMBER, "username": "member", "discriminator": "0", "bot": false},
        "content": "hello there",
        "timestamp": "2026-09-28T00:00:00.000000+00:00",
        "edited_timestamp": null,
        "tts": false,
        "mention_everyone": false,
        "mentions": [],
        "mention_roles": [],
        "attachments": [],
        "embeds": [],
        "pinned": false,
        "type": 0,
        "components": []
    })
}

fn message(seq: u64) -> Value {
    json!({"op":0,"s":seq,"t":"MESSAGE_CREATE","d":message_d()})
}

fn message_event() -> Event {
    Event::MessageCreate(Box::new(
        serde_json::from_value(message_d()).expect("fixture message"),
    ))
}

struct Running {
    store: GatewaySessionStore,
    stop: watch::Sender<bool>,
    task: JoinHandle<Result<(), sqlx::Error>>,
}

async fn start_runner(pool: &PgPool, peer: &Peer, leveling_pool: &PgPool) -> Running {
    ensure_crypto_provider();
    let store = GatewaySessionStore::new(pool.clone(), CAUSE_GUILD.to_owned(), 0);
    let saved = load_boot_session(&store).await.unwrap();
    assert!(
        saved.is_none(),
        "fresh fixture database must have no saved session"
    );
    let shard = crate::gateway::build_shard(
        TOKEN.into(),
        Intents::empty(),
        saved.as_ref(),
        Some(&peer.url),
    );
    // `dry_run` keeps awards off Discord REST: the executor below is never
    // called, so the drain outcome depends only on the database fault.
    let executor = two_bot_discord::ActionExecutor::with_proxy(
        "mock-token".into(),
        Some("http://127.0.0.1:1".into()),
    )
    .expect("test executor");
    let leveling = two_bot_discord::LevelingRuntime::new(
        leveling_pool.clone(),
        std::sync::Arc::new(executor),
        CAUSE_GUILD_ID,
        two_bot_core::OnboardingGates {
            mode: two_bot_core::OnboardingMode::Legacy,
            dry_run: true,
        },
    );
    // Pin the fixture precondition explicitly: the message must queue a
    // leveling award, or these tests would exercise the commit path instead
    // of the drain-failure site.
    assert!(
        build_pipeline(vec![], None)
            .collect_at(
                &message_event(),
                &two_bot_core::now_iso(),
                two_bot_discord::MessageEligibility::default(),
            )
            .is_ok_and(|requests| !requests.is_empty()),
        "fixture message must queue a leveling award"
    );
    let pipeline = Arc::new(build_pipeline(
        store.milestones().await.unwrap(),
        Some(leveling),
    ));
    let state = Arc::new(RwLock::new(GatewayState::Armed));
    let (stop, rx) = watch::channel(false);
    let task = tokio::spawn(run_shard(
        shard,
        pipeline,
        state,
        store.clone(),
        None,
        None,
        None,
        None,
        None,
        crate::server::shutdown_requested(rx),
    ));
    Running { store, stop, task }
}

enum Outcome {
    Advanced(JoinHandle<Result<(), sqlx::Error>>),
    Held(sqlx::Error),
}

/// Race the checkpoint advance against the worker resolving: a held cursor
/// resolves the run with the retained worker error instead of hanging the
/// 20 s `wait_sequence` deadline.
async fn await_outcome(
    store: &GatewaySessionStore,
    task: JoinHandle<Result<(), sqlx::Error>>,
    sequence: u64,
) -> Outcome {
    tokio::time::timeout(Duration::from_secs(20), async move {
        let task = task;
        loop {
            if task.is_finished() {
                return Outcome::Held(
                    task.await
                        .expect("gateway task joins")
                        .expect_err("a resolved-before-commit worker holds its error"),
                );
            }
            if store
                .load()
                .await
                .expect("load")
                .is_some_and(|saved| saved.sequence == sequence)
            {
                return Outcome::Advanced(task);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("dispatch outcome deadline")
}

async fn stop_runner(stop: watch::Sender<bool>, task: JoinHandle<Result<(), sqlx::Error>>) {
    stop.send_replace(true);
    tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("gateway shutdown joins writer")
        .expect("gateway task joins")
        .expect("clean shutdown commits");
}

fn capture() -> &'static crate::tracing_capture::Capture {
    crate::tracing_capture::global()
}

/// Log lines containing every needle. The capture is process-global, so each
/// needles set pairs the fault's SQLSTATE with `leveling`: retention's own
/// schema-gap skip lines can never match.
fn lines_with<'a>(logs: &'a str, needles: &[&str]) -> Vec<&'a str> {
    logs.lines()
        .filter(|line| needles.iter().all(|needle| line.contains(needle)))
        .collect()
}

fn drain_skip_count() -> u64 {
    let metrics = two_bot_core::metrics::global().render(None);
    let prefix = format!("{SKIP_COUNTER} ");
    metrics
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .expect("drain skip counter sample must be present")
        .parse()
        .expect("drain skip counter sample must be an unsigned integer")
}

/// READY then one award-queueing message, returning the dispatch outcome and
/// the logs captured around the message only.
async fn run_message_dispatch(
    peer: &mut Peer,
    store: &GatewaySessionStore,
    task: JoinHandle<Result<(), sqlx::Error>>,
) -> (Outcome, String) {
    let auth = peer.authentication().await;
    assert_eq!(auth["op"], 2, "fresh fixture must IDENTIFY");
    peer.send(ready(&peer.url, "cause-session")).await;
    wait_sequence(store, 1).await;
    let start = capture().text().len();
    peer.send(message(2)).await;
    let outcome = await_outcome(store, task, 2).await;
    let logs = capture().text().split_off(start);
    (outcome, logs)
}

#[tokio::test]
async fn leveling_drain_missing_table_skips_with_metric_and_advances_cursor() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    sqlx::query("DROP TABLE member_season_xp")
        .execute(pool)
        .await
        .expect("drop seasonal table for 42P01");
    let fault = sqlx::query("SELECT count(*) FROM member_season_xp")
        .execute(pool)
        .await
        .expect_err("dropped table must fail");
    assert_eq!(
        crate::db_log::error_code(&fault).as_deref(),
        Some("42P01"),
        "drop must produce a missing-table gap"
    );
    let mut peer = Peer::new().await;
    let Running { store, stop, task } = start_runner(pool, &peer, pool).await;
    let _skip_guard = DRAIN_SKIP_GUARD.lock().await;
    let skips_before = drain_skip_count();
    let (outcome, logs) = run_message_dispatch(&mut peer, &store, task).await;
    match outcome {
        Outcome::Advanced(task) => stop_runner(stop, task).await,
        Outcome::Held(error) => {
            panic!("missing-table drain must skip, but the worker held the cursor: {error:?}")
        }
    }
    assert!(
        !lines_with(&logs, &["42P01", "leveling", "skip"]).is_empty(),
        "skip warn must name the SQLSTATE, got:\n{logs}"
    );
    let skips_after = drain_skip_count();
    assert_eq!(
        skips_after,
        skips_before + 1,
        "skipped drain must increment {SKIP_COUNTER} exactly once: before={skips_before}, after={skips_after}"
    );
    peer.close().await;
    db.close()
        .await
        .expect("drop dispatch-cause 42P01 fixture database");
}

#[tokio::test]
async fn leveling_drain_permission_gap_skips_with_metric_and_advances_cursor() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    // The fixture role is superuser, so no grant or RLS trick on the main
    // pool can fail with 42501. A dedicated NOLOGIN role with every grant
    // except seasonal writes produces a real permission gap on a dedicated
    // leveling pool (SET ROLE per connection, `pool_with_search_path` shape);
    // the checkpoint pool stays superuser so the skip can commit. The role
    // name is fixed and creation is duplicate-tolerant: a leaked role from
    // an aborted run is simply reused with the same grants.
    for ddl in [
        "DO $$ BEGIN CREATE ROLE dispatch_cause_gap_ro NOLOGIN; EXCEPTION WHEN duplicate_object THEN NULL; END $$",
        "GRANT dispatch_cause_gap_ro TO CURRENT_USER",
        "GRANT USAGE ON SCHEMA public TO dispatch_cause_gap_ro",
        "GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO dispatch_cause_gap_ro",
        "REVOKE INSERT, UPDATE, DELETE, TRUNCATE ON member_season_xp FROM dispatch_cause_gap_ro",
        "GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO dispatch_cause_gap_ro",
    ] {
        sqlx::query(ddl)
            .execute(pool)
            .await
            .expect("gap role setup");
    }
    let role_pool = PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(Duration::from_secs(10))
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::query("SET ROLE dispatch_cause_gap_ro")
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect_lazy_with(pool.connect_options().as_ref().clone());
    let fault = sqlx::query("INSERT INTO member_season_xp (guild_id, season_id, member_id, seasonal_xp, updated_at) VALUES ('canary','canary','canary',1,now())")
        .execute(&role_pool)
        .await
        .expect_err("canary insert must fail");
    assert_eq!(
        crate::db_log::error_code(&fault).as_deref(),
        Some("42501"),
        "role fault must be a permission gap"
    );
    let mut peer = Peer::new().await;
    let Running { store, stop, task } = start_runner(pool, &peer, &role_pool).await;
    let _skip_guard = DRAIN_SKIP_GUARD.lock().await;
    let skips_before = drain_skip_count();
    let (outcome, logs) = run_message_dispatch(&mut peer, &store, task).await;
    match outcome {
        Outcome::Advanced(task) => stop_runner(stop, task).await,
        Outcome::Held(error) => {
            panic!("permission-gap drain must skip, but the worker held the cursor: {error:?}")
        }
    }
    assert!(
        !lines_with(&logs, &["42501", "leveling", "skip"]).is_empty(),
        "skip warn must name the SQLSTATE, got:\n{logs}"
    );
    let skips_after = drain_skip_count();
    assert_eq!(
        skips_after,
        skips_before + 1,
        "skipped drain must increment {SKIP_COUNTER} exactly once: before={skips_before}, after={skips_after}"
    );
    peer.close().await;
    role_pool.close().await;
    db.close()
        .await
        .expect("drop dispatch-cause 42501 fixture database");
}

#[tokio::test]
async fn leveling_drain_check_violation_holds_cursor_with_cause_and_sanitized_logs() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    // `CHECK (false)` fails every seasonal insert with 23514. The constraint
    // name rides in the raw Postgres error text, so it doubles as the leak
    // canary: sanitized logs must never contain it.
    // The constraint name is the fixed `LEAK_MARKER` const (alphanumeric
    // plus underscores), so this is audited-static SQL like `TestDb::create`.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "ALTER TABLE member_season_xp ADD CONSTRAINT {LEAK_MARKER} CHECK (false)"
    )))
    .execute(pool)
    .await
    .expect("add failing check");
    let fault = sqlx::query("INSERT INTO member_season_xp (guild_id, season_id, member_id, seasonal_xp, updated_at) VALUES ('canary','canary','canary',1,now())")
        .execute(pool)
        .await
        .expect_err("check must fail");
    assert_eq!(
        crate::db_log::error_code(&fault).as_deref(),
        Some("23514"),
        "failing check must be a non-gap fault"
    );
    assert!(
        format!("{fault:?}").contains(LEAK_MARKER),
        "raw error must carry the canary"
    );
    let mut peer = Peer::new().await;
    // Keep the shutdown sender alive: dropping it resolves the runner's
    // shutdown watch at once and the shard would never connect.
    let Running {
        store,
        stop: _stop,
        task,
    } = start_runner(pool, &peer, pool).await;
    let (outcome, logs) = run_message_dispatch(&mut peer, &store, task).await;
    match outcome {
        Outcome::Held(error) => {
            assert_eq!(
                crate::db_log::error_code(&error).as_deref(),
                Some("23514"),
                "non-gap drain failure must carry its SQLSTATE"
            );
            assert!(
                !crate::gateway_failure::is_schema_gap(&error),
                "23514 must stay a non-gap hold"
            );
        }
        Outcome::Advanced(task) => {
            task.abort();
            panic!("check-violation drain must hold the cursor");
        }
    }
    let saved = store.load().await.expect("load").expect("checkpoint row");
    assert_eq!(saved.sequence, 1, "held cursor must not advance past READY");
    assert!(
        !lines_with(&logs, &["23514", "leveling"]).is_empty(),
        "hold path must log the cause SQLSTATE, got:\n{logs}"
    );
    assert!(
        !logs.contains(LEAK_MARKER),
        "failure logs must not echo SQL error text"
    );
    assert!(
        !logs.contains("violates check constraint"),
        "failure logs must not echo SQL error text"
    );
    peer.close().await;
    db.close()
        .await
        .expect("drop dispatch-cause 23514 fixture database");
}

#[tokio::test]
async fn leveling_drain_success_commits_and_writes_season_xp() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    let mut peer = Peer::new().await;
    let Running { store, stop, task } = start_runner(pool, &peer, pool).await;
    let (outcome, _) = run_message_dispatch(&mut peer, &store, task).await;
    match outcome {
        Outcome::Advanced(task) => stop_runner(stop, task).await,
        Outcome::Held(error) => panic!("healthy drain must commit: {error:?}"),
    }
    let seasons: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM member_season_xp WHERE guild_id = $1 AND member_id = $2",
    )
    .bind(CAUSE_GUILD)
    .bind(CAUSE_MEMBER)
    .fetch_one(pool)
    .await
    .expect("season rows");
    assert!(
        seasons >= 1,
        "the control drain must write the member's seasonal XP row"
    );
    peer.close().await;
    db.close()
        .await
        .expect("drop dispatch-cause healthy fixture database");
}

#[test]
fn dispatch_cause_skip_counter_and_alert_rule_are_documented() {
    assert!(
        METRICS_DOC.contains(SKIP_COUNTER),
        "docs/metrics.md must document {SKIP_COUNTER}"
    );
    assert!(
        METRICS_DOC.contains("gateway_drain_skips"),
        "docs/metrics.md must document the gateway_drain_skips alert rule"
    );
}
