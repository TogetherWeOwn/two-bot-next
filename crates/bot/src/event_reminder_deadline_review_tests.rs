//! Independent acceptance for deadline validation after admission waits and
//! urgent candidate filtering before attendee materialization. Only disposable
//! database synchronization and the Discord wire boundary are instrumented.

#![cfg(test)]

use std::time::Duration;

use serde_json::json;
use sqlx::PgPool;
use two_bot_core::{
    event_reminder_store::{begin_event_reminder_delivery, due_event_reminders},
    rsvp::{RsvpRecord, RsvpStatus},
    rsvp_store::put_rsvp,
    scheduled::format_iso_ms,
    scheduled_events::{EventStatus, ScheduledEvent},
    website_store::replace_events,
};

use super::{executor, fixture, post_count, reminder_fences, GUILD, IDS};
use crate::{
    discord_test_common::{MockRest, RestRequest, ScriptedResponse},
    scheduled_jobs::{now_ms, tick, tick_with_deadline_follow_up},
};

async fn seed_deadlines(pool: &PgPool, now: u64, starts: &[u64]) -> Vec<ScheduledEvent> {
    let events: Vec<_> = IDS
        .iter()
        .zip(starts)
        .map(|(id, starts)| ScheduledEvent {
            id: (*id).to_owned(),
            name: "Deadline admission fixture".to_owned(),
            starts_at: format_iso_ms(*starts),
            channel_id: Some("1546451670500642111".to_owned()),
            description: None,
            status: EventStatus::Scheduled,
        })
        .collect();
    replace_events(pool, GUILD, &format_iso_ms(now), &events)
        .await
        .expect("seed deadline mirror");
    for event in &events {
        put_rsvp(
            pool,
            &RsvpRecord {
                guild_id: GUILD.to_owned(),
                event_id: event.id.clone(),
                user_id: "1546451670500642888".to_owned(),
                status: RsvpStatus::Going,
                responded_at: format_iso_ms(now),
            },
        )
        .await
        .expect("seed deadline attendee");
    }
    events
}

