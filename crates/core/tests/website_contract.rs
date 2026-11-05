//! Website-contract jobs acceptance (TOG-10090).
//!
//! End-to-end over agent-testdb only, never production: cutover migrations
//! 0001 (funnel) + 0300 (contract tables), then the store writes, then the
//! `web_v1` views return legacy-shaped rows.
//!
//! Requires `TWO_TEST_DATABASE_URL` naming a disposable test bootstrap.
//! Each test gets a unique database with the complete migrations, so parallel
//! tests and invocations cannot collide or reset the bootstrap.
//!
//!mirrors the legacy acceptance in `test/unit.communitysnapshots.test.ts` and
//! `test/unit.scheduledevents.test.ts`:
//! - single-flight per job ([`JobGate`])
//! - atomic mirror swap (empty response deletes the last event; failed read
//!   leaves the snapshot)
//! - raid-window skip (ungrounded history publishes nothing; grounded raids
//!   leave the denominator and the `members` view)
//! - `web_v1` views return legacy-shaped rows.
//! - funnel views count leavers and projection-less joins without placeholders
//! - upcoming/next serve scheduled+active only, as UTC millis instants
//! - contract_meta pins the implemented version (bump on any shape change).

#![cfg(feature = "db")]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use sqlx::{Pool, Postgres};
use two_bot_core::{
    apply_web_contract, build_community_snapshot, build_counter_reading, match_rank_roles,
    normalize_events, read_raid_windows, replace_events, upsert_event, write_counter,
    write_rank_snapshot, CommunitySnapshot, EventStatus, JobGate, RaidWindow, RankKey, RankRole,
    RawScheduledEvent, RosterMember, ScheduledEvent, WebsiteStoreError, LIVE_COUNTER_INTERVAL_MS,
    RANK_SNAPSHOT_INTERVAL_MS, SCHEDULED_EVENTS_INTERVAL_MS, WEB_CONTRACT_VERSION,
    WEB_CONTRACT_VIEWS,
};
use two_bot_testsupport::{guard_database_url, TestDatabase};

const GUILD: &str = "326474832151838730";
const OBSERVED_AT: &str = "2026-08-25T20:00:00.000Z";

async fn connect() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("TWO_TEST_DATABASE_URL is required (disposable test bootstrap)");
    let fixture = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create migrated test database");
    apply_web_contract(fixture.pool())
        .await
        .expect("apply web_v1 views");
    fixture
}

#[test]
fn reset_guard_allows_only_parsed_test_container_and_scratch_database() {
    // The shared guard also refuses PG* overrides; these pure URL cases assume
    // the same clean environment required by the database fixtures.
    for url in [
        "postgres://agent_test:@agent-testdb:5432/two_bot_test_tog10090",
        "postgresql://agent_test:@agent-testdb:5432/two_bot_test_tog10090_guard",
    ] {
        assert!(guard_database_url(url).is_ok());
    }
    // Pure guard tests: none of these targets are ever contacted.
    for url in [
        "postgres://user@production/two_bot",
        "postgres://test_runner@production/real_data",
        "postgres://agent_test:@production/two_bot_test_tog10090",
        "postgres://agent_test:@agent-testdb/real_data",
        "postgres://agent_test:@agent-testdb/test",
        "postgres://agent_test:@agent-testdb/postgres",
        "postgres://agent_test:@agent-testdb/two_bot_test_tog10090_",
        "postgres://agent_test:@agent-testdb/two_bot_test_tog10090-unsafe",
        "postgres://agent_test:@agent-testdb:5433/two_bot_test_tog10090",
        "postgres://agent_test:@agent-testdb.example/two_bot_test_tog10090",
        "postgres://agent_test:@agent-testdb/two_bot_test_tog10090?host=production",
        "postgres://agent_test:@agent-testdb/two_bot_test_tog10090?hostaddr=127.0.0.1",
        "postgres://agent_test:@agent-testdb/two_bot_test_tog10090?options=-csearch_path=test",
        "postgres://agent_test:@production/real_data?application_name=test",
        "postgres://agent_test:@%2Fvar%2Frun%2Fpostgresql/two_bot_test_tog10090",
        "postgres://agent_test:unexpected@agent-testdb/two_bot_test_tog10090",
        "postgresql://agent_test@agent-testdb:5432/two_bot_test_tog10090_guard",
        "postgres://agent_test:@agent-testdb/two_bot_test_tog10090#test",
        "postgres://agent_test:@agent-testdb/",
        "postgres://agent-testdb/two_bot_test_tog10090",
        "not a URL",
    ] {
        assert!(guard_database_url(url).is_err(), "unsafe target accepted");
    }
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

/// ISO-8601 UTC millis `hours` from now (negative = past).
fn offset_iso_hours(hours: i64) -> String {
    let t = time::OffsetDateTime::now_utc() + time::Duration::hours(hours);
    format_iso_utc(t)
}

/// ISO-8601 UTC millis `days` in the future (stays inside the views'
/// 24h / 90d freshness windows, unlike the fixed OBSERVED_AT fixture).
fn future_iso(days: i64) -> String {
    offset_iso_hours(days * 24)
}

fn format_iso_utc(t: time::OffsetDateTime) -> String {
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
    assert_eq!(WEB_CONTRACT_VIEWS.len(), 11);
}

#[tokio::test]
async fn migrations_seed_ladder_version_and_views() {
    let fixture = connect().await;
    let pool = fixture.pool().clone();

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
    assert_eq!(version, "1.1");
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

    fixture.close().await.expect("drop test database");
}

#[tokio::test]
async fn isolated_contracts_keep_their_own_tables_and_leave_public_untouched() {
    let fixture = connect().await;
    let public = fixture.pool().clone();
    sqlx::raw_sql(
        "DROP SCHEMA IF EXISTS tog10090_a_web_v1 CASCADE;
         DROP SCHEMA IF EXISTS tog10090_b_web_v1 CASCADE;
         DROP SCHEMA IF EXISTS tog10090_a CASCADE;
         DROP SCHEMA IF EXISTS tog10090_b CASCADE;
         CREATE SCHEMA tog10090_a;
         CREATE SCHEMA tog10090_b;",
    )
    .execute(&public)
    .await
    .expect("create isolated scratch schemas");
    let fresh = two_bot_core::now_iso();
    write_counter(&public, GUILD, &fresh, 333)
        .await
        .expect("public reading");

    let mut isolated = Vec::new();
    for (schema, count) in [("tog10090_a", 111), ("tog10090_b", 222)] {
        let pool = fixture
            .pool_with_search_path(schema)
            .await
            .expect("isolated pool");
        let migrator = sqlx::migrate!("../cutover/migrations");
        migrator.run(&pool).await.expect("isolated migrations");
        write_counter(&pool, GUILD, &fresh, count)
            .await
            .expect("isolated reading");
        apply_web_contract(&pool).await.expect("isolated contract");
        isolated.push(pool);
    }
    // Reapplying A must not rebind B or public either (idempotence).
    apply_web_contract(&isolated[0]).await.expect("reapply A");
    for (contract, query, expected_count) in [
        (
            "web_v1",
            "SELECT human_member_count FROM web_v1.live_counts",
            333,
        ),
        (
            "tog10090_a_web_v1",
            "SELECT human_member_count FROM tog10090_a_web_v1.live_counts",
            111,
        ),
        (
            "tog10090_b_web_v1",
            "SELECT human_member_count FROM tog10090_b_web_v1.live_counts",
            222,
        ),
    ] {
        let count: Option<i32> = sqlx::query_scalar(query)
            .fetch_one(&public)
            .await
            .expect("isolated live count");
        assert_eq!(count, Some(expected_count), "contract rebound: {contract}");
        let views: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_views WHERE schemaname = $1")
            .bind(contract)
            .fetch_one(&public)
            .await
            .expect("contract view count");
        assert_eq!(views, WEB_CONTRACT_VIEWS.len() as i64);
    }
    for pool in isolated {
        pool.close().await;
    }
    sqlx::raw_sql(
        "DROP SCHEMA tog10090_a_web_v1 CASCADE;
         DROP SCHEMA tog10090_b_web_v1 CASCADE;
         DROP SCHEMA tog10090_a CASCADE;
         DROP SCHEMA tog10090_b CASCADE;",
    )
    .execute(&public)
    .await
    .expect("remove isolated scratch schemas");
    fixture.close().await.expect("drop test database");
}

#[tokio::test]
async fn contract_refuses_a_missing_or_unsafe_current_schema_before_ddl() {
    let fixture = connect().await;
    let public = fixture.pool().clone();
    sqlx::raw_sql(
        "DROP SCHEMA IF EXISTS tog10090_missing CASCADE;
         DROP SCHEMA IF EXISTS \"tog10090-unsafe\" CASCADE;
         CREATE SCHEMA \"tog10090-unsafe\";",
    )
    .execute(&public)
    .await
    .expect("prepare unsafe and absent scratch schemas");
    for schema in ["tog10090_missing", "\"tog10090-unsafe\""] {
        let pool = fixture
            .pool_with_search_path(schema)
            .await
            .expect("scratch pool");
        let error = apply_web_contract(&pool).await.expect_err("refuse schema");
        assert!(matches!(error, sqlx::Error::Configuration(_)));
        pool.close().await;
    }
    let namespaced_contracts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_namespace
         WHERE nspname IN ('tog10090_missing_web_v1', 'tog10090-unsafe_web_v1')",
    )
    .fetch_one(&public)
    .await
    .expect("no unsafe contracts");
    assert_eq!(namespaced_contracts, 0);
    let public_views: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pg_views WHERE schemaname = 'web_v1'")
            .fetch_one(&public)
            .await
            .expect("public preserved");
    assert_eq!(public_views, WEB_CONTRACT_VIEWS.len() as i64);
    sqlx::raw_sql("DROP SCHEMA \"tog10090-unsafe\" CASCADE;")
        .execute(&public)
        .await
        .expect("cleanup unsafe scratch schema");
    fixture.close().await.expect("drop test database");
}

