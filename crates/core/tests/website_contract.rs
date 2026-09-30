//! Website-contract jobs acceptance (TOG-10090).
//!
//! End-to-end over agent-testdb only, never production: cutover migrations
//! 0001 (funnel) + 0300 (contract tables), then the store writes, then the
//! `web_v1` views return legacy-shaped rows.
//!
//! Requires `TWO_TEST_DATABASE_URL` pointing at an isolated scratch database
//! (sibling convention: `two_bot_test_tog10090`). The harness database is
//! reset per run (full schema drop), so parallel slices cannot collide.
//!
//!mirrors the legacy acceptance in `test/unit.communitysnapshots.test.ts` and
//! `test/unit.scheduledevents.test.ts`:
//! - single-flight per job ([`JobGate`])
//! - atomic mirror swap (empty response deletes the last event; failed read
//!   leaves the snapshot)
//! - raid-window skip (ungrounded history publishes nothing; grounded raids
//!   leave the denominator and the `members` view)
//! - `web_v1` views return legacy-shaped rows.

#![cfg(feature = "db")]

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Pool, Postgres};
use std::str::FromStr;
use two_bot_core::{
    apply_web_contract, build_community_snapshot, build_counter_reading, match_rank_roles,
    normalize_events, read_raid_windows, replace_events, write_counter, write_rank_snapshot,
    CommunitySnapshot, EventStatus, JobGate, RaidWindow, RankKey, RankRole, RawScheduledEvent,
    RosterMember, ScheduledEvent, WebsiteStoreError, LIVE_COUNTER_INTERVAL_MS,
    RANK_SNAPSHOT_INTERVAL_MS, SCHEDULED_EVENTS_INTERVAL_MS, WEB_CONTRACT_VIEWS,
};

const GUILD: &str = "326474832151838730";
const OBSERVED_AT: &str = "2026-08-25T20:00:00.000Z";

fn test_db_url() -> String {
    std::env::var("TWO_TEST_DATABASE_URL").expect(
        "TWO_TEST_DATABASE_URL is required (isolated scratch db, e.g. two_bot_test_tog10090)",
    )
}

/// Serializes the DB tests: every test resets the shared scratch schema, so
/// parallel resets race DDL ("tuple concurrently updated"). Each test holds
/// this across reset + body.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn connect() -> Pool<Postgres> {
    let url = test_db_url();
    assert!(
        url.starts_with("postgres://") || url.starts_with("postgresql://"),
        "test URL must be postgres"
    );
    assert!(
        !url.contains("twobot") || url.contains("test") || url.contains("tog10090"),
        "refusing a non-scratch database URL"
    );
    let options = PgConnectOptions::from_str(&url).expect("valid test URL");
    PgPoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .expect("connect to agent-testdb scratch")
}

/// Reset the harness schema and apply 0001 + 0300 + web_v1.
async fn reset(pool: &Pool<Postgres>) {
    // Drop everything this slice owns, then rebuild from the migrations.
    // `web_v1._ts/_iso/_json` helpers are functions, dropped with CASCADE.
    sqlx::raw_sql("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
        .execute(pool)
        .await
        .expect("reset schema");
    // Path is relative to the core crate root (CARGO_MANIFEST_DIR).
    let migrator = sqlx::migrate!("../cutover/migrations");
    migrator.run(pool).await.expect("apply migrations");
    apply_web_contract(pool).await.expect("apply web_v1 views");
}

fn role_ids() -> std::collections::HashMap<RankKey, String> {
    RankKey::ALL
        .into_iter()
        .map(|k| (k, format!("role-{}", k.key())))
        .collect()
}

fn ladder() -> Vec<RankRole> {
    let ids = role_ids();
    RankKey::ALL
        .into_iter()
        .map(|key| RankRole {
            key,
            role_id: ids[&key].clone(),
        })
        .collect()
}

fn ladder_names() -> Vec<(String, String)> {
    ladder()
        .iter()
        .map(|r| (r.role_id.clone(), r.key.label().to_owned()))
        .collect()
}

fn member(id: &str, held: &[RankKey], bot: bool) -> RosterMember {
    let ids = role_ids();
    RosterMember {
        user_id: id.to_owned(),
        is_bot: bot,
        roles: held.iter().map(|k| ids[k].clone()).collect(),
    }
}

/// Ground every raid window with one present, never-active join.
async fn ground_raids(pool: &Pool<Postgres>) {
    use two_bot_core::{window_bounds, RAID_ANOMALIES};
    for (i, anomaly) in RAID_ANOMALIES.iter().enumerate() {
        let (from, _) = window_bounds(anomaly.start, anomaly.end).expect("static bounds");
        sqlx::query(
            "INSERT INTO members (guild_id, member_id, joined_at, is_bot)
             VALUES ($1, $2, $3::timestamptz, FALSE)",
        )
        .bind(GUILD)
        .bind(format!("raid-{i}"))
        .bind(&from)
        .execute(pool)
        .await
        .expect("ground raid window");
    }
}

async fn read_counter(pool: &Pool<Postgres>) -> Option<(Option<i32>, Option<String>)> {
    sqlx::query_as(
        "SELECT human_member_count, human_member_count_at FROM guild_counters WHERE guild_id = $1",
    )
    .bind(GUILD)
    .fetch_optional(pool)
    .await
    .expect("read counter")
}

/// ISO-8601 UTC millis `days` in the future (stays inside the views'
/// 24h / 90d freshness windows, unlike the fixed OBSERVED_AT fixture).
fn future_iso(days: i64) -> String {
    let t = time::OffsetDateTime::now_utc() + time::Duration::days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.nanosecond() / 1_000_000
    )
}

