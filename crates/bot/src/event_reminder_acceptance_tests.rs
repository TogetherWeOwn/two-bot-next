//! Protected acceptance: event-start reminders for RSVP attendees.
//!
//! Keep this file verbatim in the implementation handoff. Only the registration
//! in `scheduled_jobs_tests.rs` is unprotected. These tests call the live
//! 15 s scheduler tick with an injected clock; the only doubles are Discord
//! ([`MockRest`]) and the disposable database. No live Discord, no DMs.
//!
//! Contract under test (builder: `rsvp: remind attendees before event start`):
//! - The tick reminds Going + Interested RSVPs exactly once per event, in the
//!   event's channel, on the first tick at or after start −
//!   [`EXPECTED_REMINDER_LEAD_MS`] (15 min, one builder-owned constant).
//! - Cancelled / completed / past / channel-less / attendee-less events post
//!   nothing. Declined members and strangers are never named.
//! - Reminder state is durable: a restart mid-window sends nothing twice, and
//!   an uncertain delivery (accepted-then-lost reply) stays fenced — no
//!   repost storm. Definitive refusals stay bounded (at most one attempt per
//!   tick) and degrade to no-reminder.
//! - Rendering is mention-safe: per-attendee `<@id>` mentions, no mass
//!   mentions (event-controlled text is neutralized), the closed
//!   `allowed_mentions.parse == []` policy, within [`CONTENT_LIMIT`].
//!
//! RED shape (baseline): the tick ignores events entirely, so every
//! "exactly one reminder post" assertion fails with **0 posts** while the
//! scheduled-message control still posts. GREEN: the builder wires a
//! reminder scan into the tick (durable sent-markers + uncertainty fence)
//! without editing this file.

#![cfg(test)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use sqlx::PgPool;
use two_bot_core::funnel::format_iso_millis;
use two_bot_core::message_safety::{contains_mass_mention, text_len, CONTENT_LIMIT};
use two_bot_core::rsvp::{RsvpRecord, RsvpStatus};
use two_bot_core::rsvp_store::put_rsvp;
use two_bot_core::scheduled::format_iso_ms;
use two_bot_core::scheduled_events::{normalize_event, RawScheduledEvent, ScheduledEvent};
use two_bot_core::website_store::replace_events;
use two_bot_core::{put_scheduled, ScheduledWrite};
use two_bot_discord::executor::ActionExecutor;
use two_bot_testsupport::TestDatabase;

use super::super::tick;
use crate::discord_test_common::{MockRest, RestRequest, ScriptedResponse};

const GUILD: &str = "3333";
const OTHER_GUILD: &str = "7777";
const T0: u64 = 1_790_000_000_000;
const TOKEN: &str = "synthetic-reminder-test-token";

/// Behavioral pin for the builder's single lead-time constant: the first tick
/// at or after `starts_at − EXPECTED_REMINDER_LEAD_MS` reminds; the tick 1 ms
/// before it stays silent. A 14-min or 16-min implementation fails below.
const EXPECTED_REMINDER_LEAD_MS: u64 = 15 * 60 * 1000;

// Snowflake-shaped fixture ids (19 digits): the reminder path may reuse the
// RSVP snowflake validation, so fixtures stay realistic.
const E1: &str = "1546451670500642999";
const E2: &str = "1546451670500643001";
const E3: &str = "1546451670500643002";
const E4: &str = "1546451670500643003";
const E9: &str = "1546451670500643099";
const C1: &str = "1546451670500642111";
const C2: &str = "1546451670500642222";
const SCHED_CHANNEL: &str = "4444";
const U_GOING: &str = "1546451670500642888";
const U_INTERESTED: &str = "1546451670500642779";
const U_DECLINED: &str = "1546451670500642777";
const U_STRANGER: &str = "1546451670500642666";
const U_LATE: &str = "1546451670500642555";

fn raw_event(
    id: &str,
    name: &str,
    start_ms: u64,
    channel: Option<&str>,
    status: i64,
) -> RawScheduledEvent {
    RawScheduledEvent {
        id: Some(id.to_owned()),
        name: Some(name.to_owned()),
        scheduled_start_time: Some(format_iso_millis(start_ms as i64)),
        channel_id: channel.map(str::to_owned),
        description: Some("Fixture event.".to_owned()),
        status: Some(status),
    }
}

fn normalize(raw: &RawScheduledEvent) -> ScheduledEvent {
    normalize_event(raw).expect("fixture event normalizes")
}