#[tokio::test]
async fn ungrounded_raid_history_writes_nothing() {
    // Legacy `raid_history_not_grounded`: no funnel history → the counter
    // tick skips and both cache tables stay empty.
    let fixture = connect().await;
    let pool = fixture.pool().clone();

    let windows = read_raid_windows(&pool, GUILD).await.expect("read windows");
    assert_eq!(windows, None, "no joins on file → ungrounded");

    assert!(read_counter(&pool).await.is_none());
    let audit: Option<(String,)> = sqlx::query_as("SELECT guild_id FROM counter_snapshots")
        .fetch_optional(&pool)
        .await
        .expect("audit empty");
    assert!(audit.is_none());

    fixture.close().await.expect("drop test database");
}

#[tokio::test]
async fn counter_tick_publishes_same_reading_to_both_tables() {
    let fixture = connect().await;
    let pool = fixture.pool().clone();
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

    fixture.close().await.expect("drop test database");
}

#[tokio::test]
async fn rank_tick_writes_ladder_ranks_and_exclusions_atomically() {
    let fixture = connect().await;
    let pool = fixture.pool().clone();
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

    fixture.close().await.expect("drop test database");
}

#[tokio::test]
async fn rank_ladder_publishes_one_row_per_rung_with_role_ids() {
    // Producer for the rank cards' Discord role ids: one row per ladder rung,
    // NULL role ids until the rank collector matches the rung. rank_counts is
    // untouched by the new view.
    let fixture = connect().await;
    let pool = fixture.pool().clone();

    let ladder: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT rank_key, role_id FROM web_v1.rank_ladder ORDER BY rank_key")
            .fetch_all(&pool)
            .await
            .expect("rank_ladder");
    assert_eq!(ladder.len(), 5);
    assert!(
        ladder.iter().all(|(_, role)| role.is_none()),
        "unmatched rungs publish NULL, never a placeholder"
    );

    sqlx::query("UPDATE rank_ladder SET role_id = 'role-' || rank_key")
        .execute(&pool)
        .await
        .expect("match rungs to roles");
    let ladder: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT rank_key, role_id FROM web_v1.rank_ladder ORDER BY rank_key")
            .fetch_all(&pool)
            .await
            .expect("rank_ladder matched");
    assert_eq!(ladder.len(), 5);
    for (key, role) in &ladder {
        assert_eq!(
            role,
            &Some(format!("role-{key}")),
            "rung carries its role id"
        );
    }
    let mut keys: Vec<&str> = ladder.iter().map(|(key, _)| key.as_str()).collect();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys.len(), 5, "exactly one row per rank_key");

    // rank_counts keeps serving all five rungs in ladder order.
    let counts: Vec<(String,)> = sqlx::query_as("SELECT rank_key FROM web_v1.rank_counts")
        .fetch_all(&pool)
        .await
        .expect("rank_counts");
    assert_eq!(
        counts.into_iter().map(|(key,)| key).collect::<Vec<_>>(),
        ["prospect", "member", "soldier", "veteran", "legend"]
    );

    fixture.close().await.expect("drop test database");
}

#[tokio::test]
async fn non_nested_ladder_is_a_finding_and_writes_nothing() {
    let fixture = connect().await;
    let pool = fixture.pool().clone();
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

    fixture.close().await.expect("drop test database");
}

