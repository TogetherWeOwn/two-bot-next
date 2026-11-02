//! Funnel sync-store DB containment acceptance (P7).
//!
//! PROTECTED TEST FILE — the paired Builder builds on this branch and must
//! never edit this file. Corrections return to the Test Author.
//!
//! Baseline `5aa5ebd80`. The sync store seam (`FunnelStore`,
//! `MembershipStore`, `InviteSnapshotStore`) has no fallible channel, so a
//! transient DB error panics the per-event hot task instead of degrading.
//! These tests pin the contained contract:
//!
//! * Reads degrade fail-closed after bounded retries, recording one error in
//!   `two_bot_db_errors_total`: `has_event`→`false`,
//!   `next_message_rung`→`None`, `membership`→`None`,
//!   `membership_rows`→empty, snapshot `load`→empty.
//! * Writes panic only after retries are exhausted; snapshot write panics
//!   keep their fixed-vocabulary messages.
//! * The invite-`uses` `i32` cast saturates instead of panicking.
//! * Healthy reads/writes are byte-identical (controls, green before/after).
//!
//! Fault model: each blip test points a lazily-connected pool at a loopback
//! TCP server that accepts and immediately drops every connection. That is a
//! genuine DB blip (server vanished mid-handshake): sqlx surfaces the
//! handshake EOF/reset immediately with no internal retry, so every observed
//! connection attempt is one caller-level attempt and the attempt count
//! proves retry-before-degrade/panic. No database, no network beyond
//! loopback, no sleeps, no timing assertions.
//!
//! On current main every blip test fails (reads panic out of the task,
//! writes panic on the first attempt, hostile `uses` panics the cast) while
//! the healthy controls pass. On a correct implementation the whole file
//! passes.
//!
//! Run the offline blip half with no database configured:
//! `cargo test -p two-bot-store --locked --test funnel_db_containment_acceptance blip`.
//! The `uses` saturation and healthy round-trip tests need the disposable
//! test database (`TWO_TEST_DATABASE_URL`, agent-testdb only) and skip
//! loudly when it is unset.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Pool, Postgres};
use two_bot_core::{
    analytics_family::AnalyticsCategory, membership::MembershipStore, EventType, FunnelEvent,
    FunnelStore, InviteSnapshotStore, InviteState,
};
use two_bot_store::{PgFunnelStore, PgInviteSnapshots};

const GUILD: u64 = 1_770_101_001;
const MEMBER: u64 = 1_770_102_002;
const OTHER_MEMBER: u64 = 1_770_102_003;

/// Serializes the blip tests so each error-metric window contains exactly one
/// store operation: with every blip test holding this lock, a metric delta
/// is attributable to that test's single call, never to a parallel test.
/// Acquire it through [`blip_guard`], never directly.
static BLIP_LOCK: Mutex<()> = Mutex::new(());

/// Acquire the blip serialisation lock, recovering from poisoning. On an
/// uncontained implementation the store call panics while the test holds the
/// guard, which poisons the mutex; without recovery that first panic would
/// cascade `PoisonError` into every other blip test instead of letting each
/// site fail on its own assertion. Recovery keeps the serialisation (only
/// one holder at a time) while giving each site an independent signal.
fn blip_guard() -> std::sync::MutexGuard<'static, ()> {
    BLIP_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Loopback server that drops every connection the instant it is accepted,
/// counting attempts. Fully offline: no database, no real Postgres.
struct BlipServer {
    port: u16,
    attempts: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl BlipServer {
    async fn drop_all() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind blip server");
        let port = listener.local_addr().expect("blip server address").port();
        let attempts = Arc::new(AtomicUsize::new(0));
        let count = attempts.clone();
        let task = tokio::spawn(async move {
            loop {
                let accepted = listener.accept().await;
                let Ok((socket, _)) = accepted else {
                    break;
                };
                count.fetch_add(1, Ordering::Relaxed);
                drop(socket);
            }
        });
        Self {
            port,
            attempts,
            task,
        }
    }

    fn attempts(&self) -> usize {
        self.attempts.load(Ordering::Relaxed)
    }
}