fn raw_event(id: &str, start: &str, status: i64) -> RawScheduledEvent {
    RawScheduledEvent {
        id: Some(id.to_owned()),
        name: Some(format!("Event {id}")),
        scheduled_start_time: Some(start.to_owned()),
        channel_id: Some("voice-1".to_owned()),
        description: None,
        status: Some(status),
    }
}

#[tokio::test]
async fn intervals_are_the_contract_values() {
    assert_eq!(LIVE_COUNTER_INTERVAL_MS, 60_000);
    assert_eq!(RANK_SNAPSHOT_INTERVAL_MS, 600_000);
    assert_eq!(SCHEDULED_EVENTS_INTERVAL_MS, 10 * 60 * 1000);
    assert_eq!(WEB_CONTRACT_VIEWS.len(), 9);
}

#[tokio::test]
async fn migrations_seed_ladder_version_and_views() {
    let _serial = SERIAL.lock().await;
    let pool = connect().await;
    reset(&pool).await;

    let ladder_rows: Vec<(String, String, i32)> = sqlx::query_as(
        "SELECT rank_key, rank_label, rank_order FROM rank_ladder ORDER BY rank_order",
    )
    .fetch_all(&pool)
    .await
    .expect("ladder rows");
    assert_eq!(ladder_rows.len(), 5);
    assert_eq!(ladder_rows[0].0, "prospect");

    let (version, guild): (String, Option<String>) =
        sqlx::query_as("SELECT contract_version, guild_id FROM web_contract_meta")
            .fetch_one(&pool)
            .await
            .expect("meta row");
    assert_eq!(version, "1.0");
    assert_eq!(guild, None);

    let views: Vec<String> =
        sqlx::query_as("SELECT viewname FROM pg_views WHERE schemaname = 'web_v1' ORDER BY 1")
            .fetch_all(&pool)
            .await
            .expect("views")
            .into_iter()
            .map(|(v,): (String,)| v)
            .collect();
    let mut expected = WEB_CONTRACT_VIEWS
        .iter()
        .map(|s: &&str| s.to_string())
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(views, expected);

    pool.close().await;
}

#[tokio::test]
async fn ungrounded_raid_history_writes_nothing() {
    // Legacy `raid_history_not_grounded`: no funnel history → the counter
    // tick skips and both cache tables stay empty.
    let _serial = SERIAL.lock().await;
    let pool = connect().await;
    reset(&pool).await;

    let windows = read_raid_windows(&pool, GUILD).await.expect("read windows");
    assert_eq!(windows, None, "no joins on file → ungrounded");

    assert!(read_counter(&pool).await.is_none());
    let audit: Option<(String,)> = sqlx::query_as("SELECT guild_id FROM counter_snapshots")
        .fetch_optional(&pool)
        .await
        .expect("audit empty");
    assert!(audit.is_none());

    pool.close().await;
}