#[tokio::test]
async fn events_mirror_swaps_atomically_and_keeps_last_good_on_failure() {
    let fixture = connect().await;
    let pool = fixture.pool().clone();

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
    // The stamp moves forward: the swap above tombstoned `old` at OBSERVED_AT,
    // and a tick at or below the tombstone high-water is stale relative to
    // that delete, so it changes nothing.
    const EMPTIED_AT: &str = "2026-08-25T20:00:01.000Z";
    {
        let _guard = gate.try_acquire().expect("acquire");
        replace_events(&pool, GUILD, EMPTIED_AT, &[])
            .await
            .expect("empty swap");
    }
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM scheduled_events")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(count.0, 0);

    // Reseed at a still-newer stamp, then a malformed response never reaches
    // the store: the last good snapshot stays in place.
    const RESEEDED_AT: &str = "2026-08-25T20:00:02.000Z";
    {
        let _guard = gate.try_acquire().expect("acquire");
        replace_events(&pool, GUILD, RESEEDED_AT, &seed)
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
    for malformed_start in ["2026-09-06T18:00:00+0é0", "2026-09-06T18:00:00.123.extraZ"] {
        let response = [
            raw_event("good", "2026-09-06T18:00:00.000Z", 1),
            raw_event("bad", malformed_start, 1),
        ];
        let _guard = gate.try_acquire().expect("acquire");
        if let Some(events) = normalize_events(&response) {
            replace_events(&pool, GUILD, OBSERVED_AT, &events)
                .await
                .expect("swap validated response");
            panic!("malformed timestamp accepted: {malformed_start}");
        }
    }
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

    fixture.close().await.expect("drop test database");
}

/// Presence spans are guild-scoped through the event mirror: when a swap
/// drops an event, its spans must go in the same transaction, or member
/// erasure and the guild purge can no longer select them.
#[tokio::test]
async fn events_mirror_swap_deletes_presence_for_removed_events() {
    let fixture = connect().await;
    let pool = fixture.pool().clone();

    let seed = normalize_events(&[
        raw_event("ev-keep", "2026-09-06T18:00:00.000Z", 1),
        raw_event("ev-drop", "2026-09-06T18:00:00.000Z", 1),
    ])
    .expect("seed normalizes");
    replace_events(&pool, GUILD, OBSERVED_AT, &seed)
        .await
        .expect("seed mirror");
    for event in ["ev-keep", "ev-drop"] {
        sqlx::query(
            "INSERT INTO event_voice_presence (event_id, instance, user_id, joined_at, left_at)
             VALUES ($1, '2026-09-06T18:00:00Z', '3201', NOW(), NULL)",
        )
        .bind(event)
        .execute(&pool)
        .await
        .expect("seed presence spans");
    }

    let next = normalize_events(&[raw_event("ev-keep", "2026-09-06T18:00:00.000Z", 1)])
        .expect("next normalizes");
    replace_events(&pool, GUILD, OBSERVED_AT, &next)
        .await
        .expect("swap mirror");

    let remaining: Vec<(String,)> =
        sqlx::query_as("SELECT event_id FROM event_voice_presence ORDER BY event_id")
            .fetch_all(&pool)
            .await
            .expect("presence rows");
    assert_eq!(
        remaining,
        vec![("ev-keep".to_owned(),)],
        "presence for the dropped event must go with its mirror row"
    );

    fixture.close().await.expect("drop test database");
}

/// Mirror ordering (TOG-20273): poller snapshots and mutation single-row
/// writes share last-observed-wins on `updated_at`. A snapshot taken before a
/// mutation leaves the newer mutation row in place; a stale mutation write
/// cannot overwrite a newer row; a newer snapshot still deletes an event the
/// poller observed as gone. Equal timestamps still apply (idempotent
/// re-application at the same instant).
#[tokio::test]
async fn event_mirror_keeps_newer_rows_across_mutation_and_poller_writes() {
    let fixture = connect().await;
    let pool = fixture.pool().clone();

    const T0: &str = "2026-08-25T19:55:00.000Z";
    const T1: &str = "2026-08-25T20:00:00.000Z";
    const T2: &str = "2026-08-25T20:05:00.000Z";
    const T3: &str = "2026-08-25T20:10:00.000Z";

    fn mirror_event(id: &str, name: &str, status: EventStatus) -> ScheduledEvent {
        ScheduledEvent {
            id: id.to_owned(),
            name: name.to_owned(),
            starts_at: "2026-09-06T17:30:00.000Z".to_owned(),
            channel_id: Some("voice-1".to_owned()),
            description: None,
            status,
        }
    }

    async fn mirror_row(pool: &Pool<Postgres>, event_id: &str) -> Option<(String, String, String)> {
        sqlx::query_as::<_, (String, String, String)>(
            "SELECT name, status, updated_at FROM scheduled_events
              WHERE guild_id = $1 AND event_id = $2",
        )
        .bind(GUILD)
        .bind(event_id)
        .fetch_optional(pool)
        .await
        .expect("read mirror row")
    }

    // Poller snapshot at T1 seeds A and B.
    replace_events(
        &pool,
        GUILD,
        T1,
        &[
            mirror_event("a", "A poller", EventStatus::Scheduled),
            mirror_event("b", "B poller", EventStatus::Scheduled),
        ],
    )
    .await
    .expect("seed snapshot");

    // Newer mutation at T2 refreshes A.
    upsert_event(
        &pool,
        GUILD,
        T2,
        &mirror_event("a", "A mutation", EventStatus::Active),
    )
    .await
    .expect("mutation applies");
    assert_eq!(
        mirror_row(&pool, "a").await,
        Some(("A mutation".to_owned(), "active".to_owned(), T2.to_owned()))
    );

    // Stale mutation at T1 cannot overwrite the newer row; still Ok because
    // the Discord effect already happened — the mirror just keeps newer data.
    upsert_event(
        &pool,
        GUILD,
        T1,
        &mirror_event("a", "A stale", EventStatus::Scheduled),
    )
    .await
    .expect("stale mutation is a no-op");
    assert_eq!(
        mirror_row(&pool, "a").await,
        Some(("A mutation".to_owned(), "active".to_owned(), T2.to_owned())),
        "stale mutation must not overwrite the newer row"
    );

    // Stale snapshot replay at T1 leaves the newer mutation row in place.
    replace_events(
        &pool,
        GUILD,
        T1,
        &[
            mirror_event("a", "A poller", EventStatus::Scheduled),
            mirror_event("b", "B poller", EventStatus::Scheduled),
        ],
    )
    .await
    .expect("stale snapshot");
    assert_eq!(
        mirror_row(&pool, "a").await,
        Some(("A mutation".to_owned(), "active".to_owned(), T2.to_owned())),
        "stale snapshot must not overwrite the newer mutation row"
    );
    assert_eq!(
        mirror_row(&pool, "b").await,
        Some(("B poller".to_owned(), "scheduled".to_owned(), T1.to_owned()))
    );

    // Same-instant re-application still applies (idempotent writers share a
    // millisecond), and also refreshes B to a newer mutation row.
    upsert_event(
        &pool,
        GUILD,
        T2,
        &mirror_event("a", "A mutation again", EventStatus::Active),
    )
    .await
    .expect("same-instant mutation applies");
    upsert_event(
        &pool,
        GUILD,
        T2,
        &mirror_event("b", "B mutation", EventStatus::Completed),
    )
    .await
    .expect("newer mutation applies");
    assert_eq!(
        mirror_row(&pool, "a").await,
        Some((
            "A mutation again".to_owned(),
            "active".to_owned(),
            T2.to_owned()
        ))
    );
    assert_eq!(
        mirror_row(&pool, "b").await,
        Some((
            "B mutation".to_owned(),
            "completed".to_owned(),
            T2.to_owned()
        ))
    );

    // Older snapshot at T0 omitting B: newer rows survive the delete and the
    // upsert alike, so B is neither deleted nor resurrected-clobbered.
    replace_events(
        &pool,
        GUILD,
        T0,
        &[mirror_event("a", "A ancient", EventStatus::Scheduled)],
    )
    .await
    .expect("older snapshot");
    assert_eq!(
        mirror_row(&pool, "a").await,
        Some((
            "A mutation again".to_owned(),
            "active".to_owned(),
            T2.to_owned()
        )),
        "older snapshot must spare the newer row"
    );
    assert_eq!(
        mirror_row(&pool, "b").await,
        Some((
            "B mutation".to_owned(),
            "completed".to_owned(),
            T2.to_owned()
        )),
        "older snapshot must not delete the newer row"
    );

    // Newer snapshot at T3 omitting B: the poller did observe B as gone, so
    // the delete still applies while A refreshes.
    replace_events(
        &pool,
        GUILD,
        T3,
        &[mirror_event("a", "A fresh", EventStatus::Scheduled)],
    )
    .await
    .expect("newer snapshot");
    assert_eq!(
        mirror_row(&pool, "a").await,
        Some(("A fresh".to_owned(), "scheduled".to_owned(), T3.to_owned()))
    );
    assert_eq!(mirror_row(&pool, "b").await, None);

    fixture.close().await.expect("drop test database");
}

/// Call-site stamp order (TOG-20273 finding): the store primitives take
/// explicit stamps, so this test replays the production order with explicit
/// instants — the poller stamps *before* its GET, the mutation executor
/// stamps *after* Discord returns — and asserts the mirror outcome at each
/// step. A late-arriving snapshot of the pre-mutation row must not clobber
/// the mutation's newer row, and a later snapshot that genuinely observed the
/// mutation must still apply.
#[tokio::test]
async fn event_mirror_applies_writes_in_call_site_stamp_order() {
    let fixture = connect().await;
    let pool = fixture.pool().clone();

    // Poller stamps S0 before its GET; the GET is served from the
    // pre-mutation row, so the snapshot carries the old name.
    const S0: &str = "2026-08-25T20:00:00.000Z";
    // Mutation PATCH returns after the poller's fetch; the executor stamps
    // the mirror write at completion, strictly newer than the in-flight fetch.
    const M1: &str = "2026-08-25T20:00:00.400Z";
    // Later poller stamps S2 before a GET that observes the mutated row.
    const S2: &str = "2026-08-25T20:05:00.000Z";

    fn mirror_event(id: &str, name: &str, status: EventStatus) -> ScheduledEvent {
        ScheduledEvent {
            id: id.to_owned(),
            name: name.to_owned(),
            starts_at: "2026-09-06T17:30:00.000Z".to_owned(),
            channel_id: Some("voice-1".to_owned()),
            description: None,
            status,
        }
    }

    async fn mirror_row(pool: &Pool<Postgres>, event_id: &str) -> Option<(String, String, String)> {
        sqlx::query_as::<_, (String, String, String)>(
            "SELECT name, status, updated_at FROM scheduled_events
              WHERE guild_id = $1 AND event_id = $2",
        )
        .bind(GUILD)
        .bind(event_id)
        .fetch_optional(pool)
        .await
        .expect("read mirror row")
    }

    // Snapshot S0 seeds the pre-mutation row.
    replace_events(
        &pool,
        GUILD,
        S0,
        &[mirror_event("a", "A poller", EventStatus::Scheduled)],
    )
    .await
    .expect("seed snapshot");

    // Mutation completes after the fetch began, so its post-return stamp is
    // newer and the write applies despite the older in-flight snapshot.
    upsert_event(
        &pool,
        GUILD,
        M1,
        &mirror_event("a", "A mutation", EventStatus::Active),
    )
    .await
    .expect("mutation applies");
    assert_eq!(
        mirror_row(&pool, "a").await,
        Some(("A mutation".to_owned(), "active".to_owned(), M1.to_owned()))
    );

    // The in-flight snapshot's write arrives late with its pre-fetch stamp:
    // it must neither delete nor overwrite the newer mutation row.
    replace_events(
        &pool,
        GUILD,
        S0,
        &[mirror_event("a", "A poller", EventStatus::Scheduled)],
    )
    .await
    .expect("late snapshot");
    assert_eq!(
        mirror_row(&pool, "a").await,
        Some(("A mutation".to_owned(), "active".to_owned(), M1.to_owned())),
        "late pre-fetch snapshot must not clobber the newer mutation row"
    );

    // A later snapshot that observed the mutation still applies.
    replace_events(
        &pool,
        GUILD,
        S2,
        &[mirror_event("a", "A mutation", EventStatus::Active)],
    )
    .await
    .expect("fresh snapshot");
    assert_eq!(
        mirror_row(&pool, "a").await,
        Some(("A mutation".to_owned(), "active".to_owned(), S2.to_owned()))
    );

    fixture.close().await.expect("drop test database");
}

/// Seed one funnel `events` row (mirrors the cutover `record_event` column
/// list; `occurred_at` binds as text and casts to timestamptz).
async fn seed_funnel_event(
    pool: &Pool<Postgres>,
    event_type: &str,
    member_id: &str,
    occurred_at: &str,
    source: &str,
    key: &str,
) {
    sqlx::query(
        "INSERT INTO events (event_type, member_id, guild_id, occurred_at, source, metadata, idempotency_key)
         VALUES ($1, $2, $3, $4::timestamptz, $5, NULL, $6)",
    )
    .bind(event_type)
    .bind(member_id)
    .bind(GUILD)
    .bind(occurred_at)
    .bind(source)
    .bind(key)
    .execute(pool)
    .await
    .expect("seed funnel event");
}

/// Seed one `members` projection row. `left_at` None = current member.
async fn seed_member(pool: &Pool<Postgres>, member_id: &str, bot: bool, left_at: Option<&str>) {
    sqlx::query(
        "INSERT INTO members (guild_id, member_id, joined_at, left_at, is_bot)
         VALUES ($1, $2, '2026-09-20T12:00:00.000Z'::timestamptz, $3::timestamptz, $4)",
    )
    .bind(GUILD)
    .bind(member_id)
    .bind(left_at)
    .bind(bot)
    .execute(pool)
    .await
    .expect("seed member");
}

#[tokio::test]
async fn funnel_views_count_leavers_without_inventing_placeholders() {
    // B4 website-read contract: the growth dashboard reads aggregates, never
    // member ids. A join counts even when the projector never wrote a member
    // row; a leaver counts as both a join and a leave. The members view shows
    // leavers as non-current but never invents a row for a member it never saw.
    let fixture = connect().await;
    let pool = fixture.pool().clone();

    seed_member(&pool, "m1", false, Some("2026-09-21T18:00:00.000Z")).await;
    seed_member(&pool, "m2", false, Some("2026-09-20T18:00:00.000Z")).await;
    seed_member(&pool, "bot1", true, None).await;
    // m3 and m4 join but never get a projection row: no placeholder allowed.

    let day_a = "2026-09-20";
    let day_b = "2026-09-21";
    seed_funnel_event(
        &pool,
        "member_join",
        "m1",
        "2026-09-20T12:00:00.000Z",
        "abc123",
        "tog11721-k1",
    )
    .await;
    seed_funnel_event(
        &pool,
        "member_join",
        "m2",
        "2026-09-20T12:05:00.000Z",
        "abc123",
        "tog11721-k2",
    )
    .await;
    seed_funnel_event(
        &pool,
        "member_join",
        "m3",
        "2026-09-20T12:10:00.000Z",
        "unknown",
        "tog11721-k3",
    )
    .await;
    seed_funnel_event(
        &pool,
        "member_join",
        "bot1",
        "2026-09-20T12:15:00.000Z",
        "abc123",
        "tog11721-k7",
    )
    .await;
    seed_funnel_event(
        &pool,
        "member_leave",
        "m2",
        "2026-09-20T18:00:00.000Z",
        "abc123",
        "tog11721-k4",
    )
    .await;
    seed_funnel_event(
        &pool,
        "first_message",
        "m1",
        "2026-09-20T13:00:00.000Z",
        "abc123",
        "tog11721-k5",
    )
    .await;
    seed_funnel_event(
        &pool,
        "first_voice_session",
        "m1",
        "2026-09-20T14:00:00.000Z",
        "abc123",
        "tog11721-k6",
    )
    .await;
    seed_funnel_event(
        &pool,
        "member_join",
        "m4",
        "2026-09-21T12:00:00.000Z",
        "abc123",
        "tog11721-k8",
    )
    .await;
    seed_funnel_event(
        &pool,
        "member_leave",
        "m1",
        "2026-09-21T18:00:00.000Z",
        "abc123",
        "tog11721-k9",
    )
    .await;

    type DailyRow = (String, i64, i64, i64, i64, i64);
    let daily: Vec<DailyRow> = sqlx::query_as(
        "SELECT day, joins, leaves, first_messages, first_voice_sessions, net_change
         FROM web_v1.funnel_daily ORDER BY day",
    )
    .fetch_all(&pool)
    .await
    .expect("funnel_daily");
    assert_eq!(
        daily,
        vec![
            (day_a.to_owned(), 3, 1, 1, 1, 2),
            (day_b.to_owned(), 1, 1, 0, 0, 0),
        ]
    );

    // 'unknown' renders as itself, never folded into a real invite code; the
    // bot join is excluded from every source bucket.
    type SourceRow = (String, String, i64);
    let by_source: Vec<SourceRow> = sqlx::query_as(
        "SELECT day, source, joins FROM web_v1.funnel_by_source ORDER BY day, source",
    )
    .fetch_all(&pool)
    .await
    .expect("funnel_by_source");
    assert_eq!(
        by_source,
        vec![
            (day_a.to_owned(), "abc123".to_owned(), 2),
            (day_a.to_owned(), "unknown".to_owned(), 1),
            (day_b.to_owned(), "abc123".to_owned(), 1),
        ]
    );

    // Leavers stay visible as non-current; the projection-less joiners and
    // the bot get no profile row at all.
    let members: Vec<(String, bool)> = sqlx::query_as(
        "SELECT member_id, is_current_member FROM web_v1.members ORDER BY member_id",
    )
    .fetch_all(&pool)
    .await
    .expect("members view");
    assert_eq!(
        members,
        vec![("m1".to_owned(), false), ("m2".to_owned(), false),]
    );

    fixture.close().await.expect("drop test database");
}

#[tokio::test]
async fn upcoming_and_next_serve_scheduled_and_active_only_as_utc() {
    let fixture = connect().await;
    let pool = fixture.pool().clone();
    let gate = JobGate::default();

    // Fixed-offset input proves the contract publishes UTC instants: a wall
    // clock in +01:00 renders back as the same instant in Z.
    let instant = time::OffsetDateTime::now_utc() + time::Duration::hours(72);
    let wall = instant + time::Duration::hours(1);
    let offset_input = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}+01:00",
        wall.year(),
        u8::from(wall.month()),
        wall.day(),
        wall.hour(),
        wall.minute(),
        wall.second(),
        wall.nanosecond() / 1_000_000
    );
    let offset_utc = format_iso_utc(instant);

    let active_start = offset_iso_hours(-2);
    let sched_start = future_iso(2);
    let rows = normalize_events(&[
        raw_event("active-started", &active_start, 2),
        raw_event("sched-future", &sched_start, 1),
        raw_event("offset-zone", &offset_input, 1),
        raw_event("done", &future_iso(5), 3),
        raw_event("nix", &future_iso(5), 4),
        raw_event("far", &future_iso(100), 1),
        raw_event("stale-sched", &offset_iso_hours(-2), 1),
    ])
    .expect("mirror normalizes");
    {
        let _guard = gate.try_acquire().expect("acquire");
        replace_events(&pool, GUILD, OBSERVED_AT, &rows)
            .await
            .expect("swap mirror");
    }

    // Completed, cancelled, beyond-90d and past-but-still-scheduled rows are
    // hidden; the started-but-active event still leads in start order.
    type UpcomingRow = (String, String, String, Option<String>, Option<String>);
    let upcoming: Vec<UpcomingRow> = sqlx::query_as(
        "SELECT event_id, name, starts_at, channel_id, description
         FROM web_v1.upcoming_events",
    )
    .fetch_all(&pool)
    .await
    .expect("upcoming view");
    assert_eq!(upcoming.len(), 3);
    assert_eq!(upcoming[0].0, "active-started");
    assert_eq!(upcoming[0].2, active_start);
    assert_eq!(upcoming[1].0, "sched-future");
    assert_eq!(upcoming[1].2, sched_start);
    assert_eq!(
        upcoming[2],
        (
            "offset-zone".to_owned(),
            "Event offset-zone".to_owned(),
            offset_utc,
            Some("voice-1".to_owned()),
            None,
        )
    );
    for row in &upcoming {
        assert!(
            row.2.ends_with('Z') && row.2.contains('T'),
            "non-UTC instant served: {}",
            row.2
        );
    }

    let next: Option<(String,)> = sqlx::query_as("SELECT event_id FROM web_v1.next_event")
        .fetch_optional(&pool)
        .await
        .expect("next view");
    assert_eq!(next.map(|(id,)| id), Some("active-started".to_owned()));

    // Zero rows is a correct answer: the empty state stays reachable and no
    // placeholder is ever invented.
    {
        let _guard = gate.try_acquire().expect("acquire");
        replace_events(&pool, GUILD, OBSERVED_AT, &[])
            .await
            .expect("empty swap");
    }
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM web_v1.upcoming_events")
        .fetch_one(&pool)
        .await
        .expect("upcoming count");
    assert_eq!(count.0, 0);
    let next: Option<(String,)> = sqlx::query_as("SELECT event_id FROM web_v1.next_event")
        .fetch_optional(&pool)
        .await
        .expect("next empty");
    assert!(next.is_none());

    fixture.close().await.expect("drop test database");
}