async fn seed_events(pool: &PgPool, guild: &str, events: &[ScheduledEvent]) {
    let skip = replace_events(pool, guild, &format_iso_ms(T0), events)
        .await
        .expect("seed event mirror");
    assert!(skip.is_none(), "fixture snapshot must apply, got {skip:?}");
}

async fn rsvp_in(pool: &PgPool, guild: &str, event: &str, user: &str, status: RsvpStatus) {
    put_rsvp(
        pool,
        &RsvpRecord {
            guild_id: guild.to_owned(),
            event_id: event.to_owned(),
            user_id: user.to_owned(),
            status,
            responded_at: format_iso_ms(T0 - 3_600_000),
        },
    )
    .await
    .expect("seed rsvp");
}

async fn fixture() -> Option<TestDatabase> {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI must supply the guarded test database"
        );
        eprintln!("SKIP event reminder acceptance: TWO_TEST_DATABASE_URL is not set");
        return None;
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create migrated disposable database"),
    )
}

fn executor(mock: &MockRest) -> ActionExecutor {
    crate::gateway::ensure_crypto_provider();
    ActionExecutor::with_proxy(TOKEN.to_owned(), Some(mock.origin())).expect("loopback executor")
}

/// Every POST attempt succeeds with a unique message id.
async fn success_mock() -> MockRest {
    let seq = Arc::new(AtomicU64::new(9001));
    MockRest::with_responder(move |_: &RestRequest| {
        let id = seq.fetch_add(1, Ordering::Relaxed);
        ScriptedResponse::json(200, json!({"id": id.to_string()}))
    })
    .await
}

/// The first POST's 2xx reply is truncated (accepted-then-lost transport
/// fault); later POSTs succeed. The stack must fence, never repost.
async fn lost_first_mock() -> MockRest {
    let calls = Arc::new(AtomicU64::new(0));
    MockRest::with_responder(move |req: &RestRequest| {
        if req.method != "POST" {
            return ScriptedResponse::json(200, json!({}));
        }
        let n = calls.fetch_add(1, Ordering::Relaxed);
        if n == 0 {
            ScriptedResponse::json(200, json!({"id": "9001"})).truncated_body()
        } else {
            ScriptedResponse::json(200, json!({"id": format!("900{}", n + 1)}))
        }
    })
    .await
}

/// Every POST is definitively refused: nothing is ever delivered.
async fn refused_mock() -> MockRest {
    MockRest::with_responder(|_: &RestRequest| {
        ScriptedResponse::json(403, json!({"message": "fixture refusal"}))
    })
    .await
}

async fn bounded_tick(pool: &PgPool, rest: &ActionExecutor, guild: &str, now: u64) {
    let _ = tokio::time::timeout(Duration::from_secs(20), tick(pool, rest, guild, || now))
        .await
        .expect("bounded scheduler attempt");
}

fn channel_posts(requests: &[RestRequest], channel: &str) -> Vec<Value> {
    let path = format!("/api/v10/channels/{channel}/messages");
    requests
        .iter()
        .filter(|r| r.method == "POST" && r.path == path)
        .map(|r| serde_json::from_slice(&r.body).expect("post body JSON"))
        .collect()
}

fn all_post_bodies(requests: &[RestRequest]) -> Vec<Value> {
    requests
        .iter()
        .filter(|r| r.method == "POST")
        .map(|r| serde_json::from_slice(&r.body).expect("post body JSON"))
        .collect()
}

fn content_of(post: &Value) -> &str {
    post["content"].as_str().expect("posted content")
}

fn assert_mentions(content: &str, present: &[&str], absent: &[&str]) {
    for id in present {
        assert!(
            content.contains(&format!("<@{id}>")),
            "reminder must mention attendee {id}: {content}"
        );
    }
    for id in absent {
        assert!(
            !content.contains(id),
            "reminder must never name {id}: {content}"
        );
    }
}

fn assert_render_safe(post: &Value) {
    let content = content_of(post);
    assert!(
        !contains_mass_mention(content),
        "mass mention reached the wire: {content}"
    );
    assert!(
        text_len(content) <= CONTENT_LIMIT,
        "reminder exceeds the content limit"
    );
    assert_eq!(
        post["allowed_mentions"]["parse"],
        json!([]),
        "closed mention policy"
    );
}