impl Drop for BlipServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Lazily-connected pool aimed at the blip server. `connect_lazy_with`
/// performs no I/O, so pool construction never touches the network; the
/// first store call drives the first dropped handshake.
fn blip_pool(port: u16) -> Pool<Postgres> {
    let options = PgConnectOptions::new()
        .host("127.0.0.1")
        .port(port)
        .username("agent_test")
        .database("two_bot_test_blip");
    PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect_lazy_with(options)
}

/// Sum of every `two_bot_db_errors_total{op=...}` counter. Summing across ops
/// keeps these tests independent of the exact op vocabulary the Builder
/// documents; the per-site attribution comes from [`BLIP_LOCK`], not labels.
fn db_error_total() -> u64 {
    two_bot_core::metrics::global()
        .render(None)
        .lines()
        .filter_map(|line| line.strip_prefix("two_bot_db_errors_total{"))
        .filter_map(|rest| rest.rsplit(' ').next())
        .filter_map(|count| count.parse::<u64>().ok())
        .sum()
}

fn join_event(member: u64, at: &str) -> FunnelEvent {
    FunnelEvent {
        guild_id: GUILD,
        member_id: Some(member),
        event_type: EventType::MemberJoin,
        occurred_at: at.to_owned(),
        source: "funnel-blip-acceptance".to_owned(),
        metadata: None,
        dedupe_token: None,
    }
}

fn message_event(member: u64, rung: EventType, at: &str) -> FunnelEvent {
    FunnelEvent {
        guild_id: GUILD,
        member_id: Some(member),
        event_type: rung,
        occurred_at: at.to_owned(),
        source: format!("channel:{GUILD}"),
        metadata: None,
        dedupe_token: None,
    }
}

/// Extract the panic message from a caught unwind for fixed-vocabulary pins.
fn panic_message(error: Box<dyn std::any::Any + Send>) -> String {
    error
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| {
            error
                .downcast_ref::<&str>()
                .map(|message| (*message).to_owned())
        })
        .unwrap_or_default()
}