#[tokio::test]
async fn contract_meta_pins_version_and_view_shapes() {
    // Any view-shape change is a contract version bump: update
    // WEB_CONTRACT_VERSION (website_store.rs), the 0300 seed default, and
    // this table together — or ship the change as a web_v2 schema instead.
    let fixture = connect().await;
    let pool = fixture.pool().clone();

    assert_eq!(WEB_CONTRACT_VERSION, "1.1");
    let (version, _guild): (String, Option<String>) =
        sqlx::query_as("SELECT contract_version, guild_id FROM web_contract_meta")
            .fetch_one(&pool)
            .await
            .expect("meta row");
    assert_eq!(version, WEB_CONTRACT_VERSION);

    // The served meta row carries the same version, with no guild invented
    // before the bot records one.
    let meta: (String, Option<String>) =
        sqlx::query_as("SELECT contract_version, guild_id FROM web_v1.contract_meta")
            .fetch_one(&pool)
            .await
            .expect("contract_meta");
    assert_eq!(meta, ("1.1".to_owned(), None));

    // Exact column shapes in ordinal order. CREATE OR REPLACE VIEW already
    // refuses renames/removes/retypes at deploy time; this pins appends too.
    let cols: Vec<(String, String)> = sqlx::query_as(
        "SELECT table_name, column_name FROM information_schema.columns
         WHERE table_schema = 'web_v1' ORDER BY table_name, ordinal_position",
    )
    .fetch_all(&pool)
    .await
    .expect("view columns");
    let mut by_view: HashMap<&str, Vec<&str>> = HashMap::new();
    for (view, col) in &cols {
        by_view.entry(view.as_str()).or_default().push(col.as_str());
    }
    let expected: &[(&str, &[&str])] = &[
        ("contract_meta", &["contract_version", "guild_id"]),
        ("funnel_by_source", &["day", "source", "joins"]),
        (
            "funnel_daily",
            &[
                "day",
                "joins",
                "leaves",
                "first_messages",
                "first_voice_sessions",
                "net_change",
            ],
        ),
        (
            "live_counts",
            &[
                "human_member_count",
                "online_count",
                "counts_updated_at",
                "online_updated_at",
            ],
        ),
        (
            "member_milestones",
            &["member_id", "milestone", "occurred_at", "detail"],
        ),
        (
            "members",
            &[
                "member_id",
                "joined_at",
                "tenure_days",
                "rank_key",
                "is_current_member",
            ],
        ),
        (
            "next_event",
            &["event_id", "name", "starts_at", "channel_id", "description"],
        ),
        (
            "rank_counts",
            &[
                "rank_key",
                "rank_label",
                "rank_order",
                "member_count",
                "holders_count",
                "snapshot_at",
            ],
        ),
        ("rank_ladder", &["rank_key", "role_id"]),
        (
            "upcoming_events",
            &["event_id", "name", "starts_at", "channel_id", "description"],
        ),
        (
            "voice_heat",
            &[
                "dow",
                "hour",
                "avg_in_voice",
                "sample_days",
                "coverage",
                "measured_through",
                "source",
            ],
        ),
    ];
    assert_eq!(by_view.len(), expected.len(), "view count drift");
    for &(view, columns) in expected {
        assert_eq!(
            by_view.get(view),
            Some(&columns.to_vec()),
            "shape drift: {view}"
        );
    }

    fixture.close().await.expect("drop test database");
}

