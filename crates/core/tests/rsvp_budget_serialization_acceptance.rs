//! Acceptance: one user's RSVP minute budget serializes across events.
//!
//! The per-user write budget is ledgered by `(guild, user)` in
//! `announcements_audit_log`, but writers lock `(guild, event)` then
//! `(guild, event, user)`: two concurrent writes for one user on distinct
//! events never meet on a lock, so a single remaining budget slot can admit
//! both. The first test races that final slot across two event IDs (exactly
//! one write must win); the others pin the isolation the fix must preserve
//! (independent users still progress, a mixed user/event matrix completes
//! without deadlock, and refusals still precede any row write). The race
//! window is widened with a disposable-DB `BEFORE INSERT` sleep on
//! `event_rsvps` only — no production seam — so the overlap is deterministic
//! rather than timing luck. The builder owns the explicit consistent lock
//! order and domain-separated keys; these tests pin the observable behavior.
//!
//! Protected acceptance file: the paired builder consumes this head without
//! editing it. Uses the live atomic path [`put_rsvp_with_audit`], the same
//! call the `/rsvp` handler makes.
#![cfg(feature = "db")]

use sqlx::{Pool, Postgres};
use two_bot_core::{
    list_rsvps, put_rsvp_with_audit, write_audit, RsvpAudit, RsvpRecord, RsvpStatus,
    RsvpStoreError, MAX_RSVP_WRITES_PER_USER_PER_MINUTE,
};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "1545644954272137297";
const EVENT_A: &str = "1546451670500642999";
const EVENT_B: &str = "1546451670500643000";
const EVENT_C: &str = "1546451670500643001";
const EVENT_FRESH: &str = "1546451670500643002";
const USER: &str = "1546451670500642888";

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

fn record(event: &str, user: &str, status: RsvpStatus, at: &str) -> RsvpRecord {
    RsvpRecord {
        guild_id: GUILD.to_owned(),
        event_id: event.to_owned(),
        user_id: user.to_owned(),
        status,
        responded_at: at.to_owned(),
    }
}

/// Ledger `count` audit rows for `user` inside the `10:00` UTC minute without
/// touching any response row, leaving `20 - count` budget slots.
async fn prefill_minute(pool: &Pool<Postgres>, user: &str, count: i64, tag: &str) {
    for i in 0..count {
        let at = format!("2026-09-10T10:00:{i:02}.000Z");
        let seed = record(EVENT_A, user, RsvpStatus::Going, &at);
        let audit = RsvpAudit::for_rsvp(&format!("{tag}-{user}-{i}"), &seed);
        assert!(
            write_audit(pool, &audit).await.expect("prefill ledger"),
            "prefill audit id must be fresh"
        );
    }
}

async fn minute_ledger(pool: &Pool<Postgres>, user: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM announcements_audit_log
         WHERE guild_id = $1 AND actor_id = $2 AND action = 'event.rsvp'
           AND created_at >= '2026-09-10T10:00:00.000Z'::timestamptz
           AND created_at < '2026-09-10T10:01:00.000Z'::timestamptz",
    )
    .bind(GUILD)
    .bind(user)
    .fetch_one(pool)
    .await
    .expect("ledger count")
}

/// Widen the race window: every `event_rsvps` insert sleeps a second inside
/// the writer's transaction, so two writers spawned together are guaranteed
/// to overlap their budget reads. A serialized fix still admits exactly one —
/// the loser queues on the user lock, then reads the committed ledger — so
/// the sleep cannot turn a correct fix red.
async fn install_race_widening(pool: &Pool<Postgres>) {
    sqlx::query(
        "CREATE FUNCTION widen_rsvp_race() RETURNS trigger LANGUAGE plpgsql AS
         $$ BEGIN PERFORM pg_sleep(1.0); RETURN NEW; END $$",
    )
    .execute(pool)
    .await
    .expect("widening function");
    sqlx::query(
        "CREATE TRIGGER widen_rsvp_race_insert BEFORE INSERT ON event_rsvps
         FOR EACH ROW EXECUTE FUNCTION widen_rsvp_race()",
    )
    .execute(pool)
    .await
    .expect("widening trigger");
}

