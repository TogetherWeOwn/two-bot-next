//! Acceptance, behavioral half: hot-path read bounds proven through existing
//! APIs only, so this target compiles on the unmodified tree (hot-path
//! bounds item B1: settings, retention, burst, funnel).
//!
//! Protected test-first handoff: the paired Builder bounds the five reads
//! without editing this file. Truncation markers live in the sibling
//! `hotpath_bounds_marker_acceptance` target, which is compile-RED until the
//! new surface exists; this file pins what must hold through the APIs that
//! exist today:
//!
//! - Retention DryRun/Execute complete at most 25 pending deletions per run
//!   over cap and drain across runs; counts from `COUNT(*)` stay exact; the
//!   request state machine (pending -> completed + receipt) is unchanged.
//! - Burst verdicts and their exact reason codes are unchanged, including
//!   over cap: a capped history must still count exactly, never flip a
//!   `user_burst` into a `guild_burst`.
//! - The settings revision gate (`poll_marks` + `needs_refresh`), the scoped
//!   `get`, and the whole-table supervisor snapshot keep working; the bound
//!   adds a scoped read, it never caps the supervisor path.
//! - Funnel membership projection is unchanged, including over cap.
//!
//! RED status on the unmodified tree: `dry_run_reports_only_the_capped_subset`
//! and `execute_drains_across_passes_over_cap` fail behaviorally (one run
//! completes all 30 seeded deletions, want 25 then 5). Every other test in
//! this file passes before and after the change.
//!
//! Database tests use the authorized disposable test database only
//! (`TWO_TEST_DATABASE_URL`, `agent-testdb` principal): they skip with a
//! diagnostic when the URL is absent locally and fail closed in CI.

use serde_json::json;
use two_bot_core::membership::MembershipStore;
use two_bot_core::settings::SettingsCache;
use two_bot_core::voice_create_admission::{CreateAdmissionConfig, RefusalReason};
use two_bot_cutover::retention::{request_user_deletion, run_retention, RetentionMode};
use two_bot_cutover::settings::SettingsStore;
use two_bot_cutover::voice_rooms::{CreateClaim, PgRoomStore};
use two_bot_store::PgFunnelStore;
use two_bot_testsupport::TestDatabase;

const PENDING_CAP: u64 = 25;
const OVER_CAP: usize = 30;

// --- shared fixture -------------------------------------------------------

fn bootstrap_url(test: &str) -> Option<String> {
    match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure hotpath-bounds acceptance"
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
    .bind(format!("hotpath-del-{user}"))
    .execute(pool)
    .await
    .expect("seed event row");
    request_user_deletion(pool, RGUILD, user)
        .await
        .expect("request queues");
}

