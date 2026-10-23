//! Production-path tests for the event voice SQL primitives: claim ordering,
//! stale/orphan selects and transitions, presence join/leave/flush, and
//! per-instance isolation. Only on the guarded disposable test service.

use sqlx::Row;
use time::{Duration, OffsetDateTime};
use two_bot_cutover::event_voice::{
    bind_event_voice_channel, bound_event_voice_channels, bound_event_voice_channels_for_guild,
    claim_event_voice_channel, clear_event_voice_lease, close_event_voice_spans_absent,
    flush_event_voice_presence, mark_event_voice_orphaned, record_event_voice_join,
    record_event_voice_leave, revive_event_voice_orphan, stale_event_voice_claims,
};
use two_bot_testsupport::TestDatabase;

async fn database() -> Option<TestDatabase> {
    let url = std::env::var("TWO_TEST_DATABASE_URL").ok()?;
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("./migrations"))
            .await
            .expect("disposable fixture database with the cutover migration set"),
    )
}

fn instance(offset_weeks: i64) -> OffsetDateTime {
    OffsetDateTime::now_utc() + Duration::weeks(offset_weeks)
}

async fn claim(pool: &sqlx::PgPool, event_id: &str, at: OffsetDateTime, lease: &str) -> bool {
    claim_event_voice_channel(pool, event_id, at, at, None, lease)
        .await
        .expect("claim runs")
}

async fn lease_of(pool: &sqlx::PgPool, event_id: &str) -> Option<String> {
    sqlx::query_scalar("SELECT owner_tick_lease FROM event_voice_channels WHERE event_id = $1")
        .bind(event_id)
        .fetch_one(pool)
        .await
        .expect("lease reads")
}