/// One remaining per-user slot, raced across two event IDs, admits exactly
/// one write: the loser is refused with `RsvpRateLimited` before any row
/// write, and the minute ledger holds exactly the full budget — never one
/// over. RED while writers serialize per event instead of per user.
#[tokio::test]
async fn one_remaining_budget_slot_raced_across_two_events_admits_exactly_one() {
    let Some(db) = fixture().await else {
        return;
    };
    assert_eq!(
        MAX_RSVP_WRITES_PER_USER_PER_MINUTE, 20,
        "budget pin: the race below prefills all but one slot"
    );
    prefill_minute(
        db.pool(),
        USER,
        MAX_RSVP_WRITES_PER_USER_PER_MINUTE - 1,
        "race",
    )
    .await;
    install_race_widening(db.pool()).await;

    let at = "2026-09-10T10:00:30.000Z".to_owned();
    let run = |event: &'static str, status: RsvpStatus, audit_id: &'static str| {
        let pool = db.pool().clone();
        let at = at.clone();
        tokio::spawn(async move {
            let attempt = record(event, USER, status, &at);
            let audit = RsvpAudit::for_rsvp(audit_id, &attempt);
            put_rsvp_with_audit(&pool, &attempt, &audit).await
        })
    };
    let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        tokio::join!(
            run(EVENT_A, RsvpStatus::Going, "race-winner-a"),
            run(EVENT_B, RsvpStatus::Interested, "race-winner-b")
        )
    })
    .await
    .expect("racers finish without deadlock");
    let (first, second) = (first.expect("racer task"), second.expect("racer task"));

    let wins = u8::from(first.is_ok()) + u8::from(second.is_ok());
    assert_eq!(
        wins,
        1,
        "exactly one racer wins the final slot: [{}, {}]",
        first.is_ok(),
        second.is_ok()
    );
    for outcome in [&first, &second] {
        match outcome {
            Ok(_) => {}
            Err(err) => assert!(
                matches!(err, RsvpStoreError::RsvpRateLimited),
                "the loser is refused on the rate budget, got: {err:?}"
            ),
        }
    }
    assert_eq!(
        minute_ledger(db.pool(), USER).await,
        MAX_RSVP_WRITES_PER_USER_PER_MINUTE,
        "the ledger holds exactly the full budget, never one over"
    );
    let rows_a = list_rsvps(db.pool(), GUILD, EVENT_A)
        .await
        .expect("event A rows");
    let rows_b = list_rsvps(db.pool(), GUILD, EVENT_B)
        .await
        .expect("event B rows");
    assert_eq!(
        rows_a.len() + rows_b.len(),
        1,
        "exactly one response row exists across both events"
    );
    db.close().await.expect("drop test database");
}

/// Isolation: independent users still progress concurrently — one user's
/// nearly-spent budget never blocks another, even on the same event behind
/// the shared event lock. GREEN control: must pass before and after the fix.
#[tokio::test]
async fn independent_users_progress_concurrently_under_shared_budget_pressure() {
    let Some(db) = fixture().await else {
        return;
    };
    let users = [
        "budget-user-1",
        "budget-user-2",
        "budget-user-3",
        "budget-user-4",
    ];
    for user in users {
        prefill_minute(
            db.pool(),
            user,
            MAX_RSVP_WRITES_PER_USER_PER_MINUTE - 1,
            "iso",
        )
        .await;
    }
    let at = "2026-09-10T10:00:30.000Z".to_owned();
    // Two writers share EVENT_A behind its event lock; the other two are on
    // disjoint events. All four carry distinct user budgets.
    let lanes = [
        ("budget-user-1", EVENT_A, RsvpStatus::Going, "iso-w-1"),
        ("budget-user-2", EVENT_A, RsvpStatus::Interested, "iso-w-2"),
        ("budget-user-3", EVENT_B, RsvpStatus::Declined, "iso-w-3"),
        ("budget-user-4", EVENT_C, RsvpStatus::Going, "iso-w-4"),
    ];
    let mut tasks = Vec::new();
    for (user, event, status, audit_id) in lanes {
        let pool = db.pool().clone();
        let at = at.clone();
        tasks.push(tokio::spawn(async move {
            let attempt = record(event, user, status, &at);
            let audit = RsvpAudit::for_rsvp(audit_id, &attempt);
            (user, put_rsvp_with_audit(&pool, &attempt, &audit).await)
        }));
    }
    let mut rows = 0;
    for task in tasks {
        let (user, outcome) = tokio::time::timeout(std::time::Duration::from_secs(30), task)
            .await
            .expect("writer finishes without deadlock")
            .expect("writer task");
        assert!(outcome.is_ok(), "independent user {user} is admitted");
        assert_eq!(
            minute_ledger(db.pool(), user).await,
            MAX_RSVP_WRITES_PER_USER_PER_MINUTE,
            "user {user} consumed exactly their final slot"
        );
        rows += 1;
    }
    assert_eq!(rows, 4, "all four independent writes land");
    db.close().await.expect("drop test database");
}