/// One `web_v1.voice_heat` row, in view column order. `coverage` is the
/// per-cell figure (valid samples / 48); the other provenance columns are
/// grid-wide and identical on every row.
#[derive(sqlx::FromRow)]
struct VoiceHeatCell {
    dow: i32,
    hour: i32,
    avg_in_voice: Option<f64>,
    sample_days: i64,
    coverage: f64,
    measured_through: Option<String>,
    source: String,
}

/// `[UTC midnight - 28 days, UTC midnight)`, read from the database clock.
async fn window_bounds(pool: &Pool<Postgres>) -> (time::OffsetDateTime, time::OffsetDateTime) {
    let end: time::OffsetDateTime =
        sqlx::query_scalar("SELECT date_trunc('day', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'")
            .fetch_one(pool)
            .await
            .expect("UTC midnight");
    (end - time::Duration::days(28), end)
}

async fn heat_grid(pool: &Pool<Postgres>) -> Vec<VoiceHeatCell> {
    let rows: Vec<VoiceHeatCell> = sqlx::query_as(
        "SELECT dow, hour, avg_in_voice, sample_days, coverage, measured_through, source
         FROM web_v1.voice_heat ORDER BY dow, hour",
    )
    .fetch_all(pool)
    .await
    .expect("voice_heat grid");
    assert_eq!(rows.len(), 168, "the grid always carries all 7x24 cells");
    rows
}

