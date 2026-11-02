//! Acceptance: RSVP compensation is fenced to the exact attempted write
//! and cleans up atomically.
//!
//! [`compensate_rsvp_write`] currently guards the response cleanup on
//! `responded_at` alone and runs the response and audit deletes as separate
//! autocommits. Two gaps follow: same-millisecond distinct writes are
//! indistinguishable to the older compensation, and a midway database
//! failure splits the cleanup. These tests pin the intended contract — a
//! later same-millisecond response (including an identical status rewrite)
//! survives older compensation, the owned response and its audit go away in
//! one transaction under the same fence (a forced second-statement failure
//! leaves both pre-cleanup rows intact), and exact-owned first-write and
//! rewrite compensations keep working. Failure seams are disposable-DB
//! triggers only; no production seam or schema is added here.
//!
//! Protected acceptance file: the paired builder consumes this head without
//! editing it.
#![cfg(feature = "db")]

use sqlx::{Pool, Postgres};
use two_bot_core::{
    compensate_rsvp_write, list_rsvps, put_rsvp, write_audit, RsvpAudit, RsvpRecord, RsvpStatus,
};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "1545644954272137297";
const EVENT: &str = "1546451670500642999";

async fn fixture() -> Option<TestDatabase> {
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => {
            eprintln!("skipped database acceptance: TWO_TEST_DATABASE_URL not set");
            return None;
        }
        Err(_) => panic!("test URL must be Unicode"),
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create guarded disposable test database"),
    )
}

fn record(user: &str, status: RsvpStatus, at: &str) -> RsvpRecord {
    RsvpRecord {
        guild_id: GUILD.to_owned(),
        event_id: EVENT.to_owned(),
        user_id: user.to_owned(),
        status,
        responded_at: at.to_owned(),
    }
}

async fn write(pool: &Pool<Postgres>, audit_id: &str, attempt: &RsvpRecord) {
    put_rsvp(pool, attempt)
        .await
        .expect("setup RSVP write is admitted");
    assert!(
        write_audit(pool, &RsvpAudit::for_rsvp(audit_id, attempt))
            .await
            .expect("setup audit ledger"),
        "setup audit id must be fresh"
    );
}

async fn audit_ids(pool: &Pool<Postgres>) -> Vec<String> {
    sqlx::query_scalar("SELECT id FROM announcements_audit_log ORDER BY id")
        .fetch_all(pool)
        .await
        .expect("audit ids")
}

async fn install_audit_delete_fault(pool: &Pool<Postgres>) {
    sqlx::query(
        "CREATE FUNCTION injected_audit_delete_fault() RETURNS trigger LANGUAGE plpgsql AS
         $$ BEGIN RAISE EXCEPTION 'injected audit-delete fault'; END $$",
    )
    .execute(pool)
    .await
    .expect("fault function");
    sqlx::query(
        "CREATE TRIGGER fault_audit_delete BEFORE DELETE ON announcements_audit_log
         FOR EACH ROW EXECUTE FUNCTION injected_audit_delete_fault()",
    )
    .execute(pool)
    .await
    .expect("fault trigger");
}

async fn remove_audit_delete_fault(pool: &Pool<Postgres>) {
    sqlx::query("DROP TRIGGER fault_audit_delete ON announcements_audit_log")
        .execute(pool)
        .await
        .expect("drop fault trigger");
    sqlx::query("DROP FUNCTION injected_audit_delete_fault()")
        .execute(pool)
        .await
        .expect("drop fault function");
}

/// A later same-millisecond response survives the older attempt's
/// compensation: attempt A writes `going` at T, attempt B rewrites to
/// `interested` at the same T, and compensating A removes only A's audit.
/// RED while compensation matches on `responded_at` alone.
#[tokio::test]
async fn older_compensation_preserves_later_same_millisecond_response() {
    let Some(db) = fixture().await else {
        return;
    };
    let user = "fence-distinct-status";
    let at = "2026-09-10T10:00:00.000Z";
    write(db.pool(), "fence-a", &record(user, RsvpStatus::Going, at)).await;
    write(
        db.pool(),
        "fence-b",
        &record(user, RsvpStatus::Interested, at),
    )
    .await;
    compensate_rsvp_write(db.pool(), GUILD, EVENT, user, at, "fence-a", None)
        .await
        .expect("compensation of the older attempt runs");
    let rows = list_rsvps(db.pool(), GUILD, EVENT)
        .await
        .expect("response rows");
    assert_eq!(rows.len(), 1, "the later response survives");
    assert_eq!(rows[0].status, RsvpStatus::Interested);
    assert_eq!(rows[0].responded_at, at);
    assert_eq!(
        audit_ids(db.pool()).await,
        ["fence-b".to_owned()],
        "only the older attempt's audit is removed"
    );
    db.close().await.expect("drop test database");
}