// --- read degradation: fail closed after retry, with an error metric ---

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn has_event_db_blip_degrades_false_with_retry_and_metric() {
    let server = BlipServer::drop_all().await;
    let _blip = blip_guard();
    let store = PgFunnelStore::new(blip_pool(server.port));
    let before = db_error_total();
    let outcome = store.has_event(GUILD, MEMBER, EventType::MemberJoin);
    assert!(
        !outcome,
        "a DB blip must degrade has_event fail-closed to false, never panic"
    );
    assert!(
        server.attempts() >= 2,
        "has_event must retry the blip before degrading (saw {} attempt(s))",
        server.attempts()
    );
    assert!(
        db_error_total() > before,
        "a degraded has_event must record a db error"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn next_message_rung_db_blip_degrades_none_with_retry_and_metric() {
    let server = BlipServer::drop_all().await;
    let _blip = blip_guard();
    let store = PgFunnelStore::new(blip_pool(server.port));
    let before = db_error_total();
    let outcome = store.next_message_rung(GUILD, MEMBER, "2026-10-21T00:00:00.000Z");
    assert!(
        outcome.is_none(),
        "a DB blip must degrade next_message_rung to None, never panic"
    );
    assert!(
        server.attempts() >= 2,
        "next_message_rung must retry the blip before degrading (saw {} attempt(s))",
        server.attempts()
    );
    assert!(
        db_error_total() > before,
        "a degraded next_message_rung must record a db error"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn membership_db_blip_degrades_none_with_retry_and_metric() {
    let server = BlipServer::drop_all().await;
    let _blip = blip_guard();
    let store = PgFunnelStore::new(blip_pool(server.port));
    let before = db_error_total();
    let outcome = store.membership(GUILD, MEMBER);
    assert!(
        outcome.is_none(),
        "a DB blip must degrade membership to None, never panic"
    );
    assert!(
        server.attempts() >= 2,
        "membership must retry the blip before degrading (saw {} attempt(s))",
        server.attempts()
    );
    assert!(
        db_error_total() > before,
        "a degraded membership must record a db error"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn membership_rows_db_blip_degrades_empty_with_retry_and_metric() {
    let server = BlipServer::drop_all().await;
    let _blip = blip_guard();
    let store = PgFunnelStore::new(blip_pool(server.port));
    let before = db_error_total();
    let outcome = store.membership_rows(GUILD, MEMBER);
    assert!(
        outcome.is_empty(),
        "a DB blip must degrade membership_rows to empty, never panic"
    );
    assert!(
        server.attempts() >= 2,
        "membership_rows must retry the blip before degrading (saw {} attempt(s))",
        server.attempts()
    );
    assert!(
        db_error_total() > before,
        "degraded membership_rows must record a db error"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_load_db_blip_degrades_empty_with_retry_and_metric() {
    let server = BlipServer::drop_all().await;
    let _blip = blip_guard();
    let store = PgInviteSnapshots::new(blip_pool(server.port));
    let before = db_error_total();
    let outcome = store.load(GUILD);
    assert!(
        outcome.is_empty(),
        "a DB blip must degrade snapshot load to empty, never panic"
    );
    assert!(
        server.attempts() >= 2,
        "snapshot load must retry the blip before degrading (saw {} attempt(s))",
        server.attempts()
    );
    assert!(
        db_error_total() > before,
        "a degraded snapshot load must record a db error"
    );
}

// --- write containment: panic only after retries are exhausted ---

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mark_bot_db_blip_retries_then_panics() {
    let server = BlipServer::drop_all().await;
    let _blip = blip_guard();
    let store = PgFunnelStore::new(blip_pool(server.port));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        store.mark_bot(GUILD, MEMBER);
    }));
    assert!(
        outcome.is_err(),
        "a persistent blip must still fail mark_bot"
    );
    assert!(
        server.attempts() >= 2,
        "mark_bot must retry the blip before panicking (saw {} attempt(s))",
        server.attempts()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn record_db_blip_retries_then_panics() {
    let server = BlipServer::drop_all().await;
    let _blip = blip_guard();
    let store = PgFunnelStore::new(blip_pool(server.port));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        store.record(join_event(MEMBER, "2026-10-21T00:00:00.000Z"));
    }));
    assert!(outcome.is_err(), "a persistent blip must still fail record");
    assert!(
        server.attempts() >= 2,
        "record must retry the blip before panicking (saw {} attempt(s))",
        server.attempts()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn touch_activity_db_blip_retries_then_panics() {
    let server = BlipServer::drop_all().await;
    let _blip = blip_guard();
    let store = PgFunnelStore::new(blip_pool(server.port));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        store.touch_activity(
            GUILD,
            MEMBER,
            "2026-10-21T00:00:00.000Z",
            AnalyticsCategory::Activity,
        );
    }));
    assert!(
        outcome.is_err(),
        "a persistent blip must still fail touch_activity"
    );
    assert!(
        server.attempts() >= 2,
        "touch_activity must retry the blip before panicking (saw {} attempt(s))",
        server.attempts()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn record_observed_db_blip_retries_then_panics() {
    let server = BlipServer::drop_all().await;
    let _blip = blip_guard();
    let store = PgFunnelStore::new(blip_pool(server.port));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        store.record_observed(
            join_event(MEMBER, "2026-10-21T00:00:00.000Z"),
            Some("2026-10-21T00:00:01.000000Z"),
        );
    }));
    assert!(
        outcome.is_err(),
        "a persistent blip must still fail record_observed"
    );
    assert!(
        server.attempts() >= 2,
        "record_observed must retry the blip before panicking (saw {} attempt(s))",
        server.attempts()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_store_all_db_blip_retries_then_panics_fixed_vocabulary() {
    let server = BlipServer::drop_all().await;
    let _blip = blip_guard();
    let store = PgInviteSnapshots::new(blip_pool(server.port));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        store.store_all(
            GUILD,
            &[InviteState {
                code: "blip".to_owned(),
                uses: 7,
                inviter_id: Some(MEMBER),
                channel_id: None,
                max_uses: 0,
                temporary: false,
            }],
        );
    }));
    let Err(error) = outcome else {
        panic!("a persistent blip must still fail snapshot store_all");
    };
    assert!(
        server.attempts() >= 2,
        "snapshot store_all must retry the blip before panicking (saw {} attempt(s))",
        server.attempts()
    );
    assert!(
        panic_message(error).contains("invite snapshot store_all failed"),
        "snapshot store_all must fail with its fixed-vocabulary message"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_delete_missing_db_blip_retries_then_panics_fixed_vocabulary() {
    let server = BlipServer::drop_all().await;
    let _blip = blip_guard();
    let store = PgInviteSnapshots::new(blip_pool(server.port));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        store.delete_missing(GUILD, &HashSet::new());
    }));
    let Err(error) = outcome else {
        panic!("a persistent blip must still fail snapshot delete_missing");
    };
    assert!(
        server.attempts() >= 2,
        "snapshot delete_missing must retry the blip before panicking (saw {} attempt(s))",
        server.attempts()
    );
    assert!(
        panic_message(error).contains("invite snapshot delete_missing failed"),
        "snapshot delete_missing must fail with its fixed-vocabulary message"
    );
}