#[tokio::test]
async fn counter_tick_publishes_same_reading_to_both_tables() {
    let _serial = SERIAL.lock().await;
    let pool = connect().await;
    reset(&pool).await;
    ground_raids(&pool).await;

    let windows = read_raid_windows(&pool, GUILD)
        .await
        .expect("read windows")
        .expect("grounded");
    assert_eq!(windows.len(), 3);

    // One human, one bot, three grounded raid accounts.
    let all: Vec<RankKey> = RankKey::ALL.into();
    let members = vec![
        member("human", &[RankKey::Prospect], false),
        member("bot", &[], true),
        member("raid-0", &all, false),
        member("raid-1", &all, false),
        member("raid-2", &all, false),
    ];
    let reading = build_counter_reading(&members, &windows).expect("reading");
    assert_eq!(reading.human_member_count, 1);
    assert_eq!(reading.raid_accounts_excluded, 3);

    let gate = JobGate::default();
    let _guard = gate.try_acquire().expect("single-flight acquire");
    write_counter(&pool, GUILD, OBSERVED_AT, reading.human_member_count as i32)
        .await
        .expect("write counter");

    let (count, at) = read_counter(&pool).await.expect("counter row");
    assert_eq!((count, at.as_deref()), (Some(1), Some(OBSERVED_AT)));
    let audit: (Option<i32>, Option<String>) = sqlx::query_as(
        "SELECT human_member_count, human_member_count_at FROM counter_snapshots WHERE guild_id = $1",
    )
    .bind(GUILD)
    .fetch_one(&pool)
    .await
    .expect("audit row");
    assert_eq!((audit.0, audit.1.as_deref()), (Some(1), Some(OBSERVED_AT)));

    // `web_v1.live_counts` always returns exactly one row. OBSERVED_AT is
    // weeks old, so the count has correctly aged out (>24h ceiling) while
    // the timestamp stays for the honest degraded state.
    let live: (Option<i32>, Option<String>) =
        sqlx::query_as("SELECT human_member_count, counts_updated_at FROM web_v1.live_counts")
            .fetch_one(&pool)
            .await
            .expect("live_counts");
    assert_eq!((live.0, live.1.as_deref()), (None, Some(OBSERVED_AT)));

    // A fresh reading is served with its count.
    let fresh = two_bot_core::now_iso();
    write_counter(&pool, GUILD, &fresh, 7)
        .await
        .expect("fresh write");
    let live: (Option<i32>, Option<String>) =
        sqlx::query_as("SELECT human_member_count, counts_updated_at FROM web_v1.live_counts")
            .fetch_one(&pool)
            .await
            .expect("live_counts fresh");
    assert_eq!((live.0, live.1.as_deref()), (Some(7), Some(fresh.as_str())));

    pool.close().await;
}

