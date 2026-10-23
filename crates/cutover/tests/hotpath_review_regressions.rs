//! Supplemental acceptance for complete projections, materialized fetch bounds,
//! and continuation past guarded deletions. The original handoff stays frozen.
//! Breaks caught: first-page-only membership/retention; independently removing
//! any one database fetch bound even when the caller still takes 25 rows.

#[path = "../../core/tests/support/hotpath_queries.rs"]
mod hotpath_queries;

use hotpath_queries::{assert_fetch_bound, Queries};
use two_bot_core::membership::{Membership, MembershipStore};
use two_bot_cutover::retention::{handle_guild_delete, run_retention, RetentionMode};
use two_bot_cutover::settings::SettingsStore;
use two_bot_cutover::voice_rooms::PgRoomStore;
use two_bot_store::PgFunnelStore;
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "111111111111111111";
const MEMBER: &str = "222";
const T0: i64 = 1_800_000_000;

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("hot-path regression requires disposable TWO_TEST_DATABASE_URL");
    TestDatabase::create(&url, &sqlx::migrate!("./migrations"))
        .await
        .unwrap()
}

async fn event(pool: &sqlx::PgPool, kind: &str, second: i32, source: &str) {
    sqlx::query(
        "INSERT INTO events (event_type, guild_id, member_id, occurred_at, source, idempotency_key)
         VALUES ($1, $2, $3, '2026-08-15T00:00:00Z'::timestamptz + make_interval(secs => $4), $5, $6)",
    )
    .bind(kind).bind(GUILD).bind(MEMBER).bind(f64::from(second)).bind(source)
    .bind(format!("review-{kind}-{second}"))
    .execute(pool).await.unwrap();
}