/// Lock-order/deadlock probe plus refusal ordering: a mixed user/event
/// matrix completes inside a bounded wait, an over-budget write is refused
/// before consuming event capacity, and a different user can still take that
/// capacity. GREEN control on the current baseline.
#[tokio::test]
async fn mixed_matrix_completes_and_refusals_precede_capacity() {
    let Some(db) = fixture().await else {
        return;
    };
    // Four users by two events, all budgets fresh: every order must complete.
    let at = "2026-09-10T10:00:30.000Z".to_owned();
    let mut tasks = Vec::new();
    for (n, user) in ["matrix-u-1", "matrix-u-2", "matrix-u-3", "matrix-u-4"]
        .into_iter()
        .enumerate()
    {
        for event in [EVENT_A, EVENT_B] {
            let pool = db.pool().clone();
            let at = at.clone();
            let audit_id = format!("matrix-{n}-{event}");
            tasks.push(tokio::spawn(async move {
                let attempt = record(event, user, RsvpStatus::Going, &at);
                let audit = RsvpAudit::for_rsvp(&audit_id, &attempt);
                put_rsvp_with_audit(&pool, &attempt, &audit).await
            }));
        }
    }
    for task in tasks {
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), task)
            .await
            .expect("matrix write finishes without deadlock")
            .expect("matrix task");
        assert!(outcome.is_ok(), "fresh-budget matrix write is admitted");
    }

    // A fully-spent user is refused before any row write, and the refusal
    // consumes no event capacity: a different user lands on that event.
    for i in 0..MAX_RSVP_WRITES_PER_USER_PER_MINUTE {
        let at = format!("2026-09-10T11:00:{i:02}.000Z");
        let seed = record(EVENT_FRESH, "spent-user", RsvpStatus::Going, &at);
        write_audit(
            db.pool(),
            &RsvpAudit::for_rsvp(&format!("spent-{i}"), &seed),
        )
        .await
        .expect("spent-user ledger");
    }
    let refused = record(
        EVENT_FRESH,
        "spent-user",
        RsvpStatus::Going,
        "2026-09-10T11:00:30.000Z",
    );
    let audit = RsvpAudit::for_rsvp("spent-final", &refused);
    assert!(
        matches!(
            put_rsvp_with_audit(db.pool(), &refused, &audit).await,
            Err(RsvpStoreError::RsvpRateLimited)
        ),
        "a spent budget refuses on the rate gate"
    );
    assert!(
        list_rsvps(db.pool(), GUILD, EVENT_FRESH)
            .await
            .expect("fresh event rows")
            .is_empty(),
        "the refused write stored no response row"
    );
    let other = record(
        EVENT_FRESH,
        "fresh-user",
        RsvpStatus::Going,
        "2026-09-10T11:00:31.000Z",
    );
    let other_audit = RsvpAudit::for_rsvp("fresh-lands", &other);
    assert!(
        put_rsvp_with_audit(db.pool(), &other, &other_audit)
            .await
            .is_ok(),
        "another user still lands on the event after the refusal"
    );
    db.close().await.expect("drop test database");
}
