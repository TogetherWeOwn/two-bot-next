//! Store-boundary acceptance: mutate independently BETWEEN selection and claim.
//! These tests reject stale-snapshot admission, per-event-only capacity locking,
//! expiring retained facts, and off-by-one admission at the lifetime guild cap.

#![cfg(test)]

use super::*;

async fn candidate(pool: &PgPool, guild: &str) -> EventReminder {
    let mut due = due_event_reminders(pool, guild, T0)
        .await
        .expect("read due candidates");
    assert_eq!(due.len(), 1, "fixture must be selectable before mutation");
    due.remove(0)
}

async fn count_attempts(pool: &PgPool, guild: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM automation_audit_log
          WHERE guild_id = $1 AND action = 'rsvp.reminder_attempt'",
    )
    .bind(guild)
    .fetch_one(pool)
    .await
    .expect("count retained attempts")
}

// Each edit is made on an independent pool after fetching the original snapshot.
// No source-string checks: the actual store must reject the stale candidate and
// leave neither an attempt nor a result fact behind.
#[tokio::test]
async fn changed_event_or_eligible_set_is_rejected_at_direct_dispatch() {
    let Some(db) = fixture().await else { return };
    let writer = db
        .independent_pool()
        .await
        .expect("independent mirror/RSVP writer");
    let changes = [
        ("cancelled", "UPDATE scheduled_events SET status = 'cancelled' WHERE guild_id = $1 AND event_id = $2"),
        ("completed", "UPDATE scheduled_events SET status = 'completed' WHERE guild_id = $1 AND event_id = $2"),
        ("deleted", "DELETE FROM scheduled_events WHERE guild_id = $1 AND event_id = $2"),
        ("channel", "UPDATE scheduled_events SET channel_id = '1546451670500642222' WHERE guild_id = $1 AND event_id = $2"),
        ("start", "UPDATE scheduled_events SET starts_at = '2026-09-21T14:28:19.999Z' WHERE guild_id = $1 AND event_id = $2"),
        ("name", "UPDATE scheduled_events SET name = 'Changed event' WHERE guild_id = $1 AND event_id = $2"),
        ("eligible removed", "DELETE FROM event_rsvps WHERE guild_id = $1 AND event_id = $2 AND user_id = '1546451670500642888'"),
        ("eligible declined", "UPDATE event_rsvps SET status = 'declined' WHERE guild_id = $1 AND event_id = $2 AND user_id = '1546451670500642888'"),
        ("eligible replaced", "UPDATE event_rsvps SET user_id = '1546451670500642666' WHERE guild_id = $1 AND event_id = $2 AND user_id = '1546451670500642888'"),
        ("declined promoted", "UPDATE event_rsvps SET status = 'going' WHERE guild_id = $1 AND event_id = $2 AND user_id = '1546451670500642777'"),
        ("eligible added", "INSERT INTO event_rsvps (guild_id, event_id, user_id, status, responded_at) VALUES ($1, $2, '1546451670500642666', 'interested', '2026-09-21T00:00:00.000Z')"),
    ];
    for (label, statement) in changes {
        seed_one(db.pool()).await;
        rsvp(db.pool(), GUILD, E1, U2, RsvpStatus::Interested).await;
        rsvp(db.pool(), GUILD, E1, DECLINED, RsvpStatus::Declined).await;
        let selected = candidate(db.pool(), GUILD).await;
        let changed = sqlx::query(statement)
            .bind(GUILD)
            .bind(E1)
            .execute(&writer)
            .await
            .expect("independent fixture mutation");
        assert_eq!(changed.rows_affected(), 1, "mutation must apply: {label}");
        assert!(
            !begin_event_reminder_delivery(db.pool(), GUILD, &selected, T0)
                .await
                .expect("dispatch revalidation"),
            "stale candidate admitted after {label}"
        );
        assert!(
            audits(&writer, GUILD, E1).await.is_empty(),
            "rejection must leave no audit: {label}"
        );
        // Fixture RSVP reset, not deletion of retained replay evidence (none was
        // admitted). The event mirror is restored by the next seed call.
        sqlx::query("DELETE FROM event_rsvps WHERE guild_id = $1 AND event_id = $2")
            .bind(GUILD)
            .bind(E1)
            .execute(&writer)
            .await
            .expect("reset fixture RSVPs");
    }
    // Positive control proves the rejection loop wasn't a permanently false
    // stub or an empty/deleted fixture. Going -> Interested preserves eligibility.
    seed_one(db.pool()).await;
    let selected = candidate(db.pool(), GUILD).await;
    rsvp(&writer, GUILD, E1, U1, RsvpStatus::Interested).await;
    assert!(
        begin_event_reminder_delivery(db.pool(), GUILD, &selected, T0)
            .await
            .expect("unchanged eligible snapshot admitted")
    );
    assert_eq!(
        audits(&writer, GUILD, E1).await,
        [pending_attempt(GUILD, E1)]
    );
    assert!(
        !begin_event_reminder_delivery(db.pool(), GUILD, &selected, T0)
            .await
            .expect("duplicate reservation refused")
    );
    assert_eq!(
        audits(&writer, GUILD, E1).await,
        [pending_attempt(GUILD, E1)]
    );
    writer.close().await;
    db.close().await.expect("verified teardown");
}