/// Which hours `seed_window` writes. Every window hour gets `samples` (12
/// max) unless overridden; occupancy averages 4 per sample.
#[derive(Default)]
struct WindowSeed {
    samples: i32,
    /// One hour that gets a different sample count.
    short_hour: Option<(time::OffsetDateTime, i32)>,
    /// `(dow, hour)` of a cell that gets no rows at all.
    omit_cell: Option<(i32, i32)>,
    /// Hours before this instant are gap rows (zero samples).
    gap_before: Option<time::OffsetDateTime>,
}

/// Seed one `voice_heat_hours` row per window hour. Rows with samples > 0 are
/// observed 55 minutes into their hour; zero-sample rows are gaps with a NULL
/// `last_observed_at`.
async fn seed_window(pool: &Pool<Postgres>, seed: WindowSeed) {
    let (start, end) = window_bounds(pool).await;
    let (short_at, short_samples) = seed.short_hour.unzip();
    let (omit_dow, omit_hour) = seed.omit_cell.unzip();
    sqlx::query(
        "INSERT INTO voice_heat_hours (hour_start_utc, samples, occupancy_sum, last_observed_at)
         SELECT hs, s, s::bigint * 4, CASE WHEN s > 0 THEN hs + interval '55 minutes' END
         FROM (
           SELECT hs,
                  CASE WHEN hs < $1::timestamptz THEN 0
                       WHEN hs = $2::timestamptz THEN $3::int
                       ELSE $4::int END AS s
           FROM generate_series($5::timestamptz, $6::timestamptz - interval '1 hour', interval '1 hour') AS hs
           WHERE $7::int IS NULL OR NOT (
             (EXTRACT(ISODOW FROM hs AT TIME ZONE 'UTC')::int - 1) = $7::int
             AND EXTRACT(HOUR FROM hs AT TIME ZONE 'UTC')::int = $8::int
           )
         ) t",
    )
    .bind(seed.gap_before)
    .bind(short_at)
    .bind(short_samples)
    .bind(seed.samples)
    .bind(start)
    .bind(end)
    .bind(omit_dow)
    .bind(omit_hour)
    .execute(pool)
    .await
    .expect("seed window hours");
}