// --- disposable test database: hostile uses saturation + healthy controls ---

/// Migrated scratch schema on the disposable test database. Mirrors the
/// store privacy-lock fixtures: `TWO_TEST_DATABASE_URL` only, agent-testdb
/// (or a CI service container) only, loud skip when unconfigured.
async fn migrated_schema(prefix: &str) -> Option<(Pool<Postgres>, String)> {
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure the funnel containment database suite"
            );
            eprintln!("SKIP funnel containment DB test: TWO_TEST_DATABASE_URL not set");
            return None;
        }
    };
    let options: PgConnectOptions = url.parse().expect("test database URL");
    let host = options.get_host();
    assert!(
        host == "agent-testdb"
            || (host == "localhost" && std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true")),
        "test-container-only URL"
    );
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let schema = format!("{prefix}_{nonce}_{}", std::process::id());
    assert!(
        !schema.is_empty()
            && schema.len() <= 63
            && schema
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
        "safe unique test schema"
    );
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone())
        .await
        .expect("test database admin");
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .expect("create owned test schema");
    admin.close().await;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect_with(options.options([("search_path", schema.as_str())]))
        .await
        .expect("test database pool");
    sqlx::migrate!("../cutover/migrations")
        .run(&pool)
        .await
        .expect("cutover migrations");
    Some((pool, schema))
}