/// Channel-only delivery: no DM/user routes anywhere, and every POST targets
/// one of the expected event channels.
fn assert_no_dm_routes(requests: &[RestRequest], allowed_channels: &[&str]) {
    for r in requests {
        assert!(
            !r.path.starts_with("/api/v10/users/"),
            "no DM/user routes: {} {}",
            r.method,
            r.path
        );
        if r.method == "POST" {
            let ok = allowed_channels
                .iter()
                .any(|c| r.path == format!("/api/v10/channels/{c}/messages"));
            assert!(ok, "unexpected POST route: {}", r.path);
        }
    }
}

#[tokio::test]
async fn pre_window_tick_posts_nothing() {
    let Some(db) = fixture().await else {
        return;
    };
    // T-16: a full minute before the window opens.
    seed_events(
        db.pool(),
        GUILD,
        &[normalize(&raw_event(
            E1,
            "Games night",
            T0 + 16 * 60 * 1000,
            Some(C1),
            1,
        ))],
    )
    .await;
    rsvp_in(db.pool(), GUILD, E1, U_GOING, RsvpStatus::Going).await;
    let mock = success_mock().await;
    bounded_tick(db.pool(), &executor(&mock), GUILD, T0).await;
    let requests = mock.requests();
    mock.shutdown().await;
    db.close().await.expect("verified fixture teardown");
    assert!(
        requests.is_empty(),
        "no reminder before T-15, got {} requests",
        requests.len()
    );
}

#[tokio::test]
async fn due_event_reminds_going_and_interested_once() {
    let Some(db) = fixture().await else {
        return;
    };
    // Window opens exactly at T0.
    seed_events(
        db.pool(),
        GUILD,
        &[normalize(&raw_event(
            E1,
            "Games night",
            T0 + EXPECTED_REMINDER_LEAD_MS,
            Some(C1),
            1,
        ))],
    )
    .await;
    rsvp_in(db.pool(), GUILD, E1, U_GOING, RsvpStatus::Going).await;
    rsvp_in(db.pool(), GUILD, E1, U_INTERESTED, RsvpStatus::Interested).await;
    rsvp_in(db.pool(), GUILD, E1, U_DECLINED, RsvpStatus::Declined).await;
    let mock = success_mock().await;
    let rest = executor(&mock);
    bounded_tick(db.pool(), &rest, GUILD, T0 - 1).await;
    assert!(mock.requests().is_empty(), "1 ms before T-15 stays silent");
    bounded_tick(db.pool(), &rest, GUILD, T0).await;
    let posts = channel_posts(&mock.requests(), C1);
    assert_eq!(posts.len(), 1, "one reminder at T-15, got {}", posts.len());
    let content = content_of(&posts[0]);
    assert_mentions(content, &[U_GOING, U_INTERESTED], &[U_DECLINED, U_STRANGER]);
    assert_render_safe(&posts[0]);
    assert_no_dm_routes(&mock.requests(), &[C1]);
    // Still inside the window: no second reminder.
    bounded_tick(db.pool(), &rest, GUILD, T0 + 60_000).await;
    let total = channel_posts(&mock.requests(), C1).len();
    mock.shutdown().await;
    db.close().await.expect("verified fixture teardown");
    assert_eq!(total, 1, "exactly once per event, got {total}");
}

#[tokio::test]
async fn cancelled_completed_and_past_events_get_nothing() {
    let Some(db) = fixture().await else {
        return;
    };
    seed_events(
        db.pool(),
        GUILD,
        &[
            normalize(&raw_event(
                E1,
                "Cancelled",
                T0 + 14 * 60 * 1000,
                Some(C1),
                4,
            )),
            normalize(&raw_event(
                E2,
                "Completed",
                T0 + 14 * 60 * 1000,
                Some(C2),
                3,
            )),
            normalize(&raw_event(E3, "Past", T0 - 60_000, Some(C1), 1)),
            normalize(&raw_event(E4, "Long past", T0 - 3_600_000, Some(C2), 3)),
        ],
    )
    .await;
    for event in [E1, E2, E3, E4] {
        rsvp_in(db.pool(), GUILD, event, U_GOING, RsvpStatus::Going).await;
    }
    let mock = success_mock().await;
    bounded_tick(db.pool(), &executor(&mock), GUILD, T0).await;
    let requests = mock.requests();
    mock.shutdown().await;
    db.close().await.expect("verified fixture teardown");
    assert!(
        requests.is_empty(),
        "cancelled/completed/past events get nothing, got {} requests",
        requests.len()
    );
}