async fn blocked_pid(pool: &PgPool, blocker: i32) -> i32 {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let pid: Option<i32> = sqlx::query_scalar(
                "SELECT pid FROM pg_stat_activity
                 WHERE datname = current_database() AND wait_event_type = 'Lock'
                   AND wait_event = 'advisory' AND $1 = ANY(pg_blocking_pids(pid))
                 ORDER BY pid LIMIT 1",
            )
            .bind(blocker)
            .fetch_optional(pool)
            .await
            .expect("observe actual admission lock waiter");
            if let Some(pid) = pid {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("admission must reach the held lock before the deadline")
}

// One direct reservation holds the real guild admission lock while its INSERT
// waits on our disposable trigger. The ticker must then wait on THAT backend,
// not merely on an unrelated synchronization lock. Releasing after event start
// catches a timestamp sampled before begin_event_reminder_delivery's awaits.
#[tokio::test]
async fn guild_admission_released_after_event_start_never_posts_late() {
    let Some((_, db)) = fixture().await else {
        return;
    };
    let mock = MockRest::with_responder(|_: &RestRequest| {
        ScriptedResponse::json(200, json!({"id": "9001"}))
    })
    .await;
    let rest = executor(&mock);
    sqlx::raw_sql(
        "CREATE FUNCTION hold_deadline_admission() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN PERFORM pg_advisory_xact_lock(672314895::bigint); RETURN NEW; END $$;
         CREATE TRIGGER hold_deadline_admission BEFORE INSERT ON automation_audit_log
         FOR EACH ROW WHEN (NEW.action = 'rsvp.reminder_attempt'
           AND NEW.guild_id = '3333' AND NEW.target_key = '1546451670500643001')
         EXECUTE FUNCTION hold_deadline_admission();",
    )
    .execute(db.pool())
    .await
    .expect("install disposable admission barrier");
    let mut gate = db.pool().begin().await.expect("barrier transaction");
    sqlx::query("SELECT pg_advisory_xact_lock(672314895::bigint)")
        .execute(&mut *gate)
        .await
        .expect("hold admission barrier");
    let gate_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *gate)
        .await
        .expect("barrier backend identity");
    let now = now_ms();
    let starts = now + 4_000;
    let events = seed_deadlines(db.pool(), now, &[starts, now + 60_000]).await;
    let holder = due_event_reminders(db.pool(), GUILD, now)
        .await
        .expect("select direct reservation control")
        .into_iter()
        .find(|candidate| candidate.event_id == IDS[1])
        .expect("nonurgent holder is ordinarily eligible");

    // All futures are joined in this scope; no spawned worker survives timeout.
    let (admitted, (result, (waited_before_start, released_at))) =
        tokio::time::timeout(Duration::from_secs(12), async {
            tokio::join!(
                begin_event_reminder_delivery(db.pool(), GUILD, &holder, now),
                async {
                    let holder_pid = blocked_pid(db.pool(), gate_pid).await;
                    tokio::join!(
                        tick_with_deadline_follow_up(db.pool(), &rest, GUILD, now_ms),
                        async {
                            let _ticker_pid = blocked_pid(db.pool(), holder_pid).await;
                            let waited_before_start = now_ms() < starts;
                            let delay = (starts + 25).saturating_sub(now_ms());
                            tokio::time::sleep(Duration::from_millis(delay)).await;
                            let released_at = now_ms();
                            gate.commit().await.expect("release expired admission");
                            (waited_before_start, released_at)
                        }
                    )
                }
            )
        })
        .await
        .expect("bounded admission wait and ticker completion");
    assert!(admitted.expect("direct reservation control"));
    result.expect("deadline refusal is a healthy bounded tick");
    let late_posts = post_count(&mock);
    let fences_before_restart = reminder_fences(db.pool()).await;
    let posted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM automation_audit_log
         WHERE guild_id = $1 AND target_key = $2
           AND action = 'rsvp.reminder' AND outcome = 'posted'",
    )
    .bind(GUILD)
    .bind(IDS[0])
    .fetch_one(db.pool())
    .await
    .expect("read expired candidate outcomes");

    // The direct reservation is a committed pre-wire fence. Make it urgent on
    // restart; fail-closed retention, not nonurgency or expiry, must stop replay.
    let restart_now = now_ms();
    let mut retained = events[1].clone();
    retained.starts_at = format_iso_ms(restart_now + 10_000);
    replace_events(db.pool(), GUILD, &format_iso_ms(restart_now), &[retained])
        .await
        .expect("retain admitted holder with a future urgent start");
    let peer = db.independent_pool().await.expect("fresh restart pool");
    let replayed_while_urgent = now_ms() < restart_now + 10_000;
    tick_with_deadline_follow_up(&peer, &executor(&mock), GUILD, now_ms)
        .await
        .expect("committed admission refuses restart replay");
    let posts_after_restart = post_count(&mock);
    let fences_after_restart = reminder_fences(&peer).await;
    peer.close().await;
    mock.shutdown().await;
    db.close().await.expect("verified teardown");

    assert!(
        waited_before_start,
        "ticker must block while the event is future"
    );
    assert!(
        released_at >= starts,
        "admission releases only after event start"
    );
    assert_eq!(
        late_posts, 0,
        "a stale pre-admission timestamp must not permit a late POST"
    );
    assert_eq!(
        posted, 0,
        "an expired candidate cannot record a posted result"
    );
    assert!(
        !fences_before_restart.is_empty(),
        "direct reservation committed its fence"
    );
    assert!(
        replayed_while_urgent,
        "restart control must remain future and urgent"
    );
    assert_eq!(
        posts_after_restart, late_posts,
        "restart must not retry a committed fence"
    );
    assert_eq!(
        fences_after_restart, fences_before_restart,
        "complete fence identities stay immutable"
    );
}

async fn roster_reads(pool: &PgPool, event: &str, ordinary_batch_complete: bool) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM reminder_roster_reads
         WHERE event_id = $1 AND ordinary_batch_complete = $2",
    )
    .bind(event)
    .bind(ordinary_batch_complete)
    .fetch_one(pool)
    .await
    .expect("read attendee materialization witness")
}

