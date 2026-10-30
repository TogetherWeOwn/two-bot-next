//! Acceptance, marker half: the scoped/capped read primitives and their
//! fixed-vocabulary truncation markers (hot-path bounds item B1: settings,
//! retention, burst, funnel).
//!
//! Protected test-first handoff: the paired Builder implements exactly the
//! surface below without editing this file, reuses it on the five hot paths
//! (a per-event settings refresh never rescans other guilds' rows; the
//! retention/burst/funnel row fetches take the 25-row cap), and documents
//! the cap next to each query. These tests pass unmodified once the contract
//! exists. Never merge this deliberately red test-only branch.
//!
//! ## Builder contract (implement exactly this surface)
//!
//! ```rust,ignore
//! // crates/cutover/src/settings.rs — same MVCC snapshot shape as
//! // `load_snapshot` (global revision + rows), rows restricted to one guild.
//! impl SettingsStore<'_> {
//!     pub async fn load_guild_snapshot(
//!         &self,
//!         guild_id: &str,
//!     ) -> Result<SettingsSnapshot, sqlx::Error>;
//! }
//!
//! // crates/cutover/src/retention.rs — one new field on the existing report.
//! pub struct RetentionReport {
//!     // ... existing fields unchanged ...
//!     /// True when the pending-request row fetch hit the 25-row cap: the
//!     /// per-request counts cover the probed prefix only. Counts from
//!     /// `COUNT(*)` stay exact.
//!     pub pending_truncated: bool,
//! }
//!
//! // crates/cutover/src/voice_rooms.rs — newest-first TOP-25 history page.
//! pub struct BurstHistoryPage {
//!     pub reservations: Vec<AcceptedReservation>,
//!     pub truncated: bool,
//! }
//! impl PgRoomStore {
//!     pub async fn burst_history_page(
//!         &self,
//!         guild_id: Snowflake,
//!         now_secs: i64,
//!     ) -> Result<BurstHistoryPage, sqlx::Error>;
//! }
//!
//! // crates/store/src/store.rs — membership with a history receipt, sync
//! // like `MembershipStore::membership`.
//! pub struct MembershipPage {
//!     pub membership: Membership,
//!     pub truncated: bool,
//! }
//! impl PgFunnelStore {
//!     pub fn membership_paged(
//!         &self,
//!         guild_id: Snowflake,
//!         member_id: Snowflake,
//!     ) -> Option<MembershipPage>;
//! }
//! ```
//!
//! RED status on the unmodified tree: this target fails to compile
//! (`E0599` on the three new methods, `E0609` on `pending_truncated`).
//! That is the missing-surface proof, not an infra refusal: the sibling
//! `hotpath_bounds_behavioral_acceptance` target compiles and shows the
//! behavioral RED (retention completes 30 per run, want 25).
//!
//! Database tests use the authorized disposable test database only
//! (`TWO_TEST_DATABASE_URL`, `agent-testdb` principal): they skip with a
//! diagnostic when the URL is absent locally and fail closed in CI.

use serde_json::json;
use two_bot_cutover::retention::{request_user_deletion, run_retention, RetentionMode};
use two_bot_cutover::settings::SettingsStore;
use two_bot_cutover::voice_rooms::PgRoomStore;
use two_bot_store::PgFunnelStore;
use two_bot_testsupport::TestDatabase;

const READ_CAP: usize = 25;
const OVER_CAP: usize = 30;

// --- shared fixture -------------------------------------------------------

fn bootstrap_url(test: &str) -> Option<String> {
    match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure hotpath-marker acceptance"
            );
            eprintln!("SKIP {test}: TWO_TEST_DATABASE_URL not set");
            None
        }
    }
}

async fn fixture(bootstrap: &str) -> TestDatabase {
    TestDatabase::create(bootstrap, &sqlx::migrate!("./migrations"))
        .await
        .unwrap()
}

// --- settings -------------------------------------------------------------

const SGUILD: &str = "100000000000000001";
const SKEY: &str = "TWO_RAID_JOIN_THRESHOLD";
const SKEY2: &str = "TWO_RAID_WINDOW_SECONDS";
const SKEY3: &str = "TWO_JOIN_RISK_THRESHOLD";
const SACTOR: &str = "hotpath-acceptance";

fn other_guild(i: usize) -> String {
    format!("200000000000{i:06}")
}

async fn seed_other_guilds(pool: &sqlx::PgPool) {
    let store = SettingsStore::new(pool);
    for i in 0..OVER_CAP {
        store
            .set(&other_guild(i), SKEY, Some(json!(i as i64)), SACTOR)
            .await
            .expect("other-guild write stores");
    }
}