#[tokio::test]
async fn declined_only_and_empty_events_post_nothing() {
    let Some(db) = fixture().await else {
        return;
    };
    seed_events(
        db.pool(),
        GUILD,
        &[
            normalize(&raw_event(
                E1,
                "No takers",
                T0 + 14 * 60 * 1000,
                Some(C1),
                1,
            )),
            normalize(&raw_event(
                E2,
                "No responses",
                T0 + 10 * 60 * 1000,
                Some(C2),
                1,
            )),
        ],
    )
    .await;
    rsvp_in(db.pool(), GUILD, E1, U_DECLINED, RsvpStatus::Declined).await;
    let mock = success_mock().await;
    bounded_tick(db.pool(), &executor(&mock), GUILD, T0).await;
    let requests = mock.requests();
    mock.shutdown().await;
    db.close().await.expect("verified fixture teardown");
    assert!(
        requests.is_empty(),
        "events with nobody to remind post nothing, got {} requests",
        requests.len()
    );
}

#[tokio::test]
async fn each_due_event_posts_to_its_own_channel() {
    let Some(db) = fixture().await else {
        return;
    };
    seed_events(
        db.pool(),
        GUILD,
        &[
            normalize(&raw_event(
                E1,
                "Games night",
                T0 + 14 * 60 * 1000,
                Some(C1),
                1,
            )),
            normalize(&raw_event(
                E2,
                "Movie night",
                T0 + 10 * 60 * 1000,
                Some(C2),
                1,
            )),
        ],
    )
    .await;
    rsvp_in(db.pool(), GUILD, E1, U_GOING, RsvpStatus::Going).await;
    rsvp_in(db.pool(), GUILD, E2, U_INTERESTED, RsvpStatus::Interested).await;
    let mock = success_mock().await;
    bounded_tick(db.pool(), &executor(&mock), GUILD, T0).await;
    let requests = mock.requests();
    let first = channel_posts(&requests, C1);
    let second = channel_posts(&requests, C2);
    assert_no_dm_routes(&requests, &[C1, C2]);
    mock.shutdown().await;
    db.close().await.expect("verified fixture teardown");
    assert_eq!(first.len(), 1, "one reminder in the first channel");
    assert_eq!(second.len(), 1, "one reminder in the second channel");
    assert_mentions(content_of(&first[0]), &[U_GOING], &[U_INTERESTED]);
    assert_mentions(content_of(&second[0]), &[U_INTERESTED], &[U_GOING]);
    assert_render_safe(&first[0]);
    assert_render_safe(&second[0]);
}

#[tokio::test]
async fn restart_mid_window_still_exactly_once() {
    let Some(db) = fixture().await else {
        return;
    };
    seed_events(
        db.pool(),
        GUILD,
        &[normalize(&raw_event(
            E1,
            "Games night",
            T0 + 14 * 60 * 1000,
            Some(C1),
            1,
        ))],
    )
    .await;
    rsvp_in(db.pool(), GUILD, E1, U_GOING, RsvpStatus::Going).await;
    let mock = success_mock().await;
    bounded_tick(db.pool(), &executor(&mock), GUILD, T0).await;
    assert_eq!(
        channel_posts(&mock.requests(), C1).len(),
        1,
        "first tick reminds"
    );
    // Both the executor (its process-local guard/cache) and DB pool are new.
    let restarted = db
        .independent_pool()
        .await
        .expect("independent restart pool");
    let rest = executor(&mock);
    bounded_tick(&restarted, &rest, GUILD, T0 + 60_000).await;
    bounded_tick(&restarted, &rest, GUILD, T0 + 120_000).await;
    let total = channel_posts(&mock.requests(), C1).len();
    mock.shutdown().await;
    restarted.close().await;
    db.close().await.expect("verified fixture teardown");
    assert_eq!(total, 1, "restart mid-window sends nothing twice");
}