#[tokio::test]
async fn dispatch_time_expiry_and_outside_window_are_rejected_without_audit() {
    let Some(db) = fixture().await else { return };
    seed_one(db.pool()).await;
    let selected = candidate(db.pool(), GUILD).await;
    for now in [T0 - 1, START, START + 1] {
        assert!(
            !begin_event_reminder_delivery(db.pool(), GUILD, &selected, now)
                .await
                .expect("dispatch-time check"),
            "outside window admitted at {now}"
        );
        assert!(audits(db.pool(), GUILD, E1).await.is_empty());
    }
    assert!(
        begin_event_reminder_delivery(db.pool(), GUILD, &selected, T0)
            .await
            .expect("unchanged in-window positive control")
    );
    assert_eq!(
        audits(db.pool(), GUILD, E1).await,
        [pending_attempt(GUILD, E1)]
    );
    db.close().await.expect("verified teardown");
}

async fn seed_retained_attempts(pool: &PgPool, guild: &str, count: i64) {
    sqlx::query(
        "INSERT INTO automation_audit_log
         (id, guild_id, actor_id, action, target_key, outcome, reason, created_at)
         SELECT 'event:reminder:' || $1 || ':retained:' || n::text, $1, NULL,
                'rsvp.reminder_attempt', 'retained:' || n::text, 'unknown',
                'delivery_pending', '2020-01-01T00:00:00Z'::timestamptz
           FROM generate_series(1::bigint, $2::bigint) n",
    )
    .bind(guild)
    .bind(count)
    .execute(pool)
    .await
    .expect("seed lifetime replay evidence");
    assert_eq!(count_attempts(pool, guild).await, count);
}

#[tokio::test]
async fn retained_lifetime_boundaries_9999_10000_10001_are_per_guild() {
    let Some(db) = fixture().await else { return };
    for (guild, retained, admitted) in [
        (GUILD, 9999, true),
        (OTHER_GUILD, 10000, false),
        ("9999", 10001, false),
    ] {
        seed_events(db.pool(), guild, &[event(E1, "Capacity")]).await;
        rsvp(db.pool(), guild, E1, U1, RsvpStatus::Going).await;
        seed_retained_attempts(db.pool(), guild, retained).await;
        // Results and other automations do not consume reminder-attempt capacity.
        sqlx::query(
            "INSERT INTO automation_audit_log (id, guild_id, actor_id, action, target_key, outcome, created_at)
             SELECT 'result:' || $1 || ':' || n::text, $1, NULL, 'rsvp.reminder', 'old', 'posted', now()
               FROM generate_series(1, 100) n",
        ).bind(guild).execute(db.pool()).await.expect("seed unrelated result facts");
        let selected = candidate(db.pool(), guild).await;
        assert_eq!(
            begin_event_reminder_delivery(db.pool(), guild, &selected, T0)
                .await
                .expect("capacity reservation"),
            admitted,
            "wrong admission at {retained} retained rows"
        );
        assert_eq!(
            count_attempts(db.pool(), guild).await,
            retained + i64::from(admitted)
        );
        let rows = audits(db.pool(), guild, E1).await;
        if admitted {
            assert_eq!(rows, [pending_attempt(guild, E1)]);
            // The 9,999 -> 10,000 transition consumes the last slot permanently.
            seed_events(
                db.pool(),
                guild,
                &[event(E1, "Capacity"), event(E2, "Next")],
            )
            .await;
            rsvp(db.pool(), guild, E2, U2, RsvpStatus::Interested).await;
            let next = candidate(db.pool(), guild).await;
            assert_eq!(next.event_id, E2);
            assert!(!begin_event_reminder_delivery(db.pool(), guild, &next, T0)
                .await
                .expect("full after last slot"));
            assert!(audits(db.pool(), guild, E2).await.is_empty());
            assert_eq!(count_attempts(db.pool(), guild).await, 10000);
        } else {
            assert!(rows.is_empty(), "full guild must leave no attempt/result");
        }
    }
    seed_events(db.pool(), "8888", &[event(E1, "Separate guild")]).await;
    rsvp(db.pool(), "8888", E1, U1, RsvpStatus::Going).await;
    let selected = candidate(db.pool(), "8888").await;
    assert!(
        begin_event_reminder_delivery(db.pool(), "8888", &selected, T0)
            .await
            .expect("another guild has its own budget")
    );
    assert_eq!(
        audits(db.pool(), "8888", E1).await,
        [pending_attempt("8888", E1)]
    );
    db.close().await.expect("verified teardown");
}