async fn insert_hour(
    pool: &Pool<Postgres>,
    at: time::OffsetDateTime,
    samples: i32,
    sum: i64,
    observed: Option<time::OffsetDateTime>,
) {
    sqlx::query(
        "INSERT INTO voice_heat_hours (hour_start_utc, samples, occupancy_sum, last_observed_at)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(at)
    .bind(samples)
    .bind(sum)
    .bind(observed)
    .execute(pool)
    .await
    .expect("seed hour");
}

async fn iso_of(pool: &Pool<Postgres>, at: time::OffsetDateTime) -> String {
    sqlx::query_scalar("SELECT web_v1._iso($1)")
        .bind(at)
        .fetch_one(pool)
        .await
        .expect("UTC timestamp text")
}

/// Every row must carry the same grid-wide provenance.
fn assert_shared(rows: &[VoiceHeatCell], days: i64, source: &str, through: Option<&str>) {
    for cell in rows {
        assert_eq!(
            cell.sample_days, days,
            "sample_days at {}/{}",
            cell.dow, cell.hour
        );
        assert_eq!(cell.source, source, "source at {}/{}", cell.dow, cell.hour);
        assert_eq!(
            cell.measured_through.as_deref(),
            through,
            "measured_through at {}/{}",
            cell.dow,
            cell.hour
        );
    }
}

#[tokio::test]
async fn voice_heat_empty_grid_reports_insufficient() {
    // No flushed hours: all 168 cells present with NULL averages, zero
    // provenance, NULL measured_through, and the insufficient label.
    let fixture = connect().await;
    let pool = fixture.pool().clone();

    let rows = heat_grid(&pool).await;
    for cell in &rows {
        assert!((0..7).contains(&cell.dow), "dow in range: {}", cell.dow);
        assert!((0..24).contains(&cell.hour), "hour in range: {}", cell.hour);
        assert!(
            cell.avg_in_voice.is_none(),
            "empty cells average nothing, never zero"
        );
        assert_eq!(cell.coverage, 0.0);
    }
    assert_shared(&rows, 0, "insufficient", None);

    fixture.close().await.expect("drop test database");
}

#[tokio::test]
async fn voice_heat_sparse_samples_stay_insufficient() {
    // A handful of flushed hours fills its cells but never lets the
    // grid-wide label flip: most cells have no samples at all.
    let fixture = connect().await;
    let pool = fixture.pool().clone();

    let (_, end) = window_bounds(&pool).await;
    let base = end - time::Duration::days(2);
    for g in 0..6 {
        let at = base + time::Duration::hours(g);
        insert_hour(&pool, at, 12, 60, Some(at + time::Duration::minutes(40))).await;
    }

    let rows = heat_grid(&pool).await;
    let through = iso_of(
        &pool,
        base + time::Duration::hours(5) + time::Duration::minutes(40),
    )
    .await;
    assert_shared(&rows, 1, "insufficient", Some(&through));
    let filled: Vec<&VoiceHeatCell> = rows.iter().filter(|c| c.avg_in_voice.is_some()).collect();
    assert_eq!(filled.len(), 6, "each seeded hour fills exactly its cell");
    for cell in &rows {
        match cell.avg_in_voice {
            // The seeded cells average 5.0 from 12 of 48 expected samples.
            Some(avg) => {
                assert_eq!(avg, 5.0);
                assert!((cell.coverage - 12.0 / 48.0).abs() < 1e-9);
            }
            None => assert_eq!(cell.coverage, 0.0),
        }
    }

    fixture.close().await.expect("drop test database");
}

#[tokio::test]
async fn voice_heat_dense_window_reports_measured() {
    // Twenty-eight full UTC days at 12 samples/hour: every cell holds 48/48,
    // sample_days is 28, and the label flips to measured. Hours outside
    // [UTC midnight - 28d, UTC midnight) never move the watermark.
    let fixture = connect().await;
    let pool = fixture.pool().clone();

    let (start, end) = window_bounds(&pool).await;
    seed_window(
        &pool,
        WindowSeed {
            samples: 12,
            ..WindowSeed::default()
        },
    )
    .await;
    // Excluded by the window: the hour before it and the current (incomplete)
    // UTC day, whose observation is later than anything inside the window.
    let before = start - time::Duration::hours(1);
    insert_hour(
        &pool,
        before,
        12,
        999,
        Some(before + time::Duration::minutes(50)),
    )
    .await;
    insert_hour(&pool, end, 12, 999, Some(end + time::Duration::minutes(10))).await;

    let rows = heat_grid(&pool).await;
    let through = iso_of(
        &pool,
        end - time::Duration::hours(1) + time::Duration::minutes(55),
    )
    .await;
    assert_shared(&rows, 28, "measured", Some(&through));
    for cell in &rows {
        assert!(
            (cell.coverage - 1.0).abs() < 1e-9,
            "full coverage: {}",
            cell.coverage
        );
        assert_eq!(
            cell.avg_in_voice,
            Some(4.0),
            "excluded rows must not leak in"
        );
    }

    fixture.close().await.expect("drop test database");
}

/// Seed a full window where the first occurrence of the cell five hours
/// after the window start holds only `short` samples (the other three hold
/// 12 each), and return the grid.
async fn short_cell_grid(short: i32) -> Vec<VoiceHeatCell> {
    let fixture = connect().await;
    let pool = fixture.pool().clone();
    let (start, _) = window_bounds(&pool).await;
    seed_window(
        &pool,
        WindowSeed {
            samples: 12,
            short_hour: Some((start + time::Duration::hours(5), short)),
            ..WindowSeed::default()
        },
    )
    .await;
    let rows = heat_grid(&pool).await;
    fixture.close().await.expect("drop test database");
    rows
}

#[tokio::test]
async fn voice_heat_cell_gate_is_39_of_48() {
    // 36 + 3 = 39/48 clears the per-cell gate everywhere; 36 + 2 = 38/48
    // makes a single thin cell refuse the whole shared source.
    let at_gate = short_cell_grid(3).await;
    let thin: Vec<&VoiceHeatCell> = at_gate.iter().filter(|c| c.coverage < 1.0).collect();
    assert_eq!(thin.len(), 1, "exactly one cell is short");
    assert!((thin[0].coverage - 39.0 / 48.0).abs() < 1e-9);
    assert_shared(
        &at_gate,
        28,
        "measured",
        at_gate[0].measured_through.as_deref(),
    );
    assert!(at_gate[0].measured_through.is_some());

    let under_gate = short_cell_grid(2).await;
    let thin: Vec<&VoiceHeatCell> = under_gate.iter().filter(|c| c.coverage < 1.0).collect();
    assert_eq!(thin.len(), 1, "exactly one cell is short");
    assert!((thin[0].coverage - 38.0 / 48.0).abs() < 1e-9);
    assert!(
        thin[0].avg_in_voice.is_some(),
        "a thin cell still reports its average"
    );
    assert_shared(
        &under_gate,
        28,
        "insufficient",
        under_gate[0].measured_through.as_deref(),
    );
    // The watermark is a property of the included hours, not of the label.
    assert!(under_gate[0].measured_through.is_some());
}

#[tokio::test]
async fn voice_heat_missing_cell_refuses_measured() {
    // Every other cell is full and 28 days are sampled, but one hour-of-week
    // has no rows at all: coverage 0 for it and an insufficient grid.
    let fixture = connect().await;
    let pool = fixture.pool().clone();
    seed_window(
        &pool,
        WindowSeed {
            samples: 12,
            omit_cell: Some((2, 5)),
            ..WindowSeed::default()
        },
    )
    .await;

    let rows = heat_grid(&pool).await;
    let missing = rows
        .iter()
        .find(|c| c.dow == 2 && c.hour == 5)
        .expect("cell");
    assert!(missing.avg_in_voice.is_none());
    assert_eq!(missing.coverage, 0.0);
    assert_eq!(rows.iter().filter(|c| c.coverage == 0.0).count(), 1);
    assert_shared(
        &rows,
        28,
        "insufficient",
        rows[0].measured_through.as_deref(),
    );

    fixture.close().await.expect("drop test database");
}

#[tokio::test]
async fn voice_heat_gap_rows_are_never_samples() {
    // All-gap rows (samples 0, NULL observation) fill nothing: no averages,
    // no sampled days, no watermark.
    let fixture = connect().await;
    let pool = fixture.pool().clone();
    seed_window(&pool, WindowSeed::default()).await;
    let rows = heat_grid(&pool).await;
    for cell in &rows {
        assert!(cell.avg_in_voice.is_none(), "a gap is never a zero average");
        assert_eq!(cell.coverage, 0.0);
    }
    assert_shared(&rows, 0, "insufficient", None);
    fixture.close().await.expect("drop test database");

    // One whole gap day among 27 sampled days: that date is not counted and
    // its weekday's cells fall to 36/48, so the grid cannot be measured.
    let fixture = connect().await;
    let pool = fixture.pool().clone();
    let (start, _) = window_bounds(&pool).await;
    seed_window(
        &pool,
        WindowSeed {
            samples: 12,
            gap_before: Some(start + time::Duration::days(1)),
            ..WindowSeed::default()
        },
    )
    .await;
    let rows = heat_grid(&pool).await;
    assert_shared(
        &rows,
        27,
        "insufficient",
        rows[0].measured_through.as_deref(),
    );
    assert!(rows[0].measured_through.is_some());
    let thin = rows
        .iter()
        .filter(|c| (c.coverage - 36.0 / 48.0).abs() < 1e-9)
        .count();
    assert_eq!(thin, 24, "the gap day's 24 cells each lose one occurrence");
    fixture.close().await.expect("drop test database");
}

#[tokio::test]
async fn voice_heat_valid_zero_occupancy_advances_watermark() {
    // An observed-but-empty hour (samples 12, sum 0) is a real measurement:
    // it averages 0.0, counts toward coverage and moves measured_through,
    // unlike a gap row sitting after it.
    let fixture = connect().await;
    let pool = fixture.pool().clone();
    let (_, end) = window_bounds(&pool).await;
    let last = end - time::Duration::hours(1);
    let zero_hour = last - time::Duration::hours(1);
    let observed = zero_hour + time::Duration::minutes(5);
    insert_hour(&pool, zero_hour, 12, 0, Some(observed)).await;
    insert_hour(&pool, last, 0, 0, None).await;

    let rows = heat_grid(&pool).await;
    let through = iso_of(&pool, observed).await;
    assert_shared(&rows, 1, "insufficient", Some(&through));
    let zero: Vec<&VoiceHeatCell> = rows
        .iter()
        .filter(|c| c.avg_in_voice == Some(0.0))
        .collect();
    assert_eq!(zero.len(), 1, "the empty hour averages 0.0, not NULL");
    assert!((zero[0].coverage - 12.0 / 48.0).abs() < 1e-9);
    assert_eq!(
        rows.iter()
            .filter(|c| c.coverage == 0.0 && c.avg_in_voice.is_none())
            .count(),
        167,
        "the gap hour and all unobserved cells are gaps, not zero occupancy"
    );

    fixture.close().await.expect("drop test database");
}

#[tokio::test]
async fn voice_heat_watermark_follows_observation_time_not_counts() {
    // Two hours with identical samples and occupancy differ only in
    // last_observed_at; the watermark is the latest observation timestamp.
    let fixture = connect().await;
    let pool = fixture.pool().clone();
    let (start, _) = window_bounds(&pool).await;
    let early_hour = start + time::Duration::hours(3);
    let late_hour = start + time::Duration::hours(9);
    insert_hour(
        &pool,
        early_hour,
        12,
        60,
        Some(early_hour + time::Duration::minutes(10)),
    )
    .await;
    insert_hour(
        &pool,
        late_hour,
        12,
        60,
        Some(late_hour + time::Duration::minutes(20)),
    )
    .await;
    let first = iso_of(&pool, late_hour + time::Duration::minutes(20)).await;
    assert_shared(&heat_grid(&pool).await, 1, "insufficient", Some(&first));

    // Re-observe the later hour later within that same hour: counts never
    // change, the watermark does.
    sqlx::query("UPDATE voice_heat_hours SET last_observed_at = $1 WHERE hour_start_utc = $2")
        .bind(late_hour + time::Duration::minutes(50))
        .bind(late_hour)
        .execute(&pool)
        .await
        .expect("move observation within hour");
    let second = iso_of(&pool, late_hour + time::Duration::minutes(50)).await;
    assert_ne!(first, second);
    assert_shared(&heat_grid(&pool).await, 1, "insufficient", Some(&second));

    fixture.close().await.expect("drop test database");
}

#[tokio::test]
async fn voice_heat_hours_constraints_bind_observation_to_samples() {
    // last_observed_at is NULL iff samples = 0; samples cap at 12; the
    // timestamp lies inside its UTC hour; zero samples carry a zero sum.
    let fixture = connect().await;
    let pool = fixture.pool().clone();
    let (start, _) = window_bounds(&pool).await;
    let base = start + time::Duration::hours(2);
    let minute = time::Duration::minutes(1);
    let cases: [(&str, i32, i64, Option<time::OffsetDateTime>, bool); 8] = [
        (
            "valid sample",
            12,
            30,
            Some(base + time::Duration::minutes(59)),
            true,
        ),
        ("valid gap", 0, 0, None, true),
        ("samples without observation", 5, 10, None, false),
        ("gap with observation", 0, 0, Some(base + minute), false),
        ("over twelve samples", 13, 10, Some(base + minute), false),
        (
            "observation before hour",
            5,
            10,
            Some(base - time::Duration::seconds(1)),
            false,
        ),
        (
            "observation after hour",
            5,
            10,
            Some(base + time::Duration::hours(1)),
            false,
        ),
        ("zero samples with sum", 0, 3, None, false),
    ];
    for (name, samples, sum, observed, ok) in cases {
        sqlx::query("DELETE FROM voice_heat_hours")
            .execute(&pool)
            .await
            .expect("reset");
        let result = sqlx::query(
            "INSERT INTO voice_heat_hours (hour_start_utc, samples, occupancy_sum, last_observed_at)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(base)
        .bind(samples)
        .bind(sum)
        .bind(observed)
        .execute(&pool)
        .await;
        assert_eq!(result.is_ok(), ok, "{name}: {result:?}");
    }

    fixture.close().await.expect("drop test database");
}

#[tokio::test]
async fn voice_heat_uses_utc_bounds_and_sample_weighted_cells() {
    let fixture = connect().await;
    let mut tx = fixture.pool().begin().await.expect("transaction");
    // UTC midnight must mean the same instant on every database connection.
    sqlx::query("SET LOCAL TIME ZONE 'Pacific/Auckland'")
        .execute(&mut *tx)
        .await
        .expect("non-UTC session");
    let today: time::OffsetDateTime =
        sqlx::query_scalar("SELECT date_trunc('day', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'")
            .fetch_one(&mut *tx)
            .await
            .expect("UTC midnight");
    let start = today - time::Duration::days(28);
    let minutes = |n: i64| time::Duration::minutes(n);
    // Two occurrences of the same hour-of-week with unequal sample counts.
    // The row just before the window and today's row must be excluded (their
    // observations are the latest and earliest timestamps, so a leak would
    // show in the watermark too). A gap row on another date counts nowhere.
    for (at, samples, sum, observed) in [
        (
            start - time::Duration::hours(1),
            12i32,
            1200i64,
            Some(start - minutes(1)),
        ),
        (start, 12, 24, Some(start + minutes(20))),
        (
            start + time::Duration::days(3) + time::Duration::hours(3),
            0,
            0,
            None,
        ),
        (
            start + time::Duration::days(7),
            6,
            36,
            Some(start + time::Duration::days(7) + minutes(30)),
        ),
        (today, 12, 1200, Some(today + minutes(10))),
    ] {
        sqlx::query("INSERT INTO voice_heat_hours (hour_start_utc, samples, occupancy_sum, last_observed_at) VALUES ($1, $2, $3, $4)")
            .bind(at).bind(samples).bind(sum).bind(observed).execute(&mut *tx).await.expect("seed hour");
    }
    let rows: Vec<VoiceHeatCell> = sqlx::query_as(
        "SELECT dow, hour, avg_in_voice, sample_days, coverage, measured_through, source
         FROM web_v1.voice_heat ORDER BY dow, hour",
    )
    .fetch_all(&mut *tx)
    .await
    .expect("grid");
    assert_eq!(rows.len(), 168);
    let filled: Vec<&VoiceHeatCell> = rows
        .iter()
        .filter(|cell| cell.avg_in_voice.is_some())
        .collect();
    assert_eq!(
        filled.len(),
        1,
        "excluded boundary hours must not leak into cells"
    );
    let cell = filled[0];
    assert_eq!(
        cell.dow,
        i32::from(start.weekday().number_days_from_monday())
    );
    assert_eq!(cell.hour, 0);
    assert!((cell.avg_in_voice.expect("measured average") - 60.0 / 18.0).abs() < 1e-9);
    assert_eq!(
        cell.sample_days, 2,
        "grid-wide distinct dates with samples > 0"
    );
    assert!((cell.coverage - 18.0 / 48.0).abs() < 1e-9);
    assert_eq!(cell.source, "insufficient");
    let through: String = sqlx::query_scalar("SELECT web_v1._iso($1)")
        .bind(start + time::Duration::days(7) + minutes(30))
        .fetch_one(&mut *tx)
        .await
        .expect("UTC timestamp");
    for row in &rows {
        assert_eq!(row.sample_days, 2);
        assert_eq!(row.measured_through.as_deref(), Some(through.as_str()));
    }
    tx.rollback().await.expect("rollback fixture");
    fixture.close().await.expect("drop test database");
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