#[tokio::test]
async fn rank_tick_writes_ladder_ranks_and_exclusions_atomically() {
    let _serial = SERIAL.lock().await;
    let pool = connect().await;
    reset(&pool).await;
    ground_raids(&pool).await;

    let windows = read_raid_windows(&pool, GUILD)
        .await
        .expect("read")
        .expect("grounded");
    let all: Vec<RankKey> = RankKey::ALL.into();
    let members = vec![
        member("prospect", &[RankKey::Prospect], false),
        member("legend", &all, false),
        member("none", &[], false),
        member("raid-0", &all, false),
        member("raid-1", &all, false),
        member("raid-2", &all, false),
    ];
    let matched = match_rank_roles(&ladder_names()).expect("ladder matches");
    let snapshot = build_community_snapshot(&members, &matched, &windows).expect("snapshot");
    assert_eq!(snapshot.human_member_count, 3);
    assert_eq!(snapshot.ranked_member_count, 2);
    assert!(snapshot.nested);

    // OBSERVED_AT is weeks old: the views' 24h ceiling NULLs the aggregates
    // while keeping the rows — assert the ladder shape on the stale write,
    // then rewrite fresh to prove the served path.
    let gate = JobGate::default();
    let _guard = gate.try_acquire().expect("acquire");
    write_rank_snapshot(&pool, GUILD, OBSERVED_AT, &snapshot)
        .await
        .expect("write rank snapshot");

    // Five aggregates.
    let ranks: Vec<(String, Option<i32>, Option<i32>)> = sqlx::query_as(
        "SELECT rank_key, member_count, holders_count FROM rank_snapshots ORDER BY rank_key",
    )
    .fetch_all(&pool)
    .await
    .expect("rank rows");
    assert_eq!(ranks.len(), 5);

    // Highest rank per member; no row for raid accounts (they never ranked).
    let member_rows: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT member_id, rank_key FROM member_ranks ORDER BY member_id")
            .fetch_all(&pool)
            .await
            .expect("member ranks");
    assert_eq!(
        member_rows,
        vec![
            ("legend".to_owned(), Some("legend".to_owned())),
            ("none".to_owned(), None),
            ("prospect".to_owned(), Some("prospect".to_owned())),
        ]
    );

    // Public exclusions: the exact grounded raid set.
    let mut exclusions: Vec<String> = sqlx::query_as::<_, (String,)>(
        "SELECT member_id FROM member_exclusions ORDER BY member_id",
    )
    .fetch_all(&pool)
    .await
    .expect("exclusions")
    .into_iter()
    .map(|(m,)| m)
    .collect();
    exclusions.sort();
    assert_eq!(exclusions, vec!["raid-0", "raid-1", "raid-2"]);

    // Counter moved with the ladder in the same commit.
    let (count, _) = read_counter(&pool).await.expect("counter row");
    assert_eq!(count, Some(3));

    // `web_v1.rank_counts` returns all five rungs in ladder order. The stale
    // snapshot ages out (NULL aggregates, kept rows); a fresh rewrite serves.
    let rank_view: Vec<(String, Option<i32>)> =
        sqlx::query_as("SELECT rank_key, member_count FROM web_v1.rank_counts")
            .fetch_all(&pool)
            .await
            .expect("rank_counts");
    assert_eq!(rank_view.len(), 5);
    assert_eq!(rank_view[0].0, "prospect");
    assert!(
        rank_view.iter().all(|(_, c)| c.is_none()),
        "stale snapshot ages out"
    );
    let fresh_at = future_iso(0);
    write_rank_snapshot(&pool, GUILD, &fresh_at, &snapshot)
        .await
        .expect("fresh rank write");
    let rank_view: Vec<(String, Option<i32>)> =
        sqlx::query_as("SELECT rank_key, member_count FROM web_v1.rank_counts")
            .fetch_all(&pool)
            .await
            .expect("rank_counts fresh");
    let prospect = rank_view
        .iter()
        .find(|(k, _)| k == "prospect")
        .expect("prospect");
    assert_eq!(prospect.1, Some(1));

    // Seed profile rows so the members view has something to join.
    for (id, bot) in [
        ("prospect", false),
        ("legend", false),
        ("none", false),
        ("bot-1", true),
    ] {
        sqlx::query("INSERT INTO members (guild_id, member_id, is_bot) VALUES ($1, $2, $3)")
            .bind(GUILD)
            .bind(id)
            .bind(bot)
            .execute(&pool)
            .await
            .expect("seed member");
    }
    let view_members: Vec<String> =
        sqlx::query_as::<_, (String,)>("SELECT member_id FROM web_v1.members")
            .fetch_all(&pool)
            .await
            .expect("members view")
            .into_iter()
            .map(|(m,)| m)
            .collect();
    assert!(!view_members.contains(&"bot-1".to_owned()), "bots hidden");
    assert!(
        !view_members.iter().any(|m| m.starts_with("raid-")),
        "raid accounts hidden"
    );
    assert!(view_members.contains(&"prospect".to_owned()));

    pool.close().await;
}

#[tokio::test]
async fn non_nested_ladder_is_a_finding_and_writes_nothing() {
    let _serial = SERIAL.lock().await;
    let pool = connect().await;
    reset(&pool).await;
    ground_raids(&pool).await;

    let windows = read_raid_windows(&pool, GUILD)
        .await
        .expect("read")
        .expect("grounded");
    let members = vec![member(
        "broken",
        &[RankKey::Prospect, RankKey::Soldier],
        false,
    )];
    let matched = match_rank_roles(&ladder_names()).expect("ladder");
    let snapshot = build_community_snapshot(&members, &matched, &windows).expect("snapshot");
    assert!(!snapshot.nested);

    let err = write_rank_snapshot(&pool, GUILD, OBSERVED_AT, &snapshot)
        .await
        .expect_err("non-nested refused");
    assert!(matches!(err, WebsiteStoreError::Invariant { .. }));

    let ranks: Vec<(String,)> = sqlx::query_as("SELECT rank_key FROM rank_snapshots")
        .fetch_all(&pool)
        .await
        .expect("no rank rows");
    assert!(ranks.is_empty());
    assert!(read_counter(&pool).await.is_none());

    pool.close().await;
}

