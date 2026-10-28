//! Exercise the registered job through its real supervisor and clock. A late
//! RSVP can first become eligible less than one ordinary cadence before start;
//! preserving two attempts per tick must not strand the third event until then.

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
    const CHILD: &str = "TWO_REMINDER_DEADLINE_TEST_CHILD";
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

async fn urgent_tail() {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI must supply the guarded disposable database"
        );
        eprintln!("SKIP reminder deadline regression: TWO_TEST_DATABASE_URL is not set");
        return;
    };
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("migrated disposable database");
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
    let now = now_ms();
    let starts = now + 10_000;
    let events = ids.map(|id| ScheduledEvent {
        id: id.to_owned(),
        name: "Late eligibility".to_owned(),
        starts_at: format_iso_ms(starts),
        channel_id: Some("4444".to_owned()),
        description: None,
        status: EventStatus::Scheduled,
    });
    replace_events(db.pool(), guild, &format_iso_ms(now), &events)
        .await
        .expect("seed event mirror");
    // Earlier ticks had no attendees. These first RSVPs are the reason the
    // 15-minute lead window cannot rescue a batch tail on the next 15 s tick.
    for id in ids {
        put_rsvp(
            db.pool(),
            &RsvpRecord {
                guild_id: guild.to_owned(),
                event_id: id.to_owned(),
                user_id: "5555".to_owned(),
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
    let mut job = register(context, &fixtures::staging()).expect("activation permits scheduler");
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
                if count == 3 {
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
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "POST")
            .count(),
        3
    );
    assert!(successful, "follow-up remains a supervised healthy attempt");
}
