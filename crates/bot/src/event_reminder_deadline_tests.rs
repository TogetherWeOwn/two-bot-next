//! Exercise the registered job through its real supervisor and clock. A late
//! RSVP can first become eligible less than one ordinary cadence before start;
//! preserving two attempts per tick must not strand the third event until then.

#![cfg(test)]

#[path = "event_reminder_deadline_review_tests.rs"]
mod review_gaps;

use std::{process::Command, sync::Arc, time::Duration};

use serde_json::json;
use tokio::sync::watch;
use two_bot_core::{
    rsvp::{RsvpRecord, RsvpStatus},
    rsvp_store::put_rsvp,
    scheduled::format_iso_ms,
    scheduled_events::{EventStatus, ScheduledEvent},
    website_store::replace_events,
};
use two_bot_testsupport::TestDatabase;

use super::super::*;
use crate::{
    activation::fixtures,
    discord_test_common::{MockRest, RestRequest, ScriptedResponse},
};

#[test]
fn urgent_reminder_tail_gets_supervised_follow_up_before_start() {
    // Registration reads the real feature gate. Isolate that environment from
    // sibling tests rather than mutating process-global configuration.
    const CHILD: &str = "TWO_TEST_REMINDER_DEADLINE_CHILD";
    if std::env::var(CHILD).as_deref() != Ok("1") {
        let module = module_path!().split_once("::").expect("test module").1;
        let target =
            format!("{module}::urgent_reminder_tail_gets_supervised_follow_up_before_start");
        let output = Command::new(std::env::current_exe().expect("test executable"))
            .args([&target, "--exact", "--nocapture"])
            .env(CHILD, "1")
            .env("TWO_AUTOMATIONS", "1")
            .output()
            .expect("spawn isolated reminder regression");
        let text = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            text.contains("running 1 test"),
            "must collect regression: {text}"
        );
        assert!(
            output.status.success(),
            "supervised deadline regression failed: {text}"
        );
        return;
    }
    tokio::runtime::Runtime::new()
        .expect("test runtime")
        .block_on(urgent_tail());
}

async fn fixture() -> Option<(String, TestDatabase)> {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI must supply the guarded disposable database"
        );
        eprintln!("SKIP reminder deadline regression: TWO_TEST_DATABASE_URL is not set");
        return None;
    };
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("migrated disposable database");
    Some((url, db))
}