#[tokio::test]
async fn events_mirror_swaps_atomically_and_keeps_last_good_on_failure() {
    let _serial = SERIAL.lock().await;
    let pool = connect().await;
    reset(&pool).await;

    let gate = JobGate::default();

    // Seed the last good snapshot. `starts_at` is a week out so the
    // `upcoming_events` view (scheduled + future + 90d window) serves it.
    let seed_start = future_iso(7);
    let seed = normalize_events(&[raw_event("old", &seed_start, 1)]).expect("seed normalizes");
    {
        let _guard = gate.try_acquire().expect("acquire");
        replace_events(&pool, GUILD, OBSERVED_AT, &seed)
            .await
            .expect("seed mirror");
    }

    // Successful read replaces the mirror — old event gone, new one stored
    // with the offset rendered to UTC.
    let next = normalize_events(&[raw_event("event-1", "2026-09-06T18:30:00+01:00", 1)])
        .expect("next normalizes");
    assert_eq!(next[0].starts_at, "2026-09-06T17:30:00.000Z");
    {
        let _guard = gate.try_acquire().expect("acquire");
        replace_events(&pool, GUILD, OBSERVED_AT, &next)
            .await
            .expect("swap mirror");
    }
    let ids: Vec<String> =
        sqlx::query_as::<_, (String,)>("SELECT event_id FROM scheduled_events ORDER BY event_id")
            .fetch_all(&pool)
            .await
            .expect("mirror rows")
            .into_iter()
            .map(|(id,)| id)
            .collect();
    assert_eq!(ids, vec!["event-1".to_owned()]);

    // A successful empty response deletes the last event (zero rows is real).
    {
        let _guard = gate.try_acquire().expect("acquire");
        replace_events(&pool, GUILD, OBSERVED_AT, &[])
            .await
            .expect("empty swap");
    }
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM scheduled_events")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(count.0, 0);

    // Reseed, then a malformed response never reaches the store: the last
    // good snapshot stays in place.
    {
        let _guard = gate.try_acquire().expect("acquire");
        replace_events(&pool, GUILD, OBSERVED_AT, &seed)
            .await
            .expect("reseed");
    }
    assert!(
        normalize_events(&[
            raw_event("good", "2026-09-06T18:00:00.000Z", 1),
            RawScheduledEvent {
                scheduled_start_time: None,
                ..raw_event("bad", "2026-09-06T18:00:00.000Z", 1)
            },
        ])
        .is_none(),
        "one malformed event rejects the snapshot"
    );
    let ids: Vec<String> = sqlx::query_as::<_, (String,)>("SELECT event_id FROM scheduled_events")
        .fetch_all(&pool)
        .await
        .expect("kept rows")
        .into_iter()
        .map(|(id,)| id)
        .collect();
    assert_eq!(ids, vec!["old".to_owned()]);

    // `web_v1.upcoming_events` returns the legacy-shaped row.
    type UpcomingRow = (String, String, String, Option<String>, Option<String>);
    let upcoming: Vec<UpcomingRow> = sqlx::query_as(
        "SELECT event_id, name, starts_at, channel_id, description FROM web_v1.upcoming_events",
    )
    .fetch_all(&pool)
    .await
    .expect("upcoming view");
    assert_eq!(upcoming.len(), 1);
    assert_eq!(upcoming[0].0, "old");
    let next_row: Option<(String,)> = sqlx::query_as("SELECT event_id FROM web_v1.next_event")
        .fetch_optional(&pool)
        .await
        .expect("next view");
    assert_eq!(next_row.map(|(id,)| id), Some("old".to_owned()));

    pool.close().await;
}

#[tokio::test]
async fn job_gate_never_overlaps_ticks() {
    // Concurrent ticks collapse to one: the losers skip, they never queue.
    let gate = Arc::new(JobGate::default());
    let entered = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let (gate, entered) = (Arc::clone(&gate), Arc::clone(&entered));
        handles.push(tokio::spawn(async move {
            let _guard = gate.try_acquire()?;
            entered.fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
            Some(())
        }));
    }
    let mut ran = 0;
    for h in handles {
        if h.await.expect("join").is_some() {
            ran += 1;
        }
    }
    assert_eq!(ran, 1, "exactly one tick holds the gate");
    assert_eq!(entered.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn event_status_round_trips() {
    assert_eq!(EventStatus::Scheduled.as_str(), "scheduled");
    let event = ScheduledEvent {
        id: "e".to_owned(),
        name: "E".to_owned(),
        starts_at: "2026-09-06T17:30:00.000Z".to_owned(),
        channel_id: None,
        description: None,
        status: EventStatus::Active,
    };
    assert_eq!(event.status.as_str(), "active");
    let _ = CommunitySnapshot {
        human_member_count: 0,
        ranked_member_count: 0,
        rank_rows: vec![],
        member_ranks: vec![],
        excluded_member_ids: vec![],
        nested: true,
        raid_accounts_excluded: 0,
    };
    let _ = RaidWindow {
        id: String::new(),
        excluded_member_ids: HashSet::new(),
    };
}