async fn drop_schema(pool: &Pool<Postgres>, schema: &str) {
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(pool)
        .await
        .expect("clean up owned test schema");
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_store_all_hostile_uses_saturates_i32() {
    let Some((pool, schema)) = migrated_schema("funnel_blip_saturate").await else {
        return;
    };
    let store = PgInviteSnapshots::new(pool.clone());
    let hostile = u64::from(i32::MAX as u32) + 1;
    store.store_all(
        GUILD,
        &[InviteState {
            code: "hostile".to_owned(),
            uses: hostile,
            inviter_id: Some(MEMBER),
            channel_id: None,
            max_uses: 0,
            temporary: false,
        }],
    );
    let stored: i32 =
        sqlx::query_scalar("SELECT uses FROM invite_snapshots WHERE guild_id = $1 AND code = $2")
            .bind(GUILD.to_string())
            .bind("hostile")
            .fetch_one(&pool)
            .await
            .expect("read back saturated uses");
    assert_eq!(
        stored,
        i32::MAX,
        "uses beyond the i32 range must saturate, never panic"
    );
    let loaded = store.load(GUILD);
    let round_tripped = loaded
        .iter()
        .find(|state| state.code == "hostile")
        .map(|state| state.uses);
    assert_eq!(
        round_tripped,
        Some(u64::from(i32::MAX as u32)),
        "the saturated counter must round-trip through load"
    );
    drop_schema(&pool, &schema).await;
}

/// Healthy-path control: every funnel site round-trips exact values, so the
/// Builder's containment cannot change what healthy reads/writes return.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn healthy_funnel_reads_and_writes_round_trip() {
    let Some((pool, schema)) = migrated_schema("funnel_blip_healthy").await else {
        return;
    };
    let store = PgFunnelStore::new(pool.clone());
    assert!(
        !store.has_event(GUILD, MEMBER, EventType::MemberJoin),
        "no join row before the join is recorded"
    );
    assert!(store.membership(GUILD, MEMBER).is_none());
    assert!(store.membership_rows(GUILD, MEMBER).is_empty());
    assert_eq!(
        store.next_message_rung(GUILD, MEMBER, "2026-10-21T00:00:00.000Z"),
        Some(EventType::FirstMessage),
        "an empty ladder opens at the first rung"
    );

    let first = store.record(join_event(MEMBER, "2026-10-21T00:00:00.000Z"));
    assert!(first.inserted, "the first join wins the idempotency key");
    let duplicate = store.record(join_event(MEMBER, "2026-10-21T00:00:00.000Z"));
    assert!(
        !duplicate.inserted,
        "the same join twice is one idempotent row"
    );
    assert!(store.has_event(GUILD, MEMBER, EventType::MemberJoin));
    let membership = store
        .membership(GUILD, MEMBER)
        .expect("a recorded join projects a membership");
    assert!(
        membership.joined_at.is_some(),
        "membership carries the join timestamp"
    );
    assert_eq!(store.membership_rows(GUILD, MEMBER).len(), 1);

    let observed = store.record_observed(
        join_event(OTHER_MEMBER, "2026-10-21T00:00:00.000Z"),
        Some("2026-10-21T00:00:01.000000Z"),
    );
    assert!(
        observed.inserted,
        "observed inserts land like plain inserts"
    );

    store.touch_activity(
        GUILD,
        MEMBER,
        "2026-10-21T00:01:00.000Z",
        AnalyticsCategory::Activity,
    );
    store.mark_bot(GUILD, OTHER_MEMBER);
    let touched: i64 =
        sqlx::query_scalar("SELECT count(*) FROM members WHERE guild_id = $1 AND member_id = $2")
            .bind(GUILD.to_string())
            .bind(MEMBER.to_string())
            .fetch_one(&pool)
            .await
            .expect("members row exists after touch_activity");
    assert_eq!(touched, 1, "touch_activity persists the members row");

    assert_eq!(
        store.next_message_rung(GUILD, MEMBER, "2026-10-21T00:01:00.000Z"),
        Some(EventType::FirstMessage)
    );
    for (rung, at, next) in [
        (
            EventType::FirstMessage,
            "2026-10-21T00:02:00.000Z",
            Some(EventType::SecondMessage),
        ),
        (
            EventType::SecondMessage,
            "2026-10-21T00:03:00.000Z",
            Some(EventType::ThirdMessage),
        ),
        (EventType::ThirdMessage, "2026-10-21T00:04:00.000Z", None),
    ] {
        let outcome = store.record(message_event(MEMBER, rung, at));
        assert!(outcome.inserted, "each message rung fills once");
        assert_eq!(
            store.next_message_rung(GUILD, MEMBER, "2026-10-21T00:05:00.000Z"),
            next,
            "the ladder advances one rung per message"
        );
    }
    drop_schema(&pool, &schema).await;
}

/// Healthy-path control for the invite snapshot seam.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn healthy_snapshot_round_trip() {
    let Some((pool, schema)) = migrated_schema("funnel_blip_snapshots").await else {
        return;
    };
    let store = PgInviteSnapshots::new(pool.clone());
    assert!(store.load(GUILD).is_empty());
    store.store_all(
        GUILD,
        &[
            InviteState {
                code: "keep".to_owned(),
                uses: 7,
                inviter_id: Some(MEMBER),
                channel_id: Some(9_001),
                max_uses: 0,
                temporary: false,
            },
            InviteState {
                code: "prune".to_owned(),
                uses: 3,
                inviter_id: None,
                channel_id: None,
                max_uses: 0,
                temporary: false,
            },
        ],
    );
    let mut loaded = store.load(GUILD);
    loaded.sort_by(|left, right| left.code.cmp(&right.code));
    assert_eq!(loaded.len(), 2, "both snapshot rows round-trip");
    assert_eq!(loaded[0].code, "keep");
    assert_eq!(loaded[0].uses, 7);
    assert_eq!(loaded[0].inviter_id, Some(MEMBER));
    assert_eq!(loaded[0].channel_id, Some(9_001));

    store.delete_missing(GUILD, &HashSet::from(["keep".to_owned()]));
    let pruned = store.load(GUILD);
    assert_eq!(pruned.len(), 1, "delete_missing prunes only dead codes");
    assert_eq!(pruned[0].code, "keep");
    drop_schema(&pool, &schema).await;
}