#[tokio::test]
async fn guild_snapshot_returns_only_the_target_guild() {
    let Some(bootstrap) = bootstrap_url("settings guild snapshot") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    seed_other_guilds(db.pool()).await;
    let store = SettingsStore::new(db.pool());
    for (key, value) in [(SKEY, 3), (SKEY2, 30), (SKEY3, 5)] {
        store
            .set(SGUILD, key, Some(json!(value)), SACTOR)
            .await
            .expect("target write stores");
    }
    let snapshot = store
        .load_guild_snapshot(SGUILD)
        .await
        .expect("scoped snapshot loads");
    assert_eq!(snapshot.rows.len(), 3, "only the target guild's rows");
    for row in &snapshot.rows {
        assert_eq!(
            row.guild_id, SGUILD,
            "a per-event refresh never rescans other guilds' rows"
        );
    }
    let (revision, _) = store.poll_marks().await.expect("poll marks read");
    assert_eq!(
        snapshot.revision, revision,
        "the scoped snapshot carries the global revision"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn guild_snapshot_empty_guild_has_no_rows() {
    let Some(bootstrap) = bootstrap_url("settings guild snapshot empty") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    seed_other_guilds(db.pool()).await;
    let store = SettingsStore::new(db.pool());
    let snapshot = store
        .load_guild_snapshot(SGUILD)
        .await
        .expect("scoped snapshot loads");
    assert!(
        snapshot.rows.is_empty(),
        "a guild with no rows snapshots empty"
    );
    let (revision, _) = store.poll_marks().await.expect("poll marks read");
    assert_eq!(snapshot.revision, revision);
    db.close().await.unwrap();
}

// --- retention ------------------------------------------------------------

const RGUILD: &str = "1545644954272137297";

fn retention_user(i: usize) -> String {
    format!("730000000000{i:06}")
}

async fn seed_deletion_request(pool: &sqlx::PgPool, user: &str) {
    sqlx::query("INSERT INTO members (guild_id, member_id) VALUES ($1, $2)")
        .bind(RGUILD)
        .bind(user)
        .execute(pool)
        .await
        .expect("seed member row");
    sqlx::query(
        "INSERT INTO member_levels (guild_id, member_id, xp, message_xp, updated_at)
         VALUES ($1, $2, 150, 150, now())",
    )
    .bind(RGUILD)
    .bind(user)
    .execute(pool)
    .await
    .expect("seed aggregate row");
    sqlx::query(
        "INSERT INTO events (event_type, member_id, guild_id, occurred_at, source, metadata, idempotency_key)
         VALUES ('voice_session_started', $2, $1, now(), 'fixture', '{}', $3)",
    )
    .bind(RGUILD)
    .bind(user)
    .bind(format!("hotpath-mdel-{user}"))
    .execute(pool)
    .await
    .expect("seed event row");
    request_user_deletion(pool, RGUILD, user)
        .await
        .expect("request queues");
}

#[tokio::test]
async fn dry_run_marks_truncated_over_cap() {
    let Some(bootstrap) = bootstrap_url("retention dry run marker") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    for i in 0..OVER_CAP {
        seed_deletion_request(db.pool(), &retention_user(i)).await;
    }
    let report = run_retention(db.pool(), RetentionMode::DryRun)
        .await
        .expect("dry run reports");
    assert_eq!(report.deletions_completed, READ_CAP as u64);
    assert!(
        report.pending_truncated,
        "30 pendings over a 25-row fetch set the marker"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn dry_run_unmarked_at_cap() {
    let Some(bootstrap) = bootstrap_url("retention dry run unmarked") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    for i in 0..READ_CAP {
        seed_deletion_request(db.pool(), &retention_user(i)).await;
    }
    let report = run_retention(db.pool(), RetentionMode::DryRun)
        .await
        .expect("dry run reports");
    assert_eq!(report.deletions_completed, READ_CAP as u64);
    assert!(
        !report.pending_truncated,
        "exactly 25 pendings fit the fetch: no marker"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn execute_marks_first_pass_only() {
    let Some(bootstrap) = bootstrap_url("retention execute marker") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    for i in 0..OVER_CAP {
        seed_deletion_request(db.pool(), &retention_user(i)).await;
    }
    let first = run_retention(db.pool(), RetentionMode::Execute)
        .await
        .expect("first drain runs");
    assert_eq!(first.deletions_completed, READ_CAP as u64);
    assert!(first.pending_truncated, "the capped first pass marks");
    let second = run_retention(db.pool(), RetentionMode::Execute)
        .await
        .expect("second drain runs");
    assert_eq!(second.deletions_completed, (OVER_CAP - READ_CAP) as u64);
    assert!(
        !second.pending_truncated,
        "the 5-row remainder fits: no marker"
    );
    db.close().await.unwrap();
}

// --- burst ----------------------------------------------------------------

const BGUILD: u64 = 100;
const BT0: i64 = 1_800_000_000;

async fn seed_reservation(pool: &sqlx::PgPool, user: u64, created_secs: i64) {
    sqlx::query(
        "INSERT INTO voice_create_reservations (guild_id, user_id, created_at)
         VALUES ($1, $2, to_timestamp($3::double precision))",
    )
    .bind(BGUILD.to_string())
    .bind(user.to_string())
    .bind(created_secs)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn burst_page_caps_newest_first_with_marker() {
    let Some(bootstrap) = bootstrap_url("burst page cap") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    let store = PgRoomStore::new(db.pool().clone());
    for i in 0..OVER_CAP {
        seed_reservation(db.pool(), 1000 + i as u64, BT0 + i as i64).await;
    }
    let page = store
        .burst_history_page(BGUILD, BT0 + 30)
        .await
        .expect("history page reads");
    assert_eq!(
        page.reservations.len(),
        READ_CAP,
        "the page holds TOP-25, got {}",
        page.reservations.len()
    );
    assert!(page.truncated, "30 rows over a 25-row page set the marker");
    let mut stamps: Vec<i64> = page
        .reservations
        .iter()
        .map(|row| row.created_at_secs)
        .collect();
    stamps.sort_unstable();
    assert_eq!(
        stamps.first(),
        Some(&(BT0 + 5)),
        "the page keeps the 25 newest rows"
    );
    assert_eq!(stamps.last(), Some(&(BT0 + 29)));
    db.close().await.unwrap();
}

#[tokio::test]
async fn burst_page_unmarked_at_cap() {
    let Some(bootstrap) = bootstrap_url("burst page at cap") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    let store = PgRoomStore::new(db.pool().clone());
    for i in 0..READ_CAP {
        seed_reservation(db.pool(), 1100 + i as u64, BT0 + i as i64).await;
    }
    let page = store
        .burst_history_page(BGUILD, BT0 + 30)
        .await
        .expect("history page reads");
    assert_eq!(page.reservations.len(), READ_CAP);
    assert!(!page.truncated, "exactly 25 rows fit the page: no marker");
    db.close().await.unwrap();
}

#[tokio::test]
async fn burst_page_empty_guild() {
    let Some(bootstrap) = bootstrap_url("burst page empty") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    let store = PgRoomStore::new(db.pool().clone());
    let page = store
        .burst_history_page(BGUILD, BT0 + 30)
        .await
        .expect("history page reads");
    assert!(page.reservations.is_empty());
    assert!(!page.truncated, "empty history sets no marker");
    db.close().await.unwrap();
}

// --- funnel ---------------------------------------------------------------

const FGUILD: u64 = 111;
const FMEMBER: u64 = 222;
const JOIN_READBACK: &str = "2026-08-15T00:00:00.000000Z";

async fn seed_event(pool: &sqlx::PgPool, event_type: &str, second: usize, key: &str) {
    sqlx::query(
        "INSERT INTO events (event_type, member_id, guild_id, occurred_at, source, idempotency_key)
         VALUES ($1, $2, $3, $4::text::timestamptz, 'acceptance', $5)",
    )
    .bind(event_type)
    .bind(FMEMBER.to_string())
    .bind(FGUILD.to_string())
    .bind(format!("2026-08-15T00:00:{second:02}Z"))
    .bind(key)
    .execute(pool)
    .await
    .unwrap();
}

fn funnel_store(db: &TestDatabase) -> PgFunnelStore {
    PgFunnelStore::new(db.pool().clone())
}

#[tokio::test(flavor = "multi_thread")]
async fn member_page_marks_truncated_over_cap() {
    let Some(bootstrap) = bootstrap_url("funnel page cap") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    seed_event(db.pool(), "member_join", 0, "hotpath-mfun-join").await;
    for i in 1..OVER_CAP {
        seed_event(db.pool(), "invite_click", i, &format!("hotpath-mfun-{i}")).await;
    }
    let page = funnel_store(&db)
        .membership_paged(FGUILD, FMEMBER)
        .expect("a joined member pages");
    assert!(page.truncated, "30 rows over a 25-row page set the marker");
    assert_eq!(page.membership.joined_at.as_deref(), Some(JOIN_READBACK));
    assert_eq!(page.membership.left_at, None);
    db.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn member_page_unmarked_within_cap() {
    let Some(bootstrap) = bootstrap_url("funnel page within cap") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    seed_event(db.pool(), "member_join", 0, "hotpath-mfun-small-join").await;
    for i in 1..=5 {
        seed_event(
            db.pool(),
            "invite_click",
            i,
            &format!("hotpath-mfun-small-{i}"),
        )
        .await;
    }
    let page = funnel_store(&db)
        .membership_paged(FGUILD, FMEMBER)
        .expect("a joined member pages");
    assert!(
        !page.truncated,
        "6 rows fit the page: no marker, got truncated"
    );
    assert_eq!(page.membership.joined_at.as_deref(), Some(JOIN_READBACK));
    db.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn member_page_unknown_member_is_none() {
    let Some(bootstrap) = bootstrap_url("funnel page unknown") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    assert!(
        funnel_store(&db)
            .membership_paged(FGUILD, FMEMBER)
            .is_none(),
        "no rows page to no membership"
    );
    db.close().await.unwrap();
}