/// The identical-status case: A writes `going` at T and B rewrites the same
/// `going` at the same T, so status plus timestamp cannot tell the attempts
/// apart — only the write identity can. Compensating A still preserves B.
/// RED while compensation matches on `responded_at` alone.
#[tokio::test]
async fn older_compensation_preserves_identical_same_millisecond_rewrite() {
    let Some(db) = fixture().await else {
        return;
    };
    let user = "fence-same-status";
    let at = "2026-09-10T10:00:00.000Z";
    write(
        db.pool(),
        "fence-same-a",
        &record(user, RsvpStatus::Going, at),
    )
    .await;
    write(
        db.pool(),
        "fence-same-b",
        &record(user, RsvpStatus::Going, at),
    )
    .await;
    compensate_rsvp_write(db.pool(), GUILD, EVENT, user, at, "fence-same-a", None)
        .await
        .expect("compensation of the older attempt runs");
    let rows = list_rsvps(db.pool(), GUILD, EVENT)
        .await
        .expect("response rows");
    assert_eq!(
        rows.len(),
        1,
        "the identical rewrite survives older compensation"
    );
    assert_eq!(rows[0].status, RsvpStatus::Going);
    assert_eq!(rows[0].responded_at, at);
    assert_eq!(
        audit_ids(db.pool()).await,
        ["fence-same-b".to_owned()],
        "only the older attempt's audit is removed"
    );
    db.close().await.expect("drop test database");
}

/// Positive control: compensating an exact owned first write (no later
/// write) still removes both rows. Guards against an over-fenced
/// compensation that never deletes. GREEN before and after the fix.
#[tokio::test]
async fn compensation_removes_exactly_the_owned_first_write() {
    let Some(db) = fixture().await else {
        return;
    };
    let user = "fence-owned-first";
    let at = "2026-09-10T10:00:00.000Z";
    write(
        db.pool(),
        "fence-owned-a",
        &record(user, RsvpStatus::Going, at),
    )
    .await;
    compensate_rsvp_write(db.pool(), GUILD, EVENT, user, at, "fence-owned-a", None)
        .await
        .expect("owned compensation runs");
    assert!(
        list_rsvps(db.pool(), GUILD, EVENT)
            .await
            .expect("response rows")
            .is_empty(),
        "the owned first write is removed"
    );
    assert!(
        audit_ids(db.pool()).await.is_empty(),
        "the owned audit row is removed"
    );
    db.close().await.expect("drop test database");
}

/// Positive control: compensating an exact owned rewrite restores the
/// member's prior row and removes only the refused attempt's audit. GREEN
/// before and after the fix.
#[tokio::test]
async fn compensation_restores_exact_previous_response_on_owned_rewrite() {
    let Some(db) = fixture().await else {
        return;
    };
    let user = "fence-owned-rewrite";
    let first_at = "2026-09-10T10:00:00.000Z";
    let second_at = "2026-09-10T10:01:00.000Z";
    write(
        db.pool(),
        "fence-owned-rw-a",
        &record(user, RsvpStatus::Going, first_at),
    )
    .await;
    write(
        db.pool(),
        "fence-owned-rw-b",
        &record(user, RsvpStatus::Interested, second_at),
    )
    .await;
    compensate_rsvp_write(
        db.pool(),
        GUILD,
        EVENT,
        user,
        second_at,
        "fence-owned-rw-b",
        Some((RsvpStatus::Going, first_at)),
    )
    .await
    .expect("owned rewrite compensation runs");
    let rows = list_rsvps(db.pool(), GUILD, EVENT)
        .await
        .expect("response rows");
    assert_eq!(rows.len(), 1, "the prior response is restored");
    assert_eq!(rows[0].status, RsvpStatus::Going);
    assert_eq!(rows[0].responded_at, first_at);
    assert_eq!(
        audit_ids(db.pool()).await,
        ["fence-owned-rw-a".to_owned()],
        "only the refused attempt's audit is removed"
    );
    db.close().await.expect("drop test database");
}