// A user-id projection records actual roster materialization, not just POSTs.
// Tag reads using the two committed ordinary outcomes: PostgreSQL may evaluate
// projections before an outer LIMIT, which must not be blamed on the follow-up.
// A later ordinary read must observe the nonurgent roster in that same phase,
// proving both the projection and phase tag are live rather than inert.
#[tokio::test]
async fn urgent_follow_up_filters_nonurgent_before_materializing_rosters() {
    let Some((_, db)) = fixture().await else {
        return;
    };
    let now = now_ms();
    seed_deadlines(db.pool(), now, &[now + 10_000, now + 10_000, now + 60_000]).await;
    sqlx::raw_sql(
        "ALTER TABLE event_rsvps RENAME TO deadline_fixture_rsvps;
         CREATE TABLE reminder_roster_reads (
           event_id text NOT NULL, ordinary_batch_complete boolean NOT NULL
         );
         CREATE FUNCTION observe_reminder_roster(observed_event text, observed_user text)
         RETURNS text LANGUAGE plpgsql VOLATILE AS $$
         BEGIN
           INSERT INTO reminder_roster_reads(event_id, ordinary_batch_complete)
             SELECT observed_event, count(*) >= 2 FROM automation_audit_log
             WHERE guild_id = '3333' AND action = 'rsvp.reminder' AND outcome = 'posted';
           RETURN observed_user;
         END $$;
         CREATE VIEW event_rsvps AS
           SELECT guild_id, event_id,
             observe_reminder_roster(event_id, user_id) AS user_id, status, responded_at
           FROM deadline_fixture_rsvps;",
    )
    .execute(db.pool())
    .await
    .expect("install disposable attendee read witness");
    let ordinary = due_event_reminders(db.pool(), GUILD, now)
        .await
        .expect("ordinary bounded observer control");
    let head_reads =
        roster_reads(db.pool(), IDS[0], false).await + roster_reads(db.pool(), IDS[1], false).await;
    let tail_before = roster_reads(db.pool(), IDS[2], true).await;
    let mock = MockRest::with_responder(|_: &RestRequest| {
        ScriptedResponse::json(200, json!({"id": "9001"}))
    })
    .await;
    let rest = executor(&mock);
    tick_with_deadline_follow_up(db.pool(), &rest, GUILD, || now)
        .await
        .expect("ordinary delivery plus one urgent scan");
    let tail_after_follow_up = roster_reads(db.pool(), IDS[2], true).await;
    let ordinary_posts = post_count(&mock);
    let ordinary_fences = reminder_fences(db.pool()).await;
    let ordinary_results: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM automation_audit_log
         WHERE guild_id = $1 AND action = 'rsvp.reminder' AND outcome = 'posted'",
    )
    .bind(GUILD)
    .fetch_one(db.pool())
    .await
    .expect("the two ordinary results activate the follow-up observer phase");

    let next = due_event_reminders(db.pool(), GUILD, now)
        .await
        .expect("positive observer control for the remaining nonurgent event");
    let tail_after_positive_control = roster_reads(db.pool(), IDS[2], true).await;
    tick(db.pool(), &rest, GUILD, || now)
        .await
        .expect("next ordinary invocation may deliver the nonurgent event");
    let posts_after_ordinary_tick = post_count(&mock);
    mock.shutdown().await;
    db.close().await.expect("verified teardown");

    assert_eq!(
        ordinary
            .iter()
            .map(|row| row.event_id.as_str())
            .collect::<Vec<_>>(),
        &IDS[..2]
    );
    assert!(
        head_reads > 0,
        "ordinary control must exercise the roster observer"
    );
    assert_eq!(
        tail_before, 0,
        "ordinary reads cannot be tagged as follow-up work"
    );
    assert_eq!(
        ordinary_results, 2,
        "both ordinary results must be committed before follow-up"
    );
    assert_eq!(
        ordinary_posts, 2,
        "the ordinary two-event batch still progresses"
    );
    assert_eq!(
        ordinary_fences.len(),
        2,
        "the nonurgent tail has not been admitted"
    );
    assert_eq!(
        tail_after_follow_up, 0,
        "urgent candidates must be filtered in the store before the nonurgent roster is read"
    );
    assert_eq!(
        next.iter()
            .map(|row| row.event_id.as_str())
            .collect::<Vec<_>>(),
        [IDS[2]]
    );
    assert!(
        tail_after_positive_control > tail_after_follow_up,
        "the normal store query must still materialize the nonurgent control"
    );
    assert_eq!(
        posts_after_ordinary_tick, 3,
        "urgency filtering must not suppress the next ordinary tick"
    );
}