fn projection(store: &PgFunnelStore, want: Membership) {
    assert_eq!(
        store.membership(111_111_111_111_111_111, 222),
        Some(want.clone()),
        "trait must fold later decisive evidence"
    );
    let page = store
        .membership_paged(111_111_111_111_111_111, 222)
        .unwrap();
    assert!(page.truncated, "fixture crosses the first page");
    assert_eq!(
        page.membership, want,
        "marker must not conceal a partial projection"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn leave_rejoin_and_inactivity_after_row_25_change_both_projections() {
    let db = database().await;
    event(db.pool(), "member_join", 0, "initial").await;
    for second in 1..25 {
        event(db.pool(), "invite_click", second, "padding").await;
    }
    let store = PgFunnelStore::new(db.pool().clone());
    event(db.pool(), "member_leave", 25, "departure").await;
    projection(
        &store,
        Membership {
            joined_at: Some("2026-08-15T00:00:00.000000Z".into()),
            join_source: Some("initial".into()),
            left_at: Some("2026-08-15T00:00:25.000000Z".into()),
            inactive_flagged_at: None,
        },
    );
    event(db.pool(), "member_join", 26, "superseding").await;
    projection(
        &store,
        Membership {
            joined_at: Some("2026-08-15T00:00:26.000000Z".into()),
            join_source: Some("superseding".into()),
            left_at: None,
            inactive_flagged_at: None,
        },
    );
    event(db.pool(), "member_inactive", 27, "inactive").await;
    projection(
        &store,
        Membership {
            joined_at: Some("2026-08-15T00:00:26.000000Z".into()),
            join_source: Some("superseding".into()),
            left_at: None,
            inactive_flagged_at: Some("2026-08-15T00:00:27.000000Z".into()),
        },
    );
    db.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn membership_actual_fetches_never_materialize_more_than_26_rows() {
    let db = database().await;
    event(db.pool(), "member_join", 0, "initial").await;
    for second in 1..60 {
        event(db.pool(), "invite_click", second, "padding").await;
    }
    let store = PgFunnelStore::new(db.pool().clone());
    // Observe each public entry point separately: either may accidentally bypass paging.
    let (result, receipts) =
        Queries::capture_sync(|| store.membership_paged(111_111_111_111_111_111, 222));
    assert!(result.unwrap().truncated);
    assert_fetch_bound(
        &receipts,
        |q| q.contains("from events") && q.contains("event_type"),
        26,
    );
    let (result, receipts) =
        Queries::capture_sync(|| store.membership(111_111_111_111_111_111, 222));
    assert!(result.is_some());
    assert_fetch_bound(
        &receipts,
        |q| q.contains("from events") && q.contains("event_type"),
        26,
    );
    db.close().await.unwrap();
}

async fn pending(pool: &sqlx::PgPool, index: usize) {
    sqlx::query(
        "INSERT INTO retention_deletion_requests (request_id, guild_id, user_id)
         VALUES ($1, $2, $3)",
    )
    .bind(format!("pending-{index:03}"))
    .bind(GUILD)
    .bind(format!("{index:018}"))
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn retention_pending_actual_fetch_is_bounded_independently_of_output_cap() {
    let db = database().await;
    for index in 0..60 {
        pending(db.pool(), index).await;
    }
    let (report, receipts) =
        Queries::capture(run_retention(db.pool(), RetentionMode::DryRun)).await;
    let report = report.unwrap();
    assert_eq!(report.deletions_completed, 25);
    assert!(report.pending_truncated);
    assert_fetch_bound(
        &receipts,
        |q| q.contains("select request_id") && q.contains("from retention_deletion_requests"),
        26,
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn burst_actual_fetch_is_bounded_independently_of_output_cap() {
    let db = database().await;
    for index in 0..60 {
        sqlx::query("INSERT INTO voice_create_reservations (guild_id, user_id, created_at) VALUES ($1, $2, to_timestamp($3::double precision))")
            .bind(GUILD).bind(format!("{}", 300 + index)).bind(T0 + index % 30)
            .execute(db.pool()).await.unwrap();
    }
    let store = PgRoomStore::new(db.pool().clone());
    let (page, receipts) =
        Queries::capture(store.burst_history_page(111_111_111_111_111_111, T0 + 30)).await;
    let page = page.unwrap();
    assert_eq!(page.reservations.len(), 25);
    assert!(page.truncated);
    assert_fetch_bound(
        &receipts,
        |q| q.contains("from voice_create_reservations") && q.contains("select user_id"),
        26,
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn guild_poll_count_is_local_even_when_revision_is_global() {
    let db = database().await;
    let store = SettingsStore::new(db.pool());
    for index in 0..30 {
        store
            .set(
                &format!("{}", 300 + index),
                "TWO_RAID_JOIN_THRESHOLD",
                Some(serde_json::json!(9)),
                "fixture",
            )
            .await
            .unwrap();
    }
    for key in ["TWO_RAID_JOIN_THRESHOLD", "TWO_RAID_WINDOW_SECONDS"] {
        store
            .set(GUILD, key, Some(serde_json::json!(3)), "fixture")
            .await
            .unwrap();
    }
    let (global_rev, global_count) = store.poll_marks().await.unwrap();
    let (guild_rev, guild_count) = store.poll_guild_marks(GUILD).await.unwrap();
    assert_eq!(global_count, 32);
    assert_eq!(
        guild_count, 2,
        "global count would keep a scoped cache perpetually stale"
    );
    assert_eq!(guild_rev, global_rev);
    assert_eq!(store.poll_guild_marks("999").await.unwrap().1, 0);
    db.close().await.unwrap();
}

async fn guard_rows(pool: &sqlx::PgPool) -> Vec<(String, String, String)> {
    sqlx::query_as("SELECT key_hash, state, target_id FROM internal_idempotency ORDER BY key_hash")
        .fetch_all(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn guarded_prefix_cannot_starve_suffix_in_either_mode_or_double_count_overlap() {
    let db = database().await;
    for index in 0..30 {
        pending(db.pool(), index).await;
        sqlx::query("INSERT INTO members (guild_id, member_id) VALUES ($1, $2)")
            .bind(GUILD)
            .bind(format!("{index:018}"))
            .execute(db.pool())
            .await
            .unwrap();
        if index < 25 {
            sqlx::query(
                "INSERT INTO internal_idempotency
                 (caller_hash, key_hash, action, payload_hash, state, guild_id, actor_id, target_id)
                 VALUES (repeat('a',64), $1, 'role.assign', repeat('c',64), 'unknown', $2, $3, $3)",
            )
            .bind(format!("{index:064x}"))
            .bind(GUILD)
            .bind(format!("{index:018}"))
            .execute(db.pool())
            .await
            .unwrap();
        }
    }
    // Guarded guild purge and time-window work overlap the eligible suffix.
    sqlx::query(
        "INSERT INTO events (event_type, guild_id, member_id, occurred_at, source, idempotency_key)
         VALUES ('voice_session_start', $1, $2, now() - interval '31 days', 'fixture', 'old-suffix'),
                ('voice_session_start', $1, $2, now(), 'fixture', 'fresh-suffix')",
    ).bind(GUILD).bind(format!("{:018}", 25)).execute(db.pool()).await.unwrap();
    sqlx::query(
        "INSERT INTO presence_probe (guild_id, observed_at, approximate_presence_count, bot_floor)
         VALUES ($1, to_char(now() - interval '31 days', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), 10, NULL),
                ($1, to_char(now(), 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), 11, NULL)",
    ).bind(GUILD).execute(db.pool()).await.unwrap();
    assert!(handle_guild_delete(db.pool(), GUILD, None).await.unwrap());
    let guards_before = guard_rows(db.pool()).await;
    assert_eq!(guards_before.len(), 25);
    let dry = run_retention(db.pool(), RetentionMode::DryRun)
        .await
        .unwrap();
    assert_eq!(
        (
            dry.deletions_completed,
            dry.deletions_deferred,
            dry.schema_gap_deferrals
        ),
        (5, 25, 0),
        "all 30 requests must be selected past the guarded first page"
    );
    assert!(!dry.pending_truncated, "all pending work was inspected");
    assert_eq!(
        (
            dry.presence_raw_deleted,
            dry.voice_raw_deleted,
            dry.guild_rows_purged,
            dry.guild_purges_deferred,
            dry.guild_purges_failed
        ),
        (1, 1, 0, 1, 0)
    );
    let untouched: i64 = sqlx::query_scalar("SELECT count(*) FROM retention_deletion_requests WHERE status = 'pending' AND receipt_id IS NULL AND completed_at IS NULL").fetch_one(db.pool()).await.unwrap();
    assert_eq!(untouched, 30, "DryRun never settles receipts");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM members")
            .fetch_one(db.pool())
            .await
            .unwrap(),
        30
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM events")
            .fetch_one(db.pool())
            .await
            .unwrap(),
        2
    );
    let live = run_retention(db.pool(), RetentionMode::Execute)
        .await
        .unwrap();
    assert_eq!(
        (
            live.deletions_completed,
            live.deletions_deferred,
            live.schema_gap_deferrals
        ),
        (5, 25, 0)
    );
    assert!(!live.pending_truncated);
    assert_eq!(
        (
            live.presence_raw_deleted,
            live.voice_raw_deleted,
            live.guild_rows_purged,
            live.guild_purges_deferred,
            live.guild_purges_failed
        ),
        (1, 1, 0, 1, 0)
    );
    let ledger: Vec<(String, String, Option<String>, bool)> = sqlx::query_as(
        "SELECT request_id, status, receipt_id, completed_at IS NOT NULL FROM retention_deletion_requests ORDER BY request_id",
    ).fetch_all(db.pool()).await.unwrap();
    assert_eq!(ledger.len(), 30);
    let mut receipts = std::collections::HashSet::new();
    for (index, (id, status, receipt, completed)) in ledger.iter().enumerate() {
        assert_eq!(id, &format!("pending-{index:03}"));
        if index < 25 {
            assert_eq!(
                (status.as_str(), receipt.as_deref(), *completed),
                ("pending", None, false)
            );
        } else {
            assert_eq!((status.as_str(), *completed), ("completed", true));
            let receipt = receipt.as_deref().unwrap();
            assert!(!receipt.is_empty());
            assert!(receipts.insert(receipt), "completion receipts are unique");
        }
    }
    assert_eq!(
        guard_rows(db.pool()).await,
        guards_before,
        "replay fences are never erased or settled by retention"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM members")
            .fetch_one(db.pool())
            .await
            .unwrap(),
        25
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM events")
            .fetch_one(db.pool())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM presence_probe")
            .fetch_one(db.pool())
            .await
            .unwrap(),
        1
    );
    let again = run_retention(db.pool(), RetentionMode::Execute)
        .await
        .unwrap();
    assert_eq!(
        (again.deletions_completed, again.deletions_deferred),
        (0, 25)
    );
    assert!(!again.pending_truncated);
    assert_eq!(guard_rows(db.pool()).await, guards_before);
    db.close().await.unwrap();
}