async fn wait_for_advisory_waiters(pool: &PgPool, wanted: i64) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity
                  WHERE datname = current_database() AND wait_event_type = 'Lock'
                    AND wait_event = 'advisory'",
            )
            .fetch_one(pool)
            .await
            .expect("observe synchronized database waiters");
            if waiting >= wanted {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("both reservations reached the contested admission boundary");
}

// An INSERT trigger pauses the first reservation after its capacity predicate
// was evaluated, BEFORE it commits. A second event must wait on guild admission
// (not merely an event-specific lock). Removing serialization admits both from
// the same 9,999-row snapshot and makes the exact count assertion fail.
#[tokio::test]
async fn distinct_events_competing_for_final_guild_slot_are_serialized() {
    let Some(db) = fixture().await else { return };
    seed_events(db.pool(), GUILD, &[event(E1, "First"), event(E2, "Second")]).await;
    for id in [E1, E2] {
        rsvp(db.pool(), GUILD, id, U1, RsvpStatus::Going).await;
    }
    seed_retained_attempts(db.pool(), GUILD, 9999).await;
    let mut due = due_event_reminders(db.pool(), GUILD, T0)
        .await
        .expect("two competing candidates");
    assert_eq!(due.len(), 2);
    let second = due.pop().expect("second event");
    let first = due.pop().expect("first event");
    let peer1 = db.independent_pool().await.expect("first worker");
    let peer2 = db.independent_pool().await.expect("second worker");
    sqlx::raw_sql(
        "CREATE FUNCTION hold_reminder_insert() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN PERFORM pg_advisory_xact_lock(192837465::bigint); RETURN NEW; END $$;
         CREATE TRIGGER hold_reminder_insert BEFORE INSERT ON automation_audit_log
         FOR EACH ROW WHEN (NEW.action = 'rsvp.reminder_attempt' AND NEW.guild_id = '3333')
         EXECUTE FUNCTION hold_reminder_insert();",
    )
    .execute(db.pool())
    .await
    .expect("install disposable synchronization barrier");
    let mut gate = db.pool().acquire().await.expect("held barrier connection");
    sqlx::query("SELECT pg_advisory_lock(192837465::bigint)")
        .execute(&mut *gate)
        .await
        .expect("hold insert barrier");
    let task1 = tokio::spawn({
        let pool = peer1.clone();
        async move { begin_event_reminder_delivery(&pool, GUILD, &first, T0).await }
    });
    wait_for_advisory_waiters(db.pool(), 1).await;
    let task2 = tokio::spawn({
        let pool = peer2.clone();
        async move { begin_event_reminder_delivery(&pool, GUILD, &second, T0).await }
    });
    wait_for_advisory_waiters(db.pool(), 2).await;
    assert_eq!(
        count_attempts(db.pool(), GUILD).await,
        9999,
        "neither held reservation committed"
    );
    // Another guild must proceed while BOTH first-guild workers are blocked.
    seed_events(db.pool(), OTHER_GUILD, &[event(E3, "Independent")]).await;
    rsvp(db.pool(), OTHER_GUILD, E3, U3, RsvpStatus::Going).await;
    let independent = candidate(db.pool(), OTHER_GUILD).await;
    let other = tokio::time::timeout(
        Duration::from_secs(3),
        begin_event_reminder_delivery(db.pool(), OTHER_GUILD, &independent, T0),
    )
    .await
    .expect("guild budget cannot serialize every guild")
    .expect("other guild admission");
    assert!(other);
    sqlx::query("SELECT pg_advisory_unlock(192837465::bigint)")
        .execute(&mut *gate)
        .await
        .expect("release barrier");
    drop(gate);
    let (one, two) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(task1, task2)
    })
    .await
    .expect("workers finish after release");
    let one = one.expect("first worker joined").expect("first store call");
    let two = two
        .expect("second worker joined")
        .expect("second store call");
    assert_eq!(
        [one, two].into_iter().filter(|admitted| *admitted).count(),
        1,
        "exactly one final-slot winner"
    );
    assert_eq!(
        count_attempts(db.pool(), GUILD).await,
        10000,
        "guild must never over-admit"
    );
    assert_eq!(count_attempts(db.pool(), OTHER_GUILD).await, 1);
    let event_facts = [
        audits(db.pool(), GUILD, E1).await,
        audits(db.pool(), GUILD, E2).await,
    ];
    assert_eq!(event_facts.iter().map(Vec::len).sum::<usize>(), 1);
    assert!(event_facts
        .iter()
        .flatten()
        .all(|row| row.action == "rsvp.reminder_attempt"
            && row.actor_id.is_none()
            && row.outcome == "unknown"
            && row.reason.as_deref() == Some("delivery_pending")));
    peer1.close().await;
    peer2.close().await;
    db.close().await.expect("verified teardown");
}
