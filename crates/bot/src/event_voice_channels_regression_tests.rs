#![cfg(test)]
//! Production-path tests for the event-voice orchestration: the worker
//! transition hook and both reconcile passes against scripted Discord
//! probes. The protected acceptance file pins the schema and the sink flow;
//! these pin the production claim ordering, retry bounds and instance
//! isolation the acceptance helpers cannot reach.

use super::*;
use crate::event_voice_channels::{
    claim_channel, flush_bootstrap_presence, reconcile_bound_channels, reconcile_stale_claims,
    record_transition, repair_guild_presence, ChannelProbeOutcome, EventProbeOutcome,
    EventVoiceProbe, ProbeError,
};
use std::collections::HashMap;
use std::sync::Mutex;
use time::{Duration, OffsetDateTime};
use two_bot_cutover::event_voice::{bind_event_voice_channel, mark_event_voice_orphaned};
use two_bot_cutover::voice_rooms::PgRoomStore;
use two_bot_testsupport::TestDatabase;

async fn ev_db() -> Option<TestDatabase> {
    let url = std::env::var("TWO_TEST_DATABASE_URL").ok()?;
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("disposable fixture database with the cutover migration set"),
    )
}

fn ev_instance(weeks: i64) -> OffsetDateTime {
    OffsetDateTime::now_utc() + Duration::weeks(weeks)
}

async fn ev_claim(pool: &sqlx::PgPool, event_id: &str, at: OffsetDateTime, lease: &str) -> bool {
    claim_channel(pool, event_id, at, at, None, lease)
        .await
        .expect("production claim runs")
}

async fn ev_backdate(pool: &sqlx::PgPool, event_id: &str, minutes: i64) {
    sqlx::query(
        "UPDATE event_voice_channels SET updated_at = NOW() - make_interval(mins => $1::integer) \
         WHERE event_id = $2",
    )
    .bind(minutes)
    .bind(event_id)
    .execute(pool)
    .await
    .expect("backdate runs");
}

async fn ev_row(pool: &sqlx::PgPool, event_id: &str) -> (String, Option<String>, Option<String>) {
    sqlx::query_as(
        "SELECT status, owner_tick_lease, voice_channel_id FROM event_voice_channels \
         WHERE event_id = $1",
    )
    .bind(event_id)
    .fetch_one(pool)
    .await
    .expect("row reads")
}

async fn ev_seed_mirror(pool: &sqlx::PgPool, guild: &str, event_id: &str) {
    sqlx::query(
        "INSERT INTO scheduled_events (guild_id, event_id, name, starts_at, updated_at) \
         VALUES ($1, $2, 'show', '2026-11-01T20:00:00.000Z', '2026-10-22T00:00:00.000Z')",
    )
    .bind(guild)
    .bind(event_id)
    .execute(pool)
    .await
    .expect("mirror seeds");
}

/// A trigger function that fails the write: targeted `WHEN` triggers built
/// on it force one production write to error while the rest succeed, so the
/// per-row skip arms are pinned deterministically.
async fn ev_fail_function(pool: &sqlx::PgPool) {
    sqlx::query(
        "CREATE FUNCTION ev_fail_write() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN RAISE EXCEPTION 'ev test write failure'; END; $$",
    )
    .execute(pool)
    .await
    .expect("fail function seeds");
}

/// Scripted Discord probe: records every GET and replays scripted
/// outcomes, so the tests pin exactly-once probing and the single retry.
struct ScriptProbe {
    events: HashMap<String, Result<EventProbeOutcome, ProbeError>>,
    channels: HashMap<u64, Result<ChannelProbeOutcome, ProbeError>>,
    retries: HashMap<u64, Result<ChannelProbeOutcome, ProbeError>>,
    event_gets: Mutex<Vec<String>>,
    channel_gets: Mutex<Vec<u64>>,
    retry_gets: Mutex<Vec<u64>>,
}

impl ScriptProbe {
    fn new() -> Self {
        Self {
            events: HashMap::new(),
            channels: HashMap::new(),
            retries: HashMap::new(),
            event_gets: Mutex::new(Vec::new()),
            channel_gets: Mutex::new(Vec::new()),
            retry_gets: Mutex::new(Vec::new()),
        }
    }
}