#[tokio::test]
async fn transport_loss_stays_fenced_after_restart() {
    let Some(db) = fixture().await else {
        return;
    };
    seed_events(
        db.pool(),
        GUILD,
        &[normalize(&raw_event(
            E1,
            "Games night",
            T0 + 14 * 60 * 1000,
            Some(C1),
            1,
        ))],
    )
    .await;
    rsvp_in(db.pool(), GUILD, E1, U_GOING, RsvpStatus::Going).await;
    let mock = lost_first_mock().await;
    bounded_tick(db.pool(), &executor(&mock), GUILD, T0).await;
    assert_eq!(
        channel_posts(&mock.requests(), C1).len(),
        1,
        "the lost attempt still reached the wire once"
    );
    let restarted = db
        .independent_pool()
        .await
        .expect("independent restart pool");
    let rest = executor(&mock);
    bounded_tick(&restarted, &rest, GUILD, T0 + 60_000).await;
    bounded_tick(&restarted, &rest, GUILD, T0 + 120_000).await;
    let total = channel_posts(&mock.requests(), C1).len();
    let bodies = all_post_bodies(&mock.requests());
    mock.shutdown().await;
    restarted.close().await;
    db.close().await.expect("verified fixture teardown");
    assert_eq!(
        total, 1,
        "an uncertain delivery is fenced, never reposted after restart"
    );
    for post in &bodies {
        assert_render_safe(post);
    }
}

#[tokio::test]
async fn refused_delivery_attempts_stay_bounded() {
    let Some(db) = fixture().await else {
        return;
    };
    seed_events(
        db.pool(),
        GUILD,
        &[normalize(&raw_event(
            E1,
            "Games night",
            T0 + 14 * 60 * 1000,
            Some(C1),
            1,
        ))],
    )
    .await;
    rsvp_in(db.pool(), GUILD, E1, U_GOING, RsvpStatus::Going).await;
    let mock = refused_mock().await;
    let rest = executor(&mock);
    for offset in [0, 60_000, 120_000] {
        bounded_tick(db.pool(), &rest, GUILD, T0 + offset).await;
    }
    let attempts = channel_posts(&mock.requests(), C1).len();
    mock.shutdown().await;
    db.close().await.expect("verified fixture teardown");
    // Attempted (the reminder was due) but never a storm: at most one attempt
    // per tick, degrading to no-reminder. Nothing is ever delivered: every
    // reply here is a refusal.
    assert!(
        (1..=3).contains(&attempts),
        "bounded attempts across 3 ticks, got {attempts}"
    );
}

#[tokio::test]
async fn late_rsvp_does_not_trigger_a_second_reminder() {
    let Some(db) = fixture().await else {
        return;
    };
    seed_events(
        db.pool(),
        GUILD,
        &[normalize(&raw_event(
            E1,
            "Games night",
            T0 + 14 * 60 * 1000,
            Some(C1),
            1,
        ))],
    )
    .await;
    rsvp_in(db.pool(), GUILD, E1, U_GOING, RsvpStatus::Going).await;
    let mock = success_mock().await;
    let rest = executor(&mock);
    bounded_tick(db.pool(), &rest, GUILD, T0).await;
    assert_eq!(
        channel_posts(&mock.requests(), C1).len(),
        1,
        "first tick reminds"
    );
    rsvp_in(db.pool(), GUILD, E1, U_LATE, RsvpStatus::Going).await;
    bounded_tick(db.pool(), &rest, GUILD, T0 + 60_000).await;
    let total = channel_posts(&mock.requests(), C1).len();
    mock.shutdown().await;
    db.close().await.expect("verified fixture teardown");
    assert_eq!(total, 1, "exactly once per event, not per RSVP");
}

#[tokio::test]
async fn other_guild_events_are_not_reminded() {
    let Some(db) = fixture().await else {
        return;
    };
    seed_events(
        db.pool(),
        GUILD,
        &[normalize(&raw_event(
            E1,
            "Games night",
            T0 + 14 * 60 * 1000,
            Some(C1),
            1,
        ))],
    )
    .await;
    seed_events(
        db.pool(),
        OTHER_GUILD,
        &[normalize(&raw_event(
            E9,
            "Other games night",
            T0 + 14 * 60 * 1000,
            Some(C2),
            1,
        ))],
    )
    .await;
    rsvp_in(db.pool(), GUILD, E1, U_GOING, RsvpStatus::Going).await;
    rsvp_in(db.pool(), OTHER_GUILD, E9, U_INTERESTED, RsvpStatus::Going).await;
    let mock = success_mock().await;
    bounded_tick(db.pool(), &executor(&mock), GUILD, T0).await;
    let requests = mock.requests();
    let posts = channel_posts(&requests, C1);
    assert_no_dm_routes(&requests, &[C1]);
    for post in all_post_bodies(&requests) {
        assert!(
            !content_of(&post).contains(U_INTERESTED),
            "another guild's attendee is never named: {}",
            content_of(&post)
        );
    }
    mock.shutdown().await;
    db.close().await.expect("verified fixture teardown");
    assert_eq!(posts.len(), 1, "this guild's event still reminds");
    assert_mentions(content_of(&posts[0]), &[U_GOING], &[U_INTERESTED]);
}