async fn backdate_updated(pool: &sqlx::PgPool, event_id: &str, minutes: i64) {
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

async fn span_counts(pool: &sqlx::PgPool, user_id: &str) -> (i64, i64) {
    let row: (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COUNT(*) FILTER (WHERE left_at IS NULL) \
         FROM event_voice_presence WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("span counts read");
    row
}

#[tokio::test]
async fn claim_first_wins_and_loser_keeps_the_winner_lease() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    let at = instance(2);
    assert!(claim(pool, "ev-1", at, "tick-a").await);
    assert!(!claim(pool, "ev-1", at, "tick-b").await);
    assert_eq!(lease_of(pool, "ev-1").await.as_deref(), Some("tick-a"));
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn concurrent_claims_yield_a_single_winner() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    let at = instance(2);
    let (won_a, won_b) = tokio::join!(
        claim_event_voice_channel(pool, "ev-2", at, at, None, "tick-a"),
        claim_event_voice_channel(pool, "ev-2", at, at, None, "tick-b"),
    );
    let (won_a, won_b) = (won_a.expect("claim runs"), won_b.expect("claim runs"));
    assert!(won_a ^ won_b, "exactly one concurrent claim wins");
    let rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM event_voice_channels WHERE event_id = 'ev-2'")
            .fetch_one(pool)
            .await
            .expect("count reads");
    assert_eq!(rows, 1);
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn concurrent_releases_yield_a_single_winner() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    let at = instance(2);
    assert!(claim(pool, "ev-2r", at, "tick-old").await);
    assert!(
        clear_event_voice_lease(pool, "ev-2r", at, "tick-old")
            .await
            .expect("clear runs"),
        "the row starts pre-cleared with a NULL lease"
    );
    // Two ticks race the re-lease UPDATE on the pre-cleared row: the CASE
    // reads the latest committed lease under the row lock, so exactly one
    // takes the NULL lease (the fresh-insert race above never exercises
    // this conflict path).
    let (won_a, won_b) = tokio::join!(
        claim_event_voice_channel(pool, "ev-2r", at, at, None, "tick-a"),
        claim_event_voice_channel(pool, "ev-2r", at, at, None, "tick-b"),
    );
    let (won_a, won_b) = (won_a.expect("claim runs"), won_b.expect("claim runs"));
    assert!(won_a ^ won_b, "exactly one concurrent re-lease wins");
    let winner = if won_a { "tick-a" } else { "tick-b" };
    assert_eq!(
        lease_of(pool, "ev-2r").await.as_deref(),
        Some(winner),
        "the winner holds the re-leased row"
    );
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn claims_are_isolated_per_instance() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    let at = instance(2);
    let at_b = instance(3);
    assert!(claim(pool, "ev-3", at, "tick-a").await);
    assert!(claim(pool, "ev-3", at_b, "tick-b").await);
    assert!(!claim(pool, "ev-3", at, "tick-c").await);
    let rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM event_voice_channels WHERE event_id = 'ev-3'")
            .fetch_one(pool)
            .await
            .expect("count reads");
    assert_eq!(rows, 2);
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn claim_accepts_a_nullable_end() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    let at = instance(2);
    assert!(
        claim_event_voice_channel(pool, "ev-4", at, at, None, "tick-a")
            .await
            .expect("claim runs")
    );
    let ends: Option<OffsetDateTime> =
        sqlx::query_scalar("SELECT ends_at FROM event_voice_channels WHERE event_id = 'ev-4'")
            .fetch_one(pool)
            .await
            .expect("ends reads");
    assert_eq!(ends, None);
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn stale_select_uses_lease_age_not_claim_age() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    let at = instance(2);
    assert!(claim(pool, "ev-stale", at, "tick-old").await);
    backdate_updated(pool, "ev-stale", 6).await;
    assert!(claim(pool, "ev-fresh", at, "tick-new").await);
    assert!(claim(pool, "ev-bound", at, "tick-bound").await);
    assert!(bind_event_voice_channel(pool, "ev-bound", at, "7701")
        .await
        .expect("bind runs"));
    // Bound but untouched for 6 minutes: excluded by the bound rule alone.
    backdate_updated(pool, "ev-bound", 6).await;
    // A row claimed long ago but re-leased just now is not stale: backdate
    // the claim instant only and leave updated_at fresh.
    assert!(claim(pool, "ev-released", at, "tick-re").await);
    sqlx::query(
        "UPDATE event_voice_channels SET created_at = NOW() - make_interval(mins => 30) \
         WHERE event_id = 'ev-released'",
    )
    .execute(pool)
    .await
    .expect("claim backdate runs");

    let stale = stale_event_voice_claims(pool, 300)
        .await
        .expect("stale select runs");
    assert_eq!(stale.len(), 1, "only the untouched old row is stale");
    assert_eq!(stale[0].0, "ev-stale");
    assert_eq!(stale[0].2.as_deref(), Some("tick-old"));
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn bind_wins_once_and_never_overwrites() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    let at = instance(2);
    assert!(claim(pool, "ev-bind", at, "tick-a").await);
    assert!(bind_event_voice_channel(pool, "ev-bind", at, "7702")
        .await
        .expect("bind runs"));
    assert!(
        !bind_event_voice_channel(pool, "ev-bind", at, "7703")
            .await
            .expect("rebind runs"),
        "a racing bind must not overwrite the live binding"
    );
    let channel: Option<String> = sqlx::query_scalar(
        "SELECT voice_channel_id FROM event_voice_channels WHERE event_id = 'ev-bind'",
    )
    .fetch_one(pool)
    .await
    .expect("channel reads");
    assert_eq!(channel.as_deref(), Some("7702"));
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn lease_clear_is_fenced_to_the_expected_lease() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    let at = instance(2);
    assert!(claim(pool, "ev-clear", at, "tick-a").await);
    assert!(
        !clear_event_voice_lease(pool, "ev-clear", at, "tick-stale-probe")
            .await
            .expect("clear runs"),
        "a stale probe must not wipe a lease it never saw"
    );
    assert_eq!(lease_of(pool, "ev-clear").await.as_deref(), Some("tick-a"));
    assert!(clear_event_voice_lease(pool, "ev-clear", at, "tick-a")
        .await
        .expect("clear runs"));
    assert_eq!(lease_of(pool, "ev-clear").await, None);
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn bound_select_skips_orphaned_rows() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    let at = instance(2);
    assert!(claim(pool, "ev-live", at, "tick-a").await);
    assert!(bind_event_voice_channel(pool, "ev-live", at, "7711")
        .await
        .expect("bind runs"));
    assert!(claim(pool, "ev-gone", at, "tick-b").await);
    assert!(bind_event_voice_channel(pool, "ev-gone", at, "7712")
        .await
        .expect("bind runs"));
    mark_event_voice_orphaned(pool, "ev-gone", at)
        .await
        .expect("orphan runs");
    let bound = bound_event_voice_channels(pool)
        .await
        .expect("bound select runs");
    assert_eq!(bound.len(), 1);
    assert_eq!(bound[0].0, "ev-live");
    assert_eq!(bound[0].2, "7711");
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn orphan_mark_clears_the_lease_and_keeps_the_channel_id() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    let at = instance(2);
    assert!(claim(pool, "ev-orphan", at, "tick-a").await);
    assert!(bind_event_voice_channel(pool, "ev-orphan", at, "7713")
        .await
        .expect("bind runs"));
    mark_event_voice_orphaned(pool, "ev-orphan", at)
        .await
        .expect("orphan runs");
    let row = sqlx::query(
        "SELECT status, owner_tick_lease, voice_channel_id FROM event_voice_channels \
         WHERE event_id = 'ev-orphan'",
    )
    .fetch_one(pool)
    .await
    .expect("row reads");
    let status: String = row.try_get("status").expect("status decodes");
    let lease: Option<String> = row.try_get("owner_tick_lease").expect("lease decodes");
    let channel: Option<String> = row.try_get("voice_channel_id").expect("channel decodes");
    assert_eq!(status, "orphaned");
    assert_eq!(lease, None);
    assert_eq!(channel.as_deref(), Some("7713"));
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn revive_restores_creating_with_a_cleared_lease() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    let at = instance(2);
    assert!(claim(pool, "ev-revive", at, "tick-a").await);
    assert!(bind_event_voice_channel(pool, "ev-revive", at, "7714")
        .await
        .expect("bind runs"));
    mark_event_voice_orphaned(pool, "ev-revive", at)
        .await
        .expect("orphan runs");
    revive_event_voice_orphan(pool, "ev-revive", at)
        .await
        .expect("revive runs");
    let row = sqlx::query(
        "SELECT status, owner_tick_lease FROM event_voice_channels WHERE event_id = 'ev-revive'",
    )
    .fetch_one(pool)
    .await
    .expect("row reads");
    let status: String = row.try_get("status").expect("status decodes");
    let lease: Option<String> = row.try_get("owner_tick_lease").expect("lease decodes");
    assert_eq!(status, "creating");
    assert_eq!(lease, None);
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn join_writes_one_open_span_and_ignores_unmapped_channels() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    let at = instance(2);
    assert!(claim(pool, "ev-span", at, "tick-a").await);
    assert!(bind_event_voice_channel(pool, "ev-span", at, "7721")
        .await
        .expect("bind runs"));
    record_event_voice_join(pool, "9999", "3101")
        .await
        .expect("unmapped join runs");
    assert_eq!(span_counts(pool, "3101").await, (0, 0));
    record_event_voice_join(pool, "7721", "3101")
        .await
        .expect("join runs");
    assert_eq!(span_counts(pool, "3101").await, (1, 1));
    record_event_voice_join(pool, "7721", "3101")
        .await
        .expect("duplicate join runs");
    assert_eq!(
        span_counts(pool, "3101").await,
        (1, 1),
        "an already-open span is kept, never duplicated"
    );
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn leave_stamps_and_rejoin_opens_a_second_span() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    let at = instance(2);
    assert!(claim(pool, "ev-rejoin", at, "tick-a").await);
    assert!(bind_event_voice_channel(pool, "ev-rejoin", at, "7722")
        .await
        .expect("bind runs"));
    record_event_voice_join(pool, "7722", "3102")
        .await
        .expect("join runs");
    record_event_voice_leave(pool, "7722", "3102")
        .await
        .expect("leave runs");
    assert_eq!(span_counts(pool, "3102").await, (1, 0));
    record_event_voice_join(pool, "7722", "3102")
        .await
        .expect("rejoin runs");
    assert_eq!(span_counts(pool, "3102").await, (2, 1));
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn leave_is_scoped_to_the_channel_event() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    let at = instance(2);
    for (event, channel) in [("ev-left-a", "7731"), ("ev-left-b", "7732")] {
        assert!(claim(pool, event, at, "tick-a").await);
        assert!(bind_event_voice_channel(pool, event, at, channel)
            .await
            .expect("bind runs"));
        record_event_voice_join(pool, channel, "3103")
            .await
            .expect("join runs");
    }
    // Same member in two events (a move): leaving B closes only B's span.
    record_event_voice_leave(pool, "7732", "3103")
        .await
        .expect("leave runs");
    let open_a: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM event_voice_presence \
         WHERE event_id = 'ev-left-a' AND user_id = '3103' AND left_at IS NULL",
    )
    .fetch_one(pool)
    .await
    .expect("open count reads");
    assert_eq!(open_a, 1);
    assert_eq!(span_counts(pool, "3103").await, (2, 1));
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn joins_carry_their_own_instance() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    let at = instance(2);
    let at_b = instance(3);
    assert!(claim(pool, "ev-series", at, "tick-a").await);
    assert!(bind_event_voice_channel(pool, "ev-series", at, "7741")
        .await
        .expect("bind runs"));
    assert!(claim(pool, "ev-series", at_b, "tick-b").await);
    assert!(bind_event_voice_channel(pool, "ev-series", at_b, "7742")
        .await
        .expect("bind runs"));
    record_event_voice_join(pool, "7741", "3104")
        .await
        .expect("join runs");
    record_event_voice_join(pool, "7742", "3104")
        .await
        .expect("join runs");
    let instances: Vec<OffsetDateTime> = sqlx::query_scalar(
        "SELECT DISTINCT instance FROM event_voice_presence \
         WHERE event_id = 'ev-series' AND user_id = '3104' ORDER BY 1",
    )
    .fetch_all(pool)
    .await
    .expect("instances read");
    assert_eq!(instances.len(), 2);
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn flush_repairs_missing_spans_and_never_closes() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    let at = instance(2);
    assert!(claim(pool, "ev-flush", at, "tick-a").await);
    assert!(bind_event_voice_channel(pool, "ev-flush", at, "7751")
        .await
        .expect("bind runs"));
    // One occupant already has an open span; one closed span predates the
    // flush; one occupant has no span at all (the lost join row).
    record_event_voice_join(pool, "7751", "3105")
        .await
        .expect("join runs");
    sqlx::query(
        "INSERT INTO event_voice_presence (event_id, instance, user_id, joined_at, left_at) \
         VALUES ('ev-flush', $1, '3106', NOW() - make_interval(mins => 30), \
         NOW() - make_interval(mins => 20))",
    )
    .bind(at)
    .execute(pool)
    .await
    .expect("closed span seeds");
    let inserted = flush_event_voice_presence(
        pool,
        "ev-flush",
        at,
        &["3105".to_owned(), "3106".to_owned(), "3107".to_owned()],
    )
    .await
    .expect("flush runs");
    // 3106's closed span is history (the member left): the flush re-opens it
    // because the snapshot still lists them, and never touches left_at rows.
    assert_eq!(inserted, 2);
    assert_eq!(span_counts(pool, "3105").await, (1, 1));
    assert_eq!(span_counts(pool, "3106").await, (2, 1));
    assert_eq!(span_counts(pool, "3107").await, (1, 1));
    let closed_kept: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM event_voice_presence \
         WHERE event_id = 'ev-flush' AND user_id = '3106' AND left_at IS NOT NULL",
    )
    .fetch_one(pool)
    .await
    .expect("closed count reads");
    assert_eq!(closed_kept, 1, "the flush never rewrites a closed span");
    let empty = flush_event_voice_presence(pool, "ev-flush", at, &[])
        .await
        .expect("empty flush runs");
    assert_eq!(empty, 0);
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn reclaim_with_shifted_schedule_updates_the_row_but_keeps_the_lease() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    // Whole seconds: timestamptz round-trips at microsecond precision, so a
    // nanosecond-bearing instant would never compare equal after the read.
    let at = instance(2).replace_nanosecond(0).expect("truncate nanos");
    let moved = at + Duration::minutes(30);
    let ends = at + Duration::hours(2);
    let ends_moved = moved + Duration::hours(2);
    assert!(
        claim_event_voice_channel(pool, "ev-shift", at, at, Some(ends), "tick-a")
            .await
            .expect("claim runs")
    );
    // Freeze the lease age before the losing re-claim: a losing claim must
    // never move `updated_at`, or dead leases stay fresh forever and the
    // stale pass never clears them.
    backdate_updated(pool, "ev-shift", 6).await;
    let age_before: OffsetDateTime = sqlx::query_scalar(
        "SELECT updated_at FROM event_voice_channels WHERE event_id = 'ev-shift'",
    )
    .fetch_one(pool)
    .await
    .expect("lease age reads");
    // The Discord event is rescheduled under the same instance key: the
    // re-claim loses (tick-a still holds the lease) but the shifted schedule
    // lands on the row while lease, status and binding stand.
    assert!(
        !claim_event_voice_channel(pool, "ev-shift", at, moved, Some(ends_moved), "tick-b")
            .await
            .expect("re-claim runs"),
        "a re-claim against a held lease still loses"
    );
    let row: (
        OffsetDateTime,
        Option<OffsetDateTime>,
        Option<String>,
        String,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT starts_at, ends_at, owner_tick_lease, status, voice_channel_id \
             FROM event_voice_channels WHERE event_id = 'ev-shift'",
    )
    .fetch_one(pool)
    .await
    .expect("row reads");
    assert_eq!(row.0, moved, "the shifted starts_at updates the row");
    assert_eq!(
        row.1,
        Some(ends_moved),
        "the shifted ends_at updates the row"
    );
    assert_eq!(row.2.as_deref(), Some("tick-a"), "the lease is untouched");
    assert_eq!(row.3, "creating", "the status is untouched");
    assert_eq!(row.4, None, "the binding is untouched");
    let age_after: OffsetDateTime = sqlx::query_scalar(
        "SELECT updated_at FROM event_voice_channels WHERE event_id = 'ev-shift'",
    )
    .fetch_one(pool)
    .await
    .expect("lease age reads");
    assert_eq!(
        age_before, age_after,
        "a losing re-claim never moves lease age"
    );
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn guild_bindings_list_only_live_bound_channels() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    let at = instance(2);
    for event in ["ev-gb-live", "ev-gb-gone", "ev-gb-open", "ev-gb-foreign"] {
        assert!(claim(pool, event, at, "tick-a").await);
    }
    assert!(bind_event_voice_channel(pool, "ev-gb-live", at, "7761")
        .await
        .expect("bind runs"));
    assert!(bind_event_voice_channel(pool, "ev-gb-gone", at, "7762")
        .await
        .expect("bind runs"));
    mark_event_voice_orphaned(pool, "ev-gb-gone", at)
        .await
        .expect("orphan runs");
    assert!(bind_event_voice_channel(pool, "ev-gb-foreign", at, "7763")
        .await
        .expect("bind runs"));
    for (guild, event) in [
        ("100", "ev-gb-live"),
        ("100", "ev-gb-gone"),
        ("100", "ev-gb-open"),
        ("200", "ev-gb-foreign"),
    ] {
        sqlx::query(
            "INSERT INTO scheduled_events (guild_id, event_id, name, starts_at, updated_at) \
             VALUES ($1, $2, 'show', '2026-11-01T20:00:00.000Z', '2026-10-22T00:00:00.000Z')",
        )
        .bind(guild)
        .bind(event)
        .execute(pool)
        .await
        .expect("mirror seeds");
    }
    let bindings = bound_event_voice_channels_for_guild(pool, "100")
        .await
        .expect("guild bindings read");
    assert_eq!(bindings.len(), 1, "only the live bound row of guild 100");
    assert_eq!(bindings[0].0, "ev-gb-live");
    assert_eq!(bindings[0].2, "7761");
    let foreign = bound_event_voice_channels_for_guild(pool, "200")
        .await
        .expect("foreign guild bindings read");
    assert_eq!(foreign.len(), 1);
    assert_eq!(foreign[0].0, "ev-gb-foreign");
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn close_absent_stamps_only_the_missing_members() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    let at = instance(2);
    assert!(claim(pool, "ev-absent", at, "tick-a").await);
    assert!(bind_event_voice_channel(pool, "ev-absent", at, "7771")
        .await
        .expect("bind runs"));
    for user in ["3111", "3112", "3113"] {
        record_event_voice_join(pool, "7771", user)
            .await
            .expect("join runs");
    }
    // One member left earlier through the packet path: their closed span is
    // history the repair must not touch.
    record_event_voice_leave(pool, "7771", "3113")
        .await
        .expect("leave runs");
    let closed = close_event_voice_spans_absent(pool, "ev-absent", at, &["3111".to_owned()])
        .await
        .expect("close-absent runs");
    assert_eq!(closed, 1, "only the absent member with an open span closes");
    assert_eq!(span_counts(pool, "3111").await, (1, 1));
    assert_eq!(span_counts(pool, "3112").await, (1, 0));
    assert_eq!(span_counts(pool, "3113").await, (1, 0));
    // Nobody present closes every remaining open span of the instance.
    let closed_all = close_event_voice_spans_absent(pool, "ev-absent", at, &[])
        .await
        .expect("empty close-absent runs");
    assert_eq!(closed_all, 1);
    assert_eq!(span_counts(pool, "3111").await, (1, 0));
    db.close().await.expect("fixture teardown");
}