impl EventVoiceProbe for ScriptProbe {
    async fn get_event_channel(&self, event_id: &str) -> Result<EventProbeOutcome, ProbeError> {
        self.event_gets
            .lock()
            .expect("probe lock")
            .push(event_id.to_owned());
        self.events
            .get(event_id)
            .cloned()
            .expect("reconcile only probes scripted events")
    }

    async fn get_channel(&self, channel_id: Snowflake) -> Result<ChannelProbeOutcome, ProbeError> {
        self.channel_gets
            .lock()
            .expect("probe lock")
            .push(channel_id);
        self.channels
            .get(&channel_id)
            .cloned()
            .expect("reconcile only probes scripted channels")
    }

    async fn retry_get_channel(
        &self,
        channel_id: Snowflake,
    ) -> Result<ChannelProbeOutcome, ProbeError> {
        self.retry_gets.lock().expect("probe lock").push(channel_id);
        self.retries
            .get(&channel_id)
            .cloned()
            .expect("reconcile retries only scripted channels")
    }
}

#[tokio::test]
async fn production_claim_orders_insert_before_any_discord_call() {
    let Some(db) = ev_db().await else { return };
    let pool = db.pool();
    let at = ev_instance(2);
    // The production claim wraps the same INSERT .. ON CONFLICT DO NOTHING
    // the acceptance pins: first call wins, second loses, no Discord double.
    assert!(ev_claim(pool, "ev-pc", at, "tick-a").await);
    assert!(!ev_claim(pool, "ev-pc", at, "tick-b").await);
    assert_eq!(ev_row(pool, "ev-pc").await.1.as_deref(), Some("tick-a"));
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn stale_reconcile_binds_found_and_clears_missing() {
    let Some(db) = ev_db().await else { return };
    let pool = db.pool();
    let at = ev_instance(2);
    assert!(ev_claim(pool, "ev-rs-a", at, "tick-old-a").await);
    ev_backdate(pool, "ev-rs-a", 6).await;
    assert!(ev_claim(pool, "ev-rs-b", at, "tick-old-b").await);
    ev_backdate(pool, "ev-rs-b", 6).await;
    assert!(ev_claim(pool, "ev-rs-fresh", at, "tick-new").await);

    let mut probe = ScriptProbe::new();
    probe.events.insert(
        "ev-rs-a".to_owned(),
        Ok(EventProbeOutcome::FoundChannel(7801)),
    );
    probe
        .events
        .insert("ev-rs-b".to_owned(), Ok(EventProbeOutcome::NoChannel));
    let report = reconcile_stale_claims(pool, &probe, 300)
        .await
        .expect("reconcile runs");
    assert_eq!(report.probed, 2);
    assert_eq!(report.bound, 1);
    assert_eq!(report.cleared, 1);
    assert_eq!(report.skipped, 0);
    assert_eq!(
        probe.event_gets.lock().expect("probe lock").clone(),
        vec!["ev-rs-a".to_owned(), "ev-rs-b".to_owned()]
    );
    assert_eq!(ev_row(pool, "ev-rs-a").await.2.as_deref(), Some("7801"));
    assert_eq!(ev_row(pool, "ev-rs-b").await.1, None);
    // The cleared row survives for the next tick; the fresh row is untouched.
    assert_eq!(ev_row(pool, "ev-rs-b").await.0, "creating");
    assert_eq!(
        ev_row(pool, "ev-rs-fresh").await.1.as_deref(),
        Some("tick-new")
    );
    // The cleared row is claimable again: the next tick re-leases it and
    // owns the retry create.
    assert!(
        ev_claim(pool, "ev-rs-b", at, "tick-next").await,
        "a cleared claim must be re-leasable by the next tick"
    );
    assert_eq!(
        ev_row(pool, "ev-rs-b").await.1.as_deref(),
        Some("tick-next")
    );
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn stale_reconcile_binds_only_the_stale_instance() {
    let Some(db) = ev_db().await else { return };
    let pool = db.pool();
    let at = ev_instance(2);
    let at_b = ev_instance(3);
    assert!(ev_claim(pool, "ev-rs-i", at, "tick-old").await);
    ev_backdate(pool, "ev-rs-i", 6).await;
    // Same event, sibling instance, fresh lease: must stay unbound even
    // though the probe answers for the shared event id.
    sqlx::query(
        "INSERT INTO event_voice_channels (event_id, instance_starts_at, starts_at, ends_at, \
         status, owner_tick_lease, voice_channel_id, created_at, updated_at) \
         VALUES ('ev-rs-i', $1, $1, NULL, 'creating', 'tick-new', NULL, NOW(), NOW())",
    )
    .bind(at_b)
    .execute(pool)
    .await
    .expect("sibling instance seeds");

    let mut probe = ScriptProbe::new();
    probe.events.insert(
        "ev-rs-i".to_owned(),
        Ok(EventProbeOutcome::FoundChannel(7802)),
    );
    let report = reconcile_stale_claims(pool, &probe, 300)
        .await
        .expect("reconcile runs");
    assert_eq!(report.probed, 1);
    assert_eq!(report.bound, 1);
    let bound: Vec<(String,)> = sqlx::query_as(
        "SELECT voice_channel_id FROM event_voice_channels \
         WHERE event_id = 'ev-rs-i' AND voice_channel_id IS NOT NULL",
    )
    .fetch_all(pool)
    .await
    .expect("bound rows read");
    assert_eq!(bound.len(), 1, "exactly the stale instance binds");
    let sibling: (Option<String>,) = sqlx::query_as(
        "SELECT voice_channel_id FROM event_voice_channels \
         WHERE event_id = 'ev-rs-i' AND instance_starts_at = $1",
    )
    .bind(at_b)
    .fetch_one(pool)
    .await
    .expect("sibling reads");
    assert_eq!(sibling.0, None);
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn stale_reconcile_skips_rows_on_probe_transport_failure() {
    let Some(db) = ev_db().await else { return };
    let pool = db.pool();
    let at = ev_instance(2);
    assert!(ev_claim(pool, "ev-rs-err", at, "tick-old").await);
    ev_backdate(pool, "ev-rs-err", 6).await;

    let mut probe = ScriptProbe::new();
    probe.events.insert(
        "ev-rs-err".to_owned(),
        Err(ProbeError("timeout".to_owned())),
    );
    let report = reconcile_stale_claims(pool, &probe, 300)
        .await
        .expect("reconcile runs");
    assert_eq!(report.probed, 1);
    assert_eq!(report.skipped, 1);
    assert_eq!(report.bound, 0);
    assert_eq!(report.cleared, 0);
    assert_eq!(
        ev_row(pool, "ev-rs-err").await.1.as_deref(),
        Some("tick-old"),
        "an unanswered probe leaves the row untouched for the next tick"
    );
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn bound_reconcile_orphans_404_with_exactly_one_retry() {
    let Some(db) = ev_db().await else { return };
    let pool = db.pool();
    let at = ev_instance(2);
    assert!(ev_claim(pool, "ev-ro", at, "tick-a").await);
    assert!(bind_event_voice_channel(pool, "ev-ro", at, "7811")
        .await
        .expect("bind runs"));

    let mut probe = ScriptProbe::new();
    probe
        .channels
        .insert(7811, Ok(ChannelProbeOutcome::Missing));
    probe.retries.insert(7811, Ok(ChannelProbeOutcome::Missing));
    let report = reconcile_bound_channels(pool, &probe)
        .await
        .expect("reconcile runs");
    assert_eq!(report.probed, 1);
    assert_eq!(report.orphaned, 1);
    assert_eq!(report.revived, 0);
    assert_eq!(
        probe.retry_gets.lock().expect("probe lock").clone(),
        vec![7811],
        "the 404 row gets exactly one retry GET"
    );
    let row = ev_row(pool, "ev-ro").await;
    assert_eq!(row.0, "orphaned");
    assert_eq!(row.1, None, "the lease clears for one retry");
    assert_eq!(
        row.2.as_deref(),
        Some("7811"),
        "the dead id stays as evidence"
    );
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn bound_reconcile_revives_a_transient_404() {
    let Some(db) = ev_db().await else { return };
    let pool = db.pool();
    let at = ev_instance(2);
    assert!(ev_claim(pool, "ev-ro-t", at, "tick-a").await);
    assert!(bind_event_voice_channel(pool, "ev-ro-t", at, "7812")
        .await
        .expect("bind runs"));

    let mut probe = ScriptProbe::new();
    probe
        .channels
        .insert(7812, Ok(ChannelProbeOutcome::Missing));
    probe
        .retries
        .insert(7812, Ok(ChannelProbeOutcome::Exists { occupied: true }));
    let report = reconcile_bound_channels(pool, &probe)
        .await
        .expect("reconcile runs");
    assert_eq!(report.orphaned, 1);
    assert_eq!(report.revived, 1);
    let row = ev_row(pool, "ev-ro-t").await;
    assert_eq!(row.0, "creating");
    assert_eq!(
        row.1, None,
        "no re-lease follows a revive: the binding is live"
    );
    assert_eq!(row.2.as_deref(), Some("7812"));
    assert!(
        !ev_claim(pool, "ev-ro-t", at, "tick-next").await,
        "a revived row keeps its live binding; no tick double-creates"
    );
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn bound_reconcile_never_touches_live_rooms() {
    let Some(db) = ev_db().await else { return };
    let pool = db.pool();
    let at = ev_instance(2);
    assert!(ev_claim(pool, "ev-ro-occ", at, "tick-a").await);
    assert!(bind_event_voice_channel(pool, "ev-ro-occ", at, "7821")
        .await
        .expect("bind runs"));
    assert!(ev_claim(pool, "ev-ro-empty", at, "tick-b").await);
    assert!(bind_event_voice_channel(pool, "ev-ro-empty", at, "7822")
        .await
        .expect("bind runs"));

    let mut probe = ScriptProbe::new();
    probe
        .channels
        .insert(7821, Ok(ChannelProbeOutcome::Exists { occupied: true }));
    probe
        .channels
        .insert(7822, Ok(ChannelProbeOutcome::Exists { occupied: false }));
    let report = reconcile_bound_channels(pool, &probe)
        .await
        .expect("reconcile runs");
    assert_eq!(report.probed, 2);
    assert_eq!(report.kept, 2);
    assert_eq!(report.orphaned, 0);
    assert!(
        probe.retry_gets.lock().expect("probe lock").is_empty(),
        "live rooms get no retry GET"
    );
    assert_eq!(ev_row(pool, "ev-ro-occ").await.0, "creating");
    assert_eq!(ev_row(pool, "ev-ro-occ").await.1.as_deref(), Some("tick-a"));
    assert_eq!(ev_row(pool, "ev-ro-occ").await.2.as_deref(), Some("7821"));
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn bound_reconcile_skips_rows_on_probe_transport_failure() {
    let Some(db) = ev_db().await else { return };
    let pool = db.pool();
    let at = ev_instance(2);
    assert!(ev_claim(pool, "ev-ro-err", at, "tick-a").await);
    assert!(bind_event_voice_channel(pool, "ev-ro-err", at, "7831")
        .await
        .expect("bind runs"));

    let mut probe = ScriptProbe::new();
    probe
        .channels
        .insert(7831, Err(ProbeError("gateway 502".to_owned())));
    let report = reconcile_bound_channels(pool, &probe)
        .await
        .expect("reconcile runs");
    assert_eq!(report.probed, 1);
    assert_eq!(report.skipped, 1);
    assert_eq!(report.orphaned, 0);
    assert_eq!(ev_row(pool, "ev-ro-err").await.0, "creating");
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn stale_reconcile_skips_conflicting_bind_and_continues() {
    let Some(db) = ev_db().await else { return };
    let pool = db.pool();
    let at = ev_instance(2);
    assert!(ev_claim(pool, "ev-rs-cf-a", at, "tick-old-a").await);
    ev_backdate(pool, "ev-rs-cf-a", 6).await;
    assert!(ev_claim(pool, "ev-rs-cf-b", at, "tick-old-b").await);
    ev_backdate(pool, "ev-rs-cf-b", 6).await;
    // A third row already holds 7871: the probe answer for A collides with
    // the UNIQUE (voice_channel_id) binding, which must skip A, not abort.
    assert!(ev_claim(pool, "ev-rs-cf-c", at, "tick-c").await);
    assert!(bind_event_voice_channel(pool, "ev-rs-cf-c", at, "7871")
        .await
        .expect("bind runs"));

    let mut probe = ScriptProbe::new();
    probe.events.insert(
        "ev-rs-cf-a".to_owned(),
        Ok(EventProbeOutcome::FoundChannel(7871)),
    );
    probe.events.insert(
        "ev-rs-cf-b".to_owned(),
        Ok(EventProbeOutcome::FoundChannel(7872)),
    );
    let report = reconcile_stale_claims(pool, &probe, 300)
        .await
        .expect("a conflicting bind skips its row instead of aborting the pass");
    assert_eq!(report.probed, 2);
    assert_eq!(report.bound, 1);
    assert_eq!(report.skipped, 1);
    assert_eq!(ev_row(pool, "ev-rs-cf-a").await.2, None);
    assert_eq!(ev_row(pool, "ev-rs-cf-b").await.2.as_deref(), Some("7872"));
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn worker_transition_opens_and_closes_spans() {
    let Some(db) = ev_db().await else { return };
    let pool = db.pool();
    let store = PgRoomStore::new(pool.clone());
    let at = ev_instance(2);
    assert!(ev_claim(pool, "ev-wt", at, "tick-a").await);
    assert!(bind_event_voice_channel(pool, "ev-wt", at, "7841")
        .await
        .expect("bind runs"));

    record_transition(&store, GUILD, 3201, None, Some(7841)).await;
    let open: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM event_voice_presence \
         WHERE event_id = 'ev-wt' AND user_id = '3201' AND left_at IS NULL",
    )
    .fetch_one(pool)
    .await
    .expect("open count reads");
    assert_eq!(open, 1);
    // Same-channel frames are filtered before the call, but the helper is
    // idempotent anyway: no duplicate span.
    record_transition(&store, GUILD, 3201, Some(7841), Some(7841)).await;
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM event_voice_presence WHERE event_id = 'ev-wt' AND user_id = '3201'",
    )
    .fetch_one(pool)
    .await
    .expect("total reads");
    assert_eq!(total, 1);
    record_transition(&store, GUILD, 3201, Some(7841), None).await;
    let open_after: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM event_voice_presence \
         WHERE event_id = 'ev-wt' AND user_id = '3201' AND left_at IS NULL",
    )
    .fetch_one(pool)
    .await
    .expect("open count reads");
    assert_eq!(open_after, 0);
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn worker_transition_presence_writes_are_bounded_under_pool_starvation() {
    let Some(db) = ev_db().await else { return };
    let pool = db.pool();
    // Exhaust the fixture pool (testsupport fixes max_connections at 5):
    // every presence write then waits on acquisition past any sane bound,
    // so an unbounded write would hold the actor loop open for the full
    // acquire timeout (twice on a move). The exhaustion assert fails the
    // test loudly if the pool size ever changes, instead of passing
    // vacuously on an unstarved pool.
    let mut held = Vec::new();
    for _ in 0..5 {
        held.push(pool.acquire().await.expect("exhaust the fixture pool"));
    }
    assert!(
        pool.try_acquire().is_none(),
        "the fixture pool must be exhausted"
    );
    let store = PgRoomStore::new(pool.clone());
    tokio::time::timeout(
        std::time::Duration::from_secs(15),
        record_transition(&store, GUILD, 3203, Some(7861), Some(7862)),
    )
    .await
    .expect("saturated-pool presence writes stay bounded");
    drop(held);
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn worker_transition_move_closes_the_old_span_first() {
    let Some(db) = ev_db().await else { return };
    let pool = db.pool();
    let store = PgRoomStore::new(pool.clone());
    let at = ev_instance(2);
    assert!(ev_claim(pool, "ev-wm-a", at, "tick-a").await);
    assert!(bind_event_voice_channel(pool, "ev-wm-a", at, "7851")
        .await
        .expect("bind runs"));
    assert!(ev_claim(pool, "ev-wm-b", at, "tick-b").await);
    assert!(bind_event_voice_channel(pool, "ev-wm-b", at, "7852")
        .await
        .expect("bind runs"));

    record_transition(&store, GUILD, 3202, None, Some(7851)).await;
    record_transition(&store, GUILD, 3202, Some(7851), Some(7852)).await;
    let open_a: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM event_voice_presence \
         WHERE event_id = 'ev-wm-a' AND user_id = '3202' AND left_at IS NULL",
    )
    .fetch_one(pool)
    .await
    .expect("open count reads");
    let open_b: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM event_voice_presence \
         WHERE event_id = 'ev-wm-b' AND user_id = '3202' AND left_at IS NULL",
    )
    .fetch_one(pool)
    .await
    .expect("open count reads");
    assert_eq!((open_a, open_b), (0, 1));
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn orphaned_instance_is_claimable_and_rebinds_a_replacement() {
    let Some(db) = ev_db().await else { return };
    let pool = db.pool();
    let at = ev_instance(2);
    assert!(ev_claim(pool, "ev-ro-r", at, "tick-a").await);
    assert!(bind_event_voice_channel(pool, "ev-ro-r", at, "7881")
        .await
        .expect("bind runs"));
    mark_event_voice_orphaned(pool, "ev-ro-r", at)
        .await
        .expect("orphan runs");

    // The retry tick re-leases the orphaned row and owns the replacement
    // create; the dead channel id stays until the rebind lands.
    assert!(
        ev_claim(pool, "ev-ro-r", at, "tick-retry").await,
        "an orphaned instance must be claimable for its retry"
    );
    let row = ev_row(pool, "ev-ro-r").await;
    assert_eq!(row.0, "orphaned");
    assert_eq!(row.1.as_deref(), Some("tick-retry"));
    assert_eq!(row.2.as_deref(), Some("7881"));
    assert!(
        bind_event_voice_channel(pool, "ev-ro-r", at, "7882")
            .await
            .expect("rebind runs"),
        "an orphaned row must accept its replacement channel"
    );
    let rebound = ev_row(pool, "ev-ro-r").await;
    assert_eq!(rebound.0, "creating");
    assert_eq!(rebound.1.as_deref(), Some("tick-retry"));
    assert_eq!(rebound.2.as_deref(), Some("7882"));
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn stale_pass_recovers_an_orphaned_row_with_a_dead_lease() {
    let Some(db) = ev_db().await else { return };
    let pool = db.pool();
    let at = ev_instance(2);
    assert!(ev_claim(pool, "ev-rs-or", at, "tick-a").await);
    assert!(bind_event_voice_channel(pool, "ev-rs-or", at, "7883")
        .await
        .expect("bind runs"));
    mark_event_voice_orphaned(pool, "ev-rs-or", at)
        .await
        .expect("orphan runs");
    // The retry tick re-leased, then died before the replacement create.
    assert!(ev_claim(pool, "ev-rs-or", at, "tick-dead").await);
    ev_backdate(pool, "ev-rs-or", 6).await;

    let mut probe = ScriptProbe::new();
    probe
        .events
        .insert("ev-rs-or".to_owned(), Ok(EventProbeOutcome::NoChannel));
    let report = reconcile_stale_claims(pool, &probe, 300)
        .await
        .expect("reconcile runs");
    assert_eq!(report.probed, 1);
    assert_eq!(report.cleared, 1);
    assert_eq!(ev_row(pool, "ev-rs-or").await.1, None);
    assert!(
        ev_claim(pool, "ev-rs-or", at, "tick-next").await,
        "the recovered orphan is claimable again"
    );
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn stale_pass_binds_an_orphaned_row_to_a_live_channel() {
    let Some(db) = ev_db().await else { return };
    let pool = db.pool();
    let at = ev_instance(2);
    assert!(ev_claim(pool, "ev-rs-ol", at, "tick-a").await);
    assert!(bind_event_voice_channel(pool, "ev-rs-ol", at, "7884")
        .await
        .expect("bind runs"));
    mark_event_voice_orphaned(pool, "ev-rs-ol", at)
        .await
        .expect("orphan runs");
    ev_backdate(pool, "ev-rs-ol", 6).await;

    // The event names a live channel again (the 404 was transient): the
    // stale pass binds it straight onto the orphaned row, no create needed.
    let mut probe = ScriptProbe::new();
    probe.events.insert(
        "ev-rs-ol".to_owned(),
        Ok(EventProbeOutcome::FoundChannel(7885)),
    );
    let report = reconcile_stale_claims(pool, &probe, 300)
        .await
        .expect("reconcile runs");
    assert_eq!(report.probed, 1);
    assert_eq!(report.bound, 1);
    let row = ev_row(pool, "ev-rs-ol").await;
    assert_eq!(row.0, "creating");
    assert_eq!(row.2.as_deref(), Some("7885"));
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn revive_write_failure_skips_the_row_not_the_pass() {
    let Some(db) = ev_db().await else { return };
    let pool = db.pool();
    let at = ev_instance(2);
    assert!(ev_claim(pool, "ev-ro-f1", at, "tick-a").await);
    assert!(bind_event_voice_channel(pool, "ev-ro-f1", at, "7891")
        .await
        .expect("bind runs"));
    assert!(ev_claim(pool, "ev-ro-f2", at, "tick-b").await);
    assert!(bind_event_voice_channel(pool, "ev-ro-f2", at, "7892")
        .await
        .expect("bind runs"));
    ev_fail_function(pool).await;
    sqlx::query(
        "CREATE TRIGGER trg_ev_fail_revive BEFORE UPDATE ON event_voice_channels \
         FOR EACH ROW WHEN (OLD.event_id = 'ev-ro-f1' AND OLD.status = 'orphaned' \
         AND NEW.status = 'creating') EXECUTE FUNCTION ev_fail_write()",
    )
    .execute(pool)
    .await
    .expect("revive-fail trigger seeds");

    // Row one 404s, then the retry GET finds it alive again, but the revive
    // write fails: the row is skipped (still counted orphaned) and row two
    // is still probed.
    let mut probe = ScriptProbe::new();
    probe
        .channels
        .insert(7891, Ok(ChannelProbeOutcome::Missing));
    probe
        .retries
        .insert(7891, Ok(ChannelProbeOutcome::Exists { occupied: true }));
    probe
        .channels
        .insert(7892, Ok(ChannelProbeOutcome::Exists { occupied: false }));
    let report = reconcile_bound_channels(pool, &probe)
        .await
        .expect("a failed revive skips its row instead of aborting the pass");
    assert_eq!(report.probed, 2);
    assert_eq!(report.orphaned, 1);
    assert_eq!(report.revived, 0);
    assert_eq!(report.skipped, 1);
    assert_eq!(report.kept, 1);
    let row = ev_row(pool, "ev-ro-f1").await;
    assert_eq!(row.0, "orphaned");
    assert_eq!(row.2.as_deref(), Some("7891"));
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn clear_write_failure_skips_the_row_not_the_pass() {
    let Some(db) = ev_db().await else { return };
    let pool = db.pool();
    let at = ev_instance(2);
    assert!(ev_claim(pool, "ev-rs-f1", at, "tick-old-1").await);
    ev_backdate(pool, "ev-rs-f1", 6).await;
    assert!(ev_claim(pool, "ev-rs-f2", at, "tick-old-2").await);
    ev_backdate(pool, "ev-rs-f2", 6).await;
    ev_fail_function(pool).await;
    sqlx::query(
        "CREATE TRIGGER trg_ev_fail_clear BEFORE UPDATE ON event_voice_channels \
         FOR EACH ROW WHEN (OLD.event_id = 'ev-rs-f1' \
         AND OLD.owner_tick_lease IS NOT NULL AND NEW.owner_tick_lease IS NULL) \
         EXECUTE FUNCTION ev_fail_write()",
    )
    .execute(pool)
    .await
    .expect("clear-fail trigger seeds");

    let mut probe = ScriptProbe::new();
    probe
        .events
        .insert("ev-rs-f1".to_owned(), Ok(EventProbeOutcome::NoChannel));
    probe
        .events
        .insert("ev-rs-f2".to_owned(), Ok(EventProbeOutcome::NoChannel));
    let report = reconcile_stale_claims(pool, &probe, 300)
        .await
        .expect("a failed clear skips its row instead of aborting the pass");
    assert_eq!(report.probed, 2);
    assert_eq!(report.cleared, 1);
    assert_eq!(report.skipped, 1);
    assert_eq!(
        ev_row(pool, "ev-rs-f1").await.1.as_deref(),
        Some("tick-old-1"),
        "the failed clear keeps the dead lease for the next pass"
    );
    assert_eq!(ev_row(pool, "ev-rs-f2").await.1, None);
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn failed_leave_is_closed_by_the_guild_repair() {
    let Some(db) = ev_db().await else { return };
    let pool = db.pool();
    let store = PgRoomStore::new(pool.clone());
    let at = ev_instance(2);
    assert!(ev_claim(pool, "ev-rp-a", at, "tick-a").await);
    assert!(bind_event_voice_channel(pool, "ev-rp-a", at, "7901")
        .await
        .expect("bind runs"));
    ev_seed_mirror(pool, &GUILD.to_string(), "ev-rp-a").await;

    record_transition(&store, GUILD, 3212, None, Some(7901)).await;
    ev_fail_function(pool).await;
    sqlx::query(
        "CREATE TRIGGER trg_ev_fail_leave BEFORE UPDATE ON event_voice_presence \
         FOR EACH ROW WHEN (OLD.user_id = '3212' AND NEW.left_at IS NOT NULL) \
         EXECUTE FUNCTION ev_fail_write()",
    )
    .execute(pool)
    .await
    .expect("leave-fail trigger seeds");
    // The leave write fails (saturated pool, timeout): the helper still
    // returns normally and the span stays open.
    record_transition(&store, GUILD, 3212, Some(7901), None).await;
    let open: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM event_voice_presence \
         WHERE event_id = 'ev-rp-a' AND user_id = '3212' AND left_at IS NULL",
    )
    .fetch_one(pool)
    .await
    .expect("open count reads");
    assert_eq!(open, 1);
    sqlx::query("DROP TRIGGER trg_ev_fail_leave ON event_voice_presence")
        .execute(pool)
        .await
        .expect("trigger drops");

    // The member never returns: the repair closes the span against the
    // empty snapshot instead of merging their absence into a later span.
    repair_guild_presence(&store, GUILD, &[]).await;
    let closed: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM event_voice_presence \
         WHERE event_id = 'ev-rp-a' AND user_id = '3212' AND left_at IS NOT NULL",
    )
    .fetch_one(pool)
    .await
    .expect("closed count reads");
    assert_eq!(
        closed, 1,
        "the repair closes the lost leave, adding no span"
    );
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn failed_join_is_reopened_by_the_guild_repair() {
    let Some(db) = ev_db().await else { return };
    let pool = db.pool();
    let store = PgRoomStore::new(pool.clone());
    let at = ev_instance(2);
    assert!(ev_claim(pool, "ev-rp-b", at, "tick-a").await);
    assert!(bind_event_voice_channel(pool, "ev-rp-b", at, "7902")
        .await
        .expect("bind runs"));
    ev_seed_mirror(pool, &GUILD.to_string(), "ev-rp-b").await;
    ev_fail_function(pool).await;
    sqlx::query(
        "CREATE TRIGGER trg_ev_fail_join BEFORE INSERT ON event_voice_presence \
         FOR EACH ROW WHEN (NEW.user_id = '3213') EXECUTE FUNCTION ev_fail_write()",
    )
    .execute(pool)
    .await
    .expect("join-fail trigger seeds");

    record_transition(&store, GUILD, 3213, None, Some(7902)).await;
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM event_voice_presence \
         WHERE event_id = 'ev-rp-b' AND user_id = '3213'",
    )
    .fetch_one(pool)
    .await
    .expect("total reads");
    assert_eq!(total, 0, "the failed join writes nothing");
    sqlx::query("DROP TRIGGER trg_ev_fail_join ON event_voice_presence")
        .execute(pool)
        .await
        .expect("trigger drops");

    // The member is still sitting in the channel: the repair re-opens the
    // span, exactly once.
    repair_guild_presence(&store, GUILD, &[(3213, 7902)]).await;
    let open: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM event_voice_presence \
         WHERE event_id = 'ev-rp-b' AND user_id = '3213' AND left_at IS NULL",
    )
    .fetch_one(pool)
    .await
    .expect("open count reads");
    assert_eq!(open, 1);
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn bootstrap_flush_is_bounded_in_total_under_pool_starvation() {
    let Some(db) = ev_db().await else { return };
    let pool = db.pool();
    // Exhaust the fixture pool (testsupport fixes max_connections at 5):
    // every presence write then waits on acquisition past its bound, so an
    // unbounded flush would cost one timeout per occupant.
    let mut held = Vec::new();
    for _ in 0..5 {
        held.push(pool.acquire().await.expect("exhaust the fixture pool"));
    }
    assert!(
        pool.try_acquire().is_none(),
        "the fixture pool must be exhausted"
    );
    let store = PgRoomStore::new(pool.clone());
    let occupants: Vec<(u64, u64)> = (0..10).map(|i| (3300 + i, 7910 + i)).collect();
    let started = std::time::Instant::now();
    let report = flush_bootstrap_presence(&store, GUILD, &occupants).await;
    let elapsed = started.elapsed();
    assert_eq!(report.attempted + report.skipped, 10);
    assert!(
        report.skipped >= 1,
        "a starved flush must shed load past its budget"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(8),
        "ten starved occupants must flush under one total deadline, took {elapsed:?}"
    );
    drop(held);
    db.close().await.expect("fixture teardown");
}