#[tokio::test]
async fn event_name_mass_mention_is_neutralized() {
    let Some(db) = fixture().await else {
        return;
    };
    let hostile = RawScheduledEvent {
        name: Some("Launch @everyone party".to_owned()),
        ..raw_event(E1, "placeholder", T0 + 14 * 60 * 1000, Some(C1), 1)
    };
    seed_events(db.pool(), GUILD, &[normalize(&hostile)]).await;
    rsvp_in(db.pool(), GUILD, E1, U_GOING, RsvpStatus::Going).await;
    let mock = success_mock().await;
    bounded_tick(db.pool(), &executor(&mock), GUILD, T0).await;
    let posts = channel_posts(&mock.requests(), C1);
    mock.shutdown().await;
    db.close().await.expect("verified fixture teardown");
    assert_eq!(posts.len(), 1, "the reminder still posts");
    let content = content_of(&posts[0]);
    // The benign name fragments prove the name is included, not dropped to
    // dodge neutralization; the mass-mention forms must not reach the wire.
    assert!(
        content.contains("Launch") && content.contains("party"),
        "reminder names the event: {content}"
    );
    assert!(
        !content.contains("@everyone"),
        "raw mass mention reached the wire: {content}"
    );
    assert_mentions(content, &[U_GOING], &[]);
    assert_render_safe(&posts[0]);
}

#[tokio::test]
async fn channel_less_event_posts_nothing() {
    let Some(db) = fixture().await else {
        return;
    };
    seed_events(
        db.pool(),
        GUILD,
        &[normalize(&raw_event(
            E1,
            "Nowhere night",
            T0 + 14 * 60 * 1000,
            None,
            1,
        ))],
    )
    .await;
    rsvp_in(db.pool(), GUILD, E1, U_GOING, RsvpStatus::Going).await;
    let mock = success_mock().await;
    bounded_tick(db.pool(), &executor(&mock), GUILD, T0).await;
    let requests = mock.requests();
    mock.shutdown().await;
    db.close().await.expect("verified fixture teardown");
    assert!(
        requests.is_empty(),
        "an event with no channel posts nothing, got {} requests",
        requests.len()
    );
}

#[tokio::test]
async fn ordinary_scheduled_message_still_posts_alongside_reminders() {
    let Some(db) = fixture().await else {
        return;
    };
    // Control half: the existing tick behavior, green before and after.
    let at = format_iso_ms(T0 - 3_600_000);
    assert!(put_scheduled(
        db.pool(),
        &ScheduledWrite {
            id: "reminder-control".to_owned(),
            guild_id: GUILD.to_owned(),
            channel_id: SCHED_CHANNEL.to_owned(),
            body: "standup".to_owned(),
            next_run_at: format_iso_ms(T0 - 1_000),
            interval_seconds: None,
            enabled: true,
            created_by: U_GOING.to_owned(),
            created_at: at.clone(),
            updated_by: U_GOING.to_owned(),
            updated_at: at,
        },
    )
    .await
    .expect("seed scheduled control"));
    seed_events(
        db.pool(),
        GUILD,
        &[normalize(&raw_event(
            E1,
            "Games night",
            T0 + 14 * 60 * 1000,
            Some(C1),
            1,
        ))],
    )
    .await;
    rsvp_in(db.pool(), GUILD, E1, U_GOING, RsvpStatus::Going).await;
    let mock = success_mock().await;
    bounded_tick(db.pool(), &executor(&mock), GUILD, T0).await;
    let requests = mock.requests();
    let control = channel_posts(&requests, SCHED_CHANNEL);
    let reminders = channel_posts(&requests, C1);
    assert_no_dm_routes(&requests, &[C1, SCHED_CHANNEL]);
    mock.shutdown().await;
    db.close().await.expect("verified fixture teardown");
    assert_eq!(control.len(), 1, "the tick itself ran");
    assert_eq!(reminders.len(), 1, "the reminder posts too");
    assert_mentions(content_of(&reminders[0]), &[U_GOING], &[]);
    assert_render_safe(&reminders[0]);
}