/// Atomicity, first-write path: with the audit delete forced to fail, the
/// response cleanup rolls back too — both pre-cleanup rows stay intact.
/// Removing the fault makes the same compensation succeed. RED while the two
/// deletes commit separately.
#[tokio::test]
async fn audit_delete_failure_rolls_back_first_write_compensation() {
    let Some(db) = fixture().await else {
        return;
    };
    let user = "fence-atomic-first";
    let at = "2026-09-10T10:00:00.000Z";
    write(
        db.pool(),
        "fence-atomic-a",
        &record(user, RsvpStatus::Going, at),
    )
    .await;
    install_audit_delete_fault(db.pool()).await;
    assert!(
        compensate_rsvp_write(db.pool(), GUILD, EVENT, user, at, "fence-atomic-a", None)
            .await
            .is_err(),
        "the forced audit-delete failure surfaces"
    );
    let rows = list_rsvps(db.pool(), GUILD, EVENT)
        .await
        .expect("response rows");
    assert_eq!(rows.len(), 1, "the response row survives the split");
    assert_eq!(rows[0].status, RsvpStatus::Going);
    assert_eq!(rows[0].responded_at, at);
    assert_eq!(
        audit_ids(db.pool()).await,
        ["fence-atomic-a".to_owned()],
        "the audit row survives the split"
    );

    // Success control: without the fault the same compensation removes both.
    remove_audit_delete_fault(db.pool()).await;
    compensate_rsvp_write(db.pool(), GUILD, EVENT, user, at, "fence-atomic-a", None)
        .await
        .expect("unfaulted compensation runs");
    assert!(list_rsvps(db.pool(), GUILD, EVENT)
        .await
        .expect("response rows")
        .is_empty());
    assert!(audit_ids(db.pool()).await.is_empty());
    db.close().await.expect("drop test database");
}

/// Atomicity, rewrite path: with the audit delete forced to fail, the prior
/// response is NOT restored and both audits stay — the attempted rewrite and
/// its history remain exactly as before the call. RED while the restore
/// commits separately from the audit delete.
#[tokio::test]
async fn audit_delete_failure_rolls_back_rewrite_compensation() {
    let Some(db) = fixture().await else {
        return;
    };
    let user = "fence-atomic-rewrite";
    let first_at = "2026-09-10T10:00:00.000Z";
    let second_at = "2026-09-10T10:01:00.000Z";
    write(
        db.pool(),
        "fence-atomic-rw-a",
        &record(user, RsvpStatus::Going, first_at),
    )
    .await;
    write(
        db.pool(),
        "fence-atomic-rw-b",
        &record(user, RsvpStatus::Interested, second_at),
    )
    .await;
    install_audit_delete_fault(db.pool()).await;
    assert!(
        compensate_rsvp_write(
            db.pool(),
            GUILD,
            EVENT,
            user,
            second_at,
            "fence-atomic-rw-b",
            Some((RsvpStatus::Going, first_at)),
        )
        .await
        .is_err(),
        "the forced audit-delete failure surfaces"
    );
    let rows = list_rsvps(db.pool(), GUILD, EVENT)
        .await
        .expect("response rows");
    assert_eq!(rows.len(), 1, "exactly one response row remains");
    assert_eq!(rows[0].status, RsvpStatus::Interested);
    assert_eq!(
        rows[0].responded_at, second_at,
        "the attempted rewrite is untouched, not restored"
    );
    assert_eq!(
        audit_ids(db.pool()).await,
        [
            "fence-atomic-rw-a".to_owned(),
            "fence-atomic-rw-b".to_owned()
        ],
        "both audits survive the split"
    );

    // Success control: without the fault the rewrite restores and its audit
    // goes away.
    remove_audit_delete_fault(db.pool()).await;
    compensate_rsvp_write(
        db.pool(),
        GUILD,
        EVENT,
        user,
        second_at,
        "fence-atomic-rw-b",
        Some((RsvpStatus::Going, first_at)),
    )
    .await
    .expect("unfaulted compensation runs");
    let rows = list_rsvps(db.pool(), GUILD, EVENT)
        .await
        .expect("response rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, RsvpStatus::Going);
    assert_eq!(rows[0].responded_at, first_at);
    assert_eq!(audit_ids(db.pool()).await, ["fence-atomic-rw-a".to_owned()]);
    db.close().await.expect("drop test database");
}