#[tokio::test]
async fn dry_run_reports_only_the_capped_subset() {
    let Some(bootstrap) = bootstrap_url("retention dry run cap") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    for i in 0..OVER_CAP {
        seed_deletion_request(db.pool(), &retention_user(i)).await;
    }
    let report = run_retention(db.pool(), RetentionMode::DryRun)
        .await
        .expect("dry run reports");
    assert_eq!(
        report.deletions_completed, PENDING_CAP,
        "a capped dry run previews exactly 25 completions, got {}",
        report.deletions_completed
    );
    assert_eq!(
        report.deletions_deferred, 0,
        "unguarded requests are never deferred"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn execute_drains_across_passes_over_cap() {
    let Some(bootstrap) = bootstrap_url("retention execute drain") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    for i in 0..OVER_CAP {
        seed_deletion_request(db.pool(), &retention_user(i)).await;
    }
    let first = run_retention(db.pool(), RetentionMode::Execute)
        .await
        .expect("first drain runs");
    assert_eq!(
        first.deletions_completed, PENDING_CAP,
        "one execute pass completes 25, got {}",
        first.deletions_completed
    );
    let second = run_retention(db.pool(), RetentionMode::Execute)
        .await
        .expect("second drain runs");
    assert_eq!(
        second.deletions_completed,
        OVER_CAP as u64 - PENDING_CAP,
        "the next pass completes the remaining 5"
    );
    let third = run_retention(db.pool(), RetentionMode::Execute)
        .await
        .expect("third drain runs");
    assert_eq!(third.deletions_completed, 0, "a drained ledger is quiet");
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT status, receipt_id FROM retention_deletion_requests")
            .fetch_all(db.pool())
            .await
            .unwrap();
    assert_eq!(rows.len(), OVER_CAP, "every request settles");
    let mut receipts = std::collections::HashSet::new();
    for (status, receipt) in &rows {
        assert_eq!(status, "completed", "drained requests complete");
        assert!(
            receipts.insert(receipt.clone()),
            "receipts stay unique, duplicate {receipt}"
        );
    }
    assert_eq!(
        first.deletions_overdue, 0,
        "fresh requests are never overdue"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn dry_run_at_cap_reports_everything() {
    let Some(bootstrap) = bootstrap_url("retention dry run at cap") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    for i in 0..PENDING_CAP as usize {
        seed_deletion_request(db.pool(), &retention_user(i)).await;
    }
    let report = run_retention(db.pool(), RetentionMode::DryRun)
        .await
        .expect("dry run reports");
    assert_eq!(report.deletions_completed, PENDING_CAP);
    assert_eq!(report.deletions_deferred, 0);
    db.close().await.unwrap();
}

#[tokio::test]
async fn empty_ledger_reports_zero() {
    let Some(bootstrap) = bootstrap_url("retention empty ledger") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    let report = run_retention(db.pool(), RetentionMode::DryRun)
        .await
        .expect("dry run reports");
    assert_eq!(report.deletions_completed, 0);
    assert_eq!(report.deletions_deferred, 0);
    assert_eq!(report.deletions_overdue, 0);
    db.close().await.unwrap();
}

#[tokio::test]
async fn overdue_signal_stays_exact() {
    let Some(bootstrap) = bootstrap_url("retention overdue exact") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    for i in 0..3 {
        seed_deletion_request(db.pool(), &retention_user(i)).await;
    }
    sqlx::query(
        "UPDATE retention_deletion_requests
         SET requested_at = now() - make_interval(hours => 80)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let report = run_retention(db.pool(), RetentionMode::DryRun)
        .await
        .expect("dry run reports");
    assert_eq!(
        report.deletions_overdue, 3,
        "the overdue COUNT(*) stays exact under the row cap"
    );
    assert_eq!(report.deletions_completed, 3);
    db.close().await.unwrap();
}

#[tokio::test]
async fn execute_at_cap_completes_in_one_pass() {
    let Some(bootstrap) = bootstrap_url("retention execute at cap") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    for i in 0..PENDING_CAP as usize {
        seed_deletion_request(db.pool(), &retention_user(i)).await;
    }
    let first = run_retention(db.pool(), RetentionMode::Execute)
        .await
        .expect("drain runs");
    assert_eq!(first.deletions_completed, PENDING_CAP);
    let second = run_retention(db.pool(), RetentionMode::Execute)
        .await
        .expect("second drain runs");
    assert_eq!(second.deletions_completed, 0);
    db.close().await.unwrap();
}

// --- burst ----------------------------------------------------------------

const BGUILD: u64 = 100;
const BT0: i64 = 1_800_000_000;

fn open_config() -> CreateAdmissionConfig {
    CreateAdmissionConfig::new(10, 45, 0).unwrap()
}

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

async fn refused_reason(
    store: &PgRoomStore,
    user: u64,
    config: &CreateAdmissionConfig,
    now: i64,
) -> RefusalReason {
    match store.claim_create(BGUILD, user, config, now).await {
        Ok(CreateClaim::Refused(reason)) => reason,
        Ok(CreateClaim::Admitted { .. }) => panic!("user {user} was admitted, want a refusal"),
        Err(error) => panic!("claim failed: {error}"),
    }
}

#[tokio::test]
async fn empty_history_admits() {
    let Some(bootstrap) = bootstrap_url("burst empty history") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    let store = PgRoomStore::new(db.pool().clone());
    let open = open_config();
    match store.claim_create(BGUILD, 700, &open, BT0).await {
        Ok(CreateClaim::Admitted { reservation_id }) => {
            assert!(!reservation_id.is_empty(), "admission records a claim");
        }
        other => panic!("empty history admits, got {other:?}"),
    }
    db.close().await.unwrap();
}

#[tokio::test]
async fn exact_guild_threshold_refuses() {
    let Some(bootstrap) = bootstrap_url("burst guild threshold") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    let store = PgRoomStore::new(db.pool().clone());
    let open = open_config();
    for i in 0..10 {
        seed_reservation(db.pool(), 800 + i, BT0 + i as i64).await;
    }
    assert_eq!(
        refused_reason(&store, 899, &open, BT0 + 30).await,
        RefusalReason::GuildBurst,
        "10 in-window creates trip the guild burst"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn user_burst_refuses_on_third_prior_create() {
    let Some(bootstrap) = bootstrap_url("burst user threshold") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    let store = PgRoomStore::new(db.pool().clone());
    let open = open_config();
    for offset in 0..3 {
        match store.claim_create(BGUILD, 750, &open, BT0 + offset).await {
            Ok(CreateClaim::Admitted { .. }) => {}
            other => panic!("prior create {offset} admits, got {other:?}"),
        }
    }
    assert_eq!(
        refused_reason(&store, 750, &open, BT0 + 3).await,
        RefusalReason::UserBurst,
        "the fourth in-window create trips the user burst"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn user_burst_reason_survives_guild_wide_history_over_cap() {
    let Some(bootstrap) = bootstrap_url("burst reason over cap") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    let store = PgRoomStore::new(db.pool().clone());
    let open = open_config();
    for offset in 0..3 {
        match store.claim_create(BGUILD, 760, &open, BT0 + offset).await {
            Ok(CreateClaim::Admitted { .. }) => {}
            other => panic!("prior create {offset} admits, got {other:?}"),
        }
    }
    // The requester's three creates are the OLDEST in-window rows: a naive
    // newest-25 sample would drop them and flip this verdict to GuildBurst.
    // The capped history must still count exactly.
    for i in 0..27 {
        seed_reservation(db.pool(), 900 + i, BT0 + 3 + i as i64).await;
    }
    assert_eq!(
        refused_reason(&store, 760, &open, BT0 + 30).await,
        RefusalReason::UserBurst,
        "exact per-user counting survives the history cap"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn guild_burst_refuses_over_cap() {
    let Some(bootstrap) = bootstrap_url("burst guild over cap") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    let store = PgRoomStore::new(db.pool().clone());
    let open = open_config();
    for i in 0..OVER_CAP {
        seed_reservation(db.pool(), 950 + i as u64, BT0 + i as i64).await;
    }
    assert_eq!(
        refused_reason(&store, 999, &open, BT0 + 30).await,
        RefusalReason::GuildBurst,
        "30 in-window creates still trip the guild burst"
    );
    db.close().await.unwrap();
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

#[tokio::test]
async fn poll_marks_track_revision_and_count() {
    let Some(bootstrap) = bootstrap_url("settings poll marks") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    let store = SettingsStore::new(db.pool());
    let (rev0, count0) = store.poll_marks().await.expect("poll marks read");
    assert_eq!(count0, 0, "fresh database holds no settings rows");
    store
        .set(SGUILD, SKEY, Some(json!(3)), SACTOR)
        .await
        .expect("write stores");
    let (rev1, count1) = store.poll_marks().await.expect("poll marks reread");
    assert_eq!(count1, 1, "the count sees the write");
    assert!(
        rev1 > rev0,
        "the revision advances on every write ({rev0} -> {rev1})"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn scoped_get_reads_only_the_target_guild() {
    let Some(bootstrap) = bootstrap_url("settings scoped get") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    let store = SettingsStore::new(db.pool());
    store
        .set(SGUILD, SKEY, Some(json!(3)), SACTOR)
        .await
        .expect("target write stores");
    store
        .set(&other_guild(0), SKEY, Some(json!(9)), SACTOR)
        .await
        .expect("other-guild write stores");
    let target = store.get(SGUILD, SKEY).await.expect("scoped get reads");
    assert_eq!(target.map(|(v, _)| v), Some(json!(3)));
    let missing = store.get(SGUILD, SKEY2).await.expect("absent key reads");
    assert_eq!(missing, None, "an unset key reads back absent");
    db.close().await.unwrap();
}

#[tokio::test]
async fn refresh_gate_stays_shut_without_changes() {
    let Some(bootstrap) = bootstrap_url("settings refresh gate") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    let store = SettingsStore::new(db.pool());
    store
        .set(SGUILD, SKEY, Some(json!(3)), SACTOR)
        .await
        .expect("write stores");
    let snapshot = store.load_snapshot().await.expect("snapshot loads");
    let cache = SettingsCache::load(&snapshot);
    let (rev, count) = store.poll_marks().await.expect("poll marks read");
    assert!(
        !cache.needs_refresh(rev, count),
        "a fresh cache needs no refresh"
    );
    store
        .set(SGUILD, SKEY2, Some(json!(30)), SACTOR)
        .await
        .expect("second write stores");
    let (rev2, count2) = store.poll_marks().await.expect("poll marks reread");
    assert!(
        cache.needs_refresh(rev2, count2),
        "the gate opens when the table moves"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn whole_table_snapshot_stays_intact_for_the_supervisor_poll() {
    let Some(bootstrap) = bootstrap_url("settings supervisor snapshot") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    let store = SettingsStore::new(db.pool());
    for i in 0..OVER_CAP {
        store
            .set(&other_guild(i), SKEY, Some(json!(i as i64)), SACTOR)
            .await
            .expect("other-guild write stores");
    }
    for (key, value) in [(SKEY, 3), (SKEY2, 30), (SKEY3, 5)] {
        store
            .set(SGUILD, key, Some(json!(value)), SACTOR)
            .await
            .expect("target write stores");
    }
    let snapshot = store.load_snapshot().await.expect("snapshot loads");
    assert_eq!(
        snapshot.rows.len(),
        OVER_CAP + 3,
        "the supervisor poll keeps seeing every row"
    );
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
async fn small_history_projects_exact_membership() {
    let Some(bootstrap) = bootstrap_url("funnel small history") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    seed_event(db.pool(), "member_join", 0, "hotpath-funnel-join").await;
    for i in 1..=5 {
        seed_event(
            db.pool(),
            "invite_click",
            i,
            &format!("hotpath-funnel-pad-{i}"),
        )
        .await;
    }
    let membership = funnel_store(&db).membership(FGUILD, FMEMBER);
    let membership = membership.expect("a joined member projects");
    assert_eq!(membership.joined_at.as_deref(), Some(JOIN_READBACK));
    assert_eq!(membership.join_source.as_deref(), Some("acceptance"));
    assert_eq!(membership.left_at, None);
    assert_eq!(membership.inactive_flagged_at, None);
    db.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_member_has_no_membership() {
    let Some(bootstrap) = bootstrap_url("funnel unknown member") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    assert_eq!(
        funnel_store(&db).membership(FGUILD, FMEMBER),
        None,
        "no rows project to no membership"
    );
    db.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn over_cap_history_still_projects_exactly() {
    let Some(bootstrap) = bootstrap_url("funnel over cap") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    seed_event(db.pool(), "member_join", 0, "hotpath-funnel-big-join").await;
    for i in 1..OVER_CAP {
        seed_event(
            db.pool(),
            "invite_click",
            i,
            &format!("hotpath-funnel-big-{i}"),
        )
        .await;
    }
    let membership = funnel_store(&db).membership(FGUILD, FMEMBER);
    let membership = membership.expect("a joined member projects over cap");
    assert_eq!(
        membership.joined_at.as_deref(),
        Some(JOIN_READBACK),
        "the projection stays exact past the cap"
    );
    assert_eq!(membership.left_at, None);
    db.close().await.unwrap();
}