async fn urgent_tail() {
    let Some((url, db)) = fixture().await else {
        return;
    };
    let mock = MockRest::with_responder(|_: &RestRequest| {
        ScriptedResponse::json(200, json!({"id": "9001"}))
    })
    .await;
    crate::gateway::ensure_crypto_provider();
    let rest = ActionExecutor::with_proxy(
        "synthetic-reminder-deadline-token".to_owned(),
        Some(mock.origin()),
    )
    .expect("loopback executor");
    let guild = "3333";
    let ids = [
        "1546451670500642999",
        "1546451670500643001",
        "1546451670500643002",
    ];
    // Map the event's wall-clock start to the mock's monotonic arrival clock.
    // The audit timestamp is a pre-wire fence, not evidence of timely delivery.
    let wire_epoch = tokio::time::Instant::now();
    let now = now_ms();
    let starts = now + 10_000;
    let wire_deadline = wire_epoch + Duration::from_millis(starts - now);
    let mut events: Vec<_> = ids
        .map(|id| ScheduledEvent {
            id: id.to_owned(),
            name: "Late eligibility".to_owned(),
            starts_at: format_iso_ms(starts),
            channel_id: Some("1546451670500642111".to_owned()),
            description: None,
            status: EventStatus::Scheduled,
        })
        .into();
    let later_id = "1546451670500643003";
    let mut later = events[0].clone();
    later.id = later_id.to_owned();
    later.starts_at = format_iso_ms(now + 60_000);
    events.push(later);
    replace_events(db.pool(), guild, &format_iso_ms(now), &events)
        .await
        .expect("seed event mirror");
    // Earlier ticks had no attendees. These first RSVPs are the reason the
    // 15-minute lead window cannot rescue a batch tail on the next 15 s tick.
    for id in ids.into_iter().chain([later_id]) {
        put_rsvp(
            db.pool(),
            &RsvpRecord {
                guild_id: guild.to_owned(),
                event_id: id.to_owned(),
                user_id: "1546451670500642888".to_owned(),
                status: RsvpStatus::Going,
                responded_at: format_iso_ms(now),
            },
        )
        .await
        .expect("late Going RSVP");
    }
    let context = Arc::new(Context::for_tests(
        url,
        db.pool().clone(),
        rest,
        guild.to_owned(),
    ));
    let mut job =
        register(context.clone(), &fixtures::staging()).expect("activation permits scheduler");
    assert_eq!(job.cadence, Duration::from_secs(15));
    job.startup_jitter = Duration::ZERO;
    let status = jobs::statuses(&NAMES, false);
    let (shutdown, stop) = watch::channel(false);
    let supervisor = tokio::spawn(jobs::supervise(vec![job], status.clone(), stop));
    let delivered = tokio::time::timeout(Duration::from_secs(9), async {
        loop {
            if mock
                .requests()
                .iter()
                .filter(|request| request.method == "POST")
                .count()
                == 3
            {
                // Wait for the final outcome to commit too, not just the wire.
                let count: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM automation_audit_log
                     WHERE guild_id = $1 AND action = 'rsvp.reminder' AND outcome = 'posted'",
                )
                .bind(guild)
                .fetch_one(db.pool())
                .await
                .expect("read reminder outcomes");
                // A committed result can become visible before the action
                // returns. Observe supervisor completion before shutdown, so
                // this success-path test cannot abort that final return.
                if count == 3 && status.read().await[NAMES[0]].last_success.is_some() {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    shutdown.send(true).expect("stop supervisor");
    supervisor
        .await
        .expect("supervisor task")
        .expect("registered job");
    let attempts: Vec<(String, i64)> = sqlx::query_as(
        "SELECT target_key, (extract(epoch FROM created_at) * 1000)::bigint
         FROM automation_audit_log WHERE guild_id = $1 AND action = 'rsvp.reminder_attempt'",
    )
    .bind(guild)
    .fetch_all(db.pool())
    .await
    .expect("read immutable attempt facts");
    assert!(
        attempts.iter().all(|(id, _)| ids.contains(&id.as_str())),
        "the extra pass must leave a nonurgent fourth event for the ordinary tick"
    );
    let fences_before_restart = reminder_fences(db.pool()).await;
    if delivered.is_ok() {
        // All three remain future AND urgent after restart. Neither expiration
        // nor the follow-up's urgency filter can explain replay refusal.
        let restart_now = now_ms();
        let rescheduled: Vec<_> = events[..3]
            .iter()
            .cloned()
            .map(|mut event| {
                event.starts_at = format_iso_ms(restart_now + 10_000);
                event
            })
            .collect();
        replace_events(db.pool(), guild, &format_iso_ms(restart_now), &rescheduled)
            .await
            .expect("reschedule without clearing replay fences");
        let peer = db.independent_pool().await.expect("fresh restart pool");
        let restarted = Arc::new(Context::for_tests(
            context.url.clone(),
            peer.clone(),
            ActionExecutor::with_proxy(
                "synthetic-reminder-deadline-token".to_owned(),
                Some(mock.origin()),
            )
            .expect("fresh loopback executor"),
            guild.to_owned(),
        ));
        let replay = register(restarted, &fixtures::staging()).expect("new job after restart");
        assert!(
            now_ms() < restart_now + 10_000,
            "restart control must still be future and within the urgent window"
        );
        (replay.action)()
            .await
            .expect("retained fences refuse replay");
        peer.close().await;
    }
    let fences_after_restart = reminder_fences(db.pool()).await;
    let requests = mock.requests();
    let successful = status.read().await[NAMES[0]].last_success.is_some();
    mock.shutdown().await;
    db.close().await.expect("verified teardown");
    assert!(
        delivered.is_ok(),
        "third event must be reminded before its start, not on the next 15 s tick"
    );
    assert_eq!(attempts.len(), 3, "one immutable attempt per event");
    assert!(attempts.iter().all(|(_, at)| *at < starts as i64));
    assert_eq!(fences_before_restart.len(), 3);
    assert_eq!(
        fences_after_restart, fences_before_restart,
        "restart must retain each complete immutable fence, including its identity"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "POST")
            .count(),
        3
    );
    for id in ids {
        let link = format!("https://discord.com/events/{guild}/{id}");
        let arrivals: Vec<_> = requests
            .iter()
            .filter(|request| {
                request.method == "POST" && String::from_utf8_lossy(&request.body).contains(&link)
            })
            .collect();
        assert_eq!(arrivals.len(), 1, "one actual reminder arrival for {id}");
        assert!(
            arrivals[0].received_at < wire_deadline,
            "reminder for {id} must reach Discord before that event's start"
        );
    }
    assert!(successful, "follow-up remains a supervised healthy attempt");
}

async fn reminder_fences(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT to_jsonb(a)::text FROM automation_audit_log a
         WHERE guild_id = $1 AND action = 'rsvp.reminder_attempt' ORDER BY id",
    )
    .bind(GUILD)
    .fetch_all(pool)
    .await
    .expect("read complete immutable reminder fences")
}

const GUILD: &str = "3333";
const IDS: [&str; 5] = [
    "1546451670500642999",
    "1546451670500643001",
    "1546451670500643002",
    "1546451670500643003",
    "1546451670500643004",
];

async fn seed_urgent(pool: &PgPool, now: u64, count: usize) {
    let events: Vec<_> = IDS[..count]
        .iter()
        .map(|id| ScheduledEvent {
            id: (*id).to_owned(),
            name: "Urgent bounded backlog".to_owned(),
            starts_at: format_iso_ms(now + 10_000),
            channel_id: Some("1546451670500642111".to_owned()),
            description: None,
            status: EventStatus::Scheduled,
        })
        .collect();
    replace_events(pool, GUILD, &format_iso_ms(now), &events)
        .await
        .expect("seed urgent mirror");
    for id in &IDS[..count] {
        put_rsvp(
            pool,
            &RsvpRecord {
                guild_id: GUILD.to_owned(),
                event_id: (*id).to_owned(),
                user_id: "1546451670500642888".to_owned(),
                status: RsvpStatus::Going,
                responded_at: format_iso_ms(now),
            },
        )
        .await
        .expect("seed urgent RSVP");
    }
}

fn executor(mock: &MockRest) -> ActionExecutor {
    crate::gateway::ensure_crypto_provider();
    ActionExecutor::with_proxy(
        "synthetic-reminder-deadline-token".to_owned(),
        Some(mock.origin()),
    )
    .expect("loopback executor")
}

fn post_count(mock: &MockRest) -> usize {
    mock.requests()
        .iter()
        .filter(|request| request.method == "POST")
        .count()
}

#[tokio::test]
async fn urgent_follow_up_is_one_bounded_batch_not_a_drain_loop() {
    let Some((_, db)) = fixture().await else {
        return;
    };
    let now = now_ms();
    seed_urgent(db.pool(), now, 5).await;
    let mock = MockRest::with_responder(|_: &RestRequest| {
        ScriptedResponse::json(200, json!({"id": "9001"}))
    })
    .await;
    tick_with_deadline_follow_up(db.pool(), &executor(&mock), GUILD, || now)
        .await
        .expect("bounded urgent follow-up");
    assert_eq!(
        post_count(&mock),
        4,
        "two ordinary plus two urgent attempts"
    );
    let remaining = two_bot_core::event_reminder_store::due_event_reminders(db.pool(), GUILD, now)
        .await
        .expect("read unattempted tail");
    assert_eq!(remaining.len(), 1, "an oversized burst stays best effort");
    assert_eq!(remaining[0].event_id, IDS[4]);
    mock.shutdown().await;
    db.close().await.expect("verified teardown");
}

#[tokio::test]
async fn supervisor_cancels_an_in_flight_urgent_follow_up_without_replay() {
    let Some((_, db)) = fixture().await else {
        return;
    };
    let now = now_ms();
    seed_urgent(db.pool(), now, 3).await;
    let posted = ScriptedResponse::json(200, json!({"id": "9001"}));
    let (mock, gate) = MockRest::start_gated(vec![posted.clone(); 3], posted).await;
    let rest = Arc::new(executor(&mock));
    let action: JobAction = Arc::new({
        let pool = db.pool().clone();
        let rest = rest.clone();
        move || {
            let pool = pool.clone();
            let rest = rest.clone();
            Box::pin(async move { tick_with_deadline_follow_up(&pool, &rest, GUILD, || now).await })
        }
    });
    let mut registered = job(action);
    registered.startup_jitter = Duration::ZERO;
    let status = jobs::statuses(&NAMES, false);
    let (shutdown, stop) = watch::channel(false);
    let supervisor = tokio::spawn(jobs::supervise(vec![registered], status.clone(), stop));
    tokio::time::timeout(Duration::from_secs(10), gate.wait_for_request())
        .await
        .expect("third POST is the gated urgent follow-up");
    assert_eq!(post_count(&mock), 3);
    let pending: Vec<(String, String)> = sqlx::query_as(
        "SELECT action, outcome FROM automation_audit_log
         WHERE guild_id = $1 AND target_key = $2 ORDER BY action",
    )
    .bind(GUILD)
    .bind(IDS[2])
    .fetch_all(db.pool())
    .await
    .expect("observe committed follow-up fence before response");
    assert_eq!(
        pending,
        [("rsvp.reminder_attempt".to_owned(), "unknown".to_owned())]
    );
    shutdown.send(true).expect("cancel owning supervisor");
    tokio::time::timeout(Duration::from_secs(2), supervisor)
        .await
        .expect("no detached follow-up outlives cancellation")
        .expect("supervisor task")
        .expect("registered job");
    assert!(!status.read().await[NAMES[0]].running);
    let peer = db.independent_pool().await.expect("fresh restart pool");
    tick_with_deadline_follow_up(&peer, &executor(&mock), GUILD, || now)
        .await
        .expect("unknown follow-up attempt refuses replay");
    assert_eq!(
        post_count(&mock),
        3,
        "a fresh executor cannot retry the third event"
    );
    gate.release();
    tokio::time::timeout(Duration::from_secs(10), gate.wait_for_completion())
        .await
        .expect("held response handler finishes");
    let results: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM automation_audit_log
         WHERE guild_id = $1 AND target_key = $2 AND action = 'rsvp.reminder'",
    )
    .bind(GUILD)
    .bind(IDS[2])
    .fetch_one(&peer)
    .await
    .expect("read cancelled follow-up outcomes");
    assert_eq!(
        results, 0,
        "cancellation must not fabricate a success receipt"
    );
    peer.close().await;
    mock.shutdown().await;
    db.close().await.expect("verified teardown");
}
