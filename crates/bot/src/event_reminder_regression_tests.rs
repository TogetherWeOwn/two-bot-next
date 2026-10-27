//! Companion acceptance for the reminder's pre-wire fence, complete rendering,
//! bounded admission and error propagation. The original acceptance stays intact.
//! Only the Discord boundary is doubled; all reservations/audits use the guarded
//! disposable database. Assertions below name the regressions they reject.
//!
//! Protected acceptance: bounded fallback notice for oversized reminders.
//! Keep this file verbatim in the build handoff; the Builder never edits it.
//!
//! Contract under test (oversized but otherwise VALID sources only):
//! - A complete, permitted source whose full-mention rendering would exceed
//!   [`CONTENT_LIMIT`] (or whose roster exceeds the RSVP bound) still posts
//!   exactly ONE channel notice: the bounded fallback. Never a partial roster,
//!   never a mention ping, never a second post, never a DM.
//! - Fallback shape (all required): bounded event name, `{n} attendee`
//!   count, verbatim `starts_at`, event link
//!   `https://discord.com/events/{guild}/{event}`, zero `<@` substrings, no
//!   attendee ids, `text_len <= CONTENT_LIMIT`, `allowed_mentions.parse == []`.
//! - The fallback counts as the one delivery attempt: outcome `posted`, reason
//!   `None`, immutable across restart; a cancelled tick fabricates no result.
//! - Invalid sources (bad snowflakes) still fail closed with
//!   `invalid_or_oversized_reminder` and no post. Incomplete sources (no
//!   channel, no attendees) post nothing and leave no audit rows. Exact-limit
//!   rosters keep full mentions; valid tails always progress.
//!
//! RED shape (baseline): oversized sources post nothing, so every "fallback
//! appears" assertion fails with **0 fallback posts** while the valid-tail
//! control still posts. GREEN: the builder renders the fallback instead of
//! `None` for valid oversized sources, without editing this file.

#![cfg(test)]

#[path = "event_reminder_store_regression_tests.rs"]
mod store;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use sqlx::PgPool;
use two_bot_core::event_reminder_store::{
    begin_event_reminder_delivery, due_event_reminders, EventReminder,
};
use two_bot_core::message_safety::{contains_mass_mention, text_len, CONTENT_LIMIT};
use two_bot_core::rsvp::{RsvpRecord, RsvpStatus};
use two_bot_core::rsvp_store::put_rsvp;
use two_bot_core::scheduled::format_iso_ms;
use two_bot_core::scheduled_events::{EventStatus, ScheduledEvent};
use two_bot_core::website_store::replace_events;
use two_bot_discord::executor::ActionExecutor;
use two_bot_testsupport::TestDatabase;

use super::super::tick;
use crate::discord_test_common::{MockRest, RestRequest, ScriptedResponse};
use crate::jobs::ErrorClass;

const GUILD: &str = "3333";
const OTHER_GUILD: &str = "7777";
const T0: u64 = 1_790_000_000_000;
const START: u64 = T0 + 900_000;
const E1: &str = "1546451670500642999";
const E2: &str = "1546451670500643001";
const E3: &str = "1546451670500643002";
const E4: &str = "1546451670500643003";
const C1: &str = "1546451670500642111";
const U1: &str = "1546451670500642888";
const U2: &str = "1546451670500642779";
const U3: &str = "1546451670500642666";
const DECLINED: &str = "1546451670500642777";

async fn fixture() -> Option<TestDatabase> {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI must supply the guarded disposable database"
        );
        eprintln!("SKIP event reminder regressions: TWO_TEST_DATABASE_URL is not set");
        return None;
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create migrated disposable database"),
    )
}

fn event(id: &str, name: &str) -> ScheduledEvent {
    ScheduledEvent {
        id: id.to_owned(),
        name: name.to_owned(),
        starts_at: format_iso_ms(START),
        channel_id: Some(C1.to_owned()),
        description: None,
        status: EventStatus::Scheduled,
    }
}

fn event_in_channel(id: &str, name: &str, channel: Option<&str>) -> ScheduledEvent {
    ScheduledEvent {
        id: id.to_owned(),
        name: name.to_owned(),
        starts_at: format_iso_ms(START),
        channel_id: channel.map(str::to_owned),
        description: None,
        status: EventStatus::Scheduled,
    }
}

/// The bounded fallback contract: neutral event facts only. Rejects partial
/// rosters (any `<@` or attendee id), unbounded rendering, missing count /
/// start / link, and mention parsing. `name_fragment` is the full name for
/// short names and a head prefix for huge ones.
fn assert_fallback_shape(
    post: &Value,
    guild: &str,
    id: &str,
    name_fragment: &str,
    count: usize,
    absent_ids: &[String],
) {
    let content = post["content"].as_str().expect("fallback content");
    assert!(
        content.contains(name_fragment),
        "fallback names the bounded event: {content:?}"
    );
    assert!(
        content.contains(&format!("{count} attendee")),
        "fallback carries the attendee count: {content:?}"
    );
    assert!(
        content.contains(&format_iso_ms(START)),
        "fallback carries the start time: {content:?}"
    );
    assert!(
        content.contains(&format!("https://discord.com/events/{guild}/{id}")),
        "fallback links the event: {content:?}"
    );
    assert_eq!(
        content.matches("<@").count(),
        0,
        "fallback never pings: {content:?}"
    );
    for user in absent_ids {
        assert!(
            !content.contains(user.as_str()),
            "fallback never leaks roster ids: {content:?}"
        );
    }
    assert!(
        text_len(content) <= CONTENT_LIMIT,
        "fallback stays bounded: {} units",
        text_len(content)
    );
    assert!(!contains_mass_mention(content), "fallback is mention-safe");
    assert_eq!(post["allowed_mentions"]["parse"], json!([]));
}

async fn seed_events(pool: &PgPool, guild: &str, events: &[ScheduledEvent]) {
    assert!(replace_events(pool, guild, &format_iso_ms(T0), events)
        .await
        .expect("seed event mirror")
        .is_none());
}

async fn rsvp(pool: &PgPool, guild: &str, id: &str, user: &str, status: RsvpStatus) {
    put_rsvp(
        pool,
        &RsvpRecord {
            guild_id: guild.to_owned(),
            event_id: id.to_owned(),
            user_id: user.to_owned(),
            status,
            responded_at: format_iso_ms(T0 - 60_000),
        },
    )
    .await
    .expect("seed RSVP");
}

async fn seed_one(pool: &PgPool) {
    seed_events(pool, GUILD, &[event(E1, "Games night")]).await;
    rsvp(pool, GUILD, E1, U1, RsvpStatus::Going).await;
}

fn executor(mock: &MockRest) -> ActionExecutor {
    crate::gateway::ensure_crypto_provider();
    ActionExecutor::with_proxy(
        "synthetic-reminder-regression-token".to_owned(),
        Some(mock.origin()),
    )
    .expect("loopback executor")
}

async fn success_mock() -> MockRest {
    let seq = Arc::new(AtomicU64::new(9001));
    MockRest::with_responder(move |_: &RestRequest| {
        let id = seq.fetch_add(1, Ordering::SeqCst);
        ScriptedResponse::json(200, json!({"id": id.to_string()}))
    })
    .await
}

async fn tick_result(
    pool: &PgPool,
    rest: &ActionExecutor,
    guild: &str,
    now: u64,
) -> Result<(), ErrorClass> {
    tokio::time::timeout(Duration::from_secs(20), tick(pool, rest, guild, || now))
        .await
        .expect("tick must finish within its test budget")
}

async fn successful_tick(pool: &PgPool, rest: &ActionExecutor, guild: &str, now: u64) {
    tick_result(pool, rest, guild, now)
        .await
        .expect("tick propagates no store error");
}

fn posts(mock: &MockRest) -> Vec<Value> {
    mock.requests()
        .into_iter()
        .filter(|request| request.method == "POST")
        .map(|request| {
            assert_eq!(
                request.path,
                format!("/api/v10/channels/{C1}/messages"),
                "reminders must use only the event channel"
            );
            serde_json::from_slice(&request.body).expect("POST JSON")
        })
        .collect()
}

fn assert_complete_mentions(post: &Value, users: &[String]) {
    let content = post["content"].as_str().expect("reminder content");
    for user in users {
        assert_eq!(
            content.matches(&format!("<@{user}>")).count(),
            1,
            "each eligible attendee appears exactly once"
        );
    }
    assert_eq!(
        content.matches("<@").count(),
        users.len(),
        "no extra mentions"
    );
    assert!(!content.contains(DECLINED));
    assert_eq!(post["allowed_mentions"]["parse"], json!([]));
}

#[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
struct Audit {
    id: String,
    guild_id: String,
    actor_id: Option<String>,
    action: String,
    target_key: Option<String>,
    outcome: String,
    reason: Option<String>,
    at_ms: i64,
}

async fn audits(pool: &PgPool, guild: &str, id: &str) -> Vec<Audit> {
    sqlx::query_as(
        "SELECT id, guild_id, actor_id, action, target_key, outcome, reason,
                (extract(epoch FROM created_at::timestamptz) * 1000)::bigint AS at_ms
           FROM automation_audit_log
          WHERE guild_id = $1 AND target_key = $2
            AND action IN ('rsvp.reminder_attempt', 'rsvp.reminder')
          ORDER BY (action = 'rsvp.reminder_attempt'), id",
    )
    .bind(guild)
    .bind(id)
    .fetch_all(pool)
    .await
    .expect("read reminder audit facts")
}

fn pending_attempt(guild: &str, id: &str) -> Audit {
    Audit {
        id: format!("event:reminder:{guild}:{id}"),
        guild_id: guild.to_owned(),
        actor_id: None,
        action: "rsvp.reminder_attempt".to_owned(),
        target_key: Some(id.to_owned()),
        outcome: "unknown".to_owned(),
        reason: Some("delivery_pending".to_owned()),
        at_ms: T0 as i64,
    }
}

fn assert_outcome(rows: &[Audit], guild: &str, id: &str, outcome: &str, reason: Option<&str>) {
    assert_eq!(
        rows.len(),
        2,
        "one immutable attempt and one separate result"
    );
    assert_eq!(rows[1], pending_attempt(guild, id));
    let result = &rows[0];
    assert!(!result.id.is_empty());
    assert_ne!(
        result.id, rows[1].id,
        "never update the attempt into a result"
    );
    assert_eq!(result.guild_id, guild);
    assert_eq!(
        result.actor_id, None,
        "system actor contains no member data"
    );
    assert_eq!(result.action, "rsvp.reminder");
    assert_eq!(result.target_key.as_deref(), Some(id));
    assert_eq!(result.outcome, outcome);
    assert_eq!(result.reason.as_deref(), reason);
    assert_eq!(result.at_ms, T0 as i64);
}

// Holding the actual HTTP response after receipt exposes the pre-wire crash
// window. A post-send/in-memory-only fence cannot satisfy the independent read.
#[tokio::test]
async fn committed_attempt_is_visible_before_response_and_abort_restart_never_resends() {
    let Some(db) = fixture().await else { return };
    seed_one(db.pool()).await;
    let (mock, gate) = MockRest::start_gated(
        vec![ScriptedResponse::json(200, json!({"id": "9001"}))],
        ScriptedResponse::json(200, json!({"id": "9002"})),
    )
    .await;
    let worker = db.independent_pool().await.expect("in-flight worker pool");
    let observer = db
        .independent_pool()
        .await
        .expect("independent committed-state observer");
    let rest = executor(&mock);
    let task = tokio::spawn({
        let pool = worker.clone();
        async move { tick(&pool, &rest, GUILD, || T0).await }
    });
    tokio::time::timeout(Duration::from_secs(10), gate.wait_for_request())
        .await
        .expect("POST received while response is withheld");
    assert_eq!(posts(&mock).len(), 1);
    assert_eq!(
        audits(&observer, GUILD, E1).await,
        [pending_attempt(GUILD, E1)],
        "attempt must COMMIT before the response, with no result yet"
    );
    task.abort();
    assert!(task
        .await
        .expect_err("in-flight tick was aborted")
        .is_cancelled());
    worker.close().await;
    let restarted = db
        .independent_pool()
        .await
        .expect("fresh pool after worker death");
    // Response is STILL held. Neither the old receipt nor executor memory can
    // supply the replay exclusion observed by this fresh worker.
    successful_tick(&restarted, &executor(&mock), GUILD, T0 + 1).await;
    assert_eq!(
        posts(&mock).len(),
        1,
        "crash window must never authorize a second POST"
    );
    assert_eq!(
        audits(&restarted, GUILD, E1).await,
        [pending_attempt(GUILD, E1)]
    );
    gate.release();
    tokio::time::timeout(Duration::from_secs(10), gate.wait_for_completion())
        .await
        .expect("held response handler finished");
    successful_tick(&restarted, &executor(&mock), GUILD, T0 + 2).await;
    assert_eq!(posts(&mock).len(), 1);
    assert_eq!(
        audits(&observer, GUILD, E1).await,
        [pending_attempt(GUILD, E1)],
        "cancelled tick must not fabricate a result"
    );
    observer.close().await;
    restarted.close().await;
    mock.shutdown().await;
    db.close().await.expect("verified teardown");
}

#[tokio::test]
async fn simultaneous_event_reminder_ticks_commit_one_attempt_and_send_one_post() {
    let Some(db) = fixture().await else { return };
    seed_one(db.pool()).await;
    let (mock, gate) = MockRest::start_gated(
        vec![ScriptedResponse::json(200, json!({"id": "9001"}))],
        ScriptedResponse::json(200, json!({"id": "9002"})),
    )
    .await;
    let peer1 = db.independent_pool().await.expect("first worker pool");
    let peer2 = db.independent_pool().await.expect("second worker pool");
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let mut tasks = Vec::new();
    for pool in [peer1.clone(), peer2.clone()] {
        let barrier = barrier.clone();
        let rest = executor(&mock);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            tick(&pool, &rest, GUILD, || T0).await
        }));
    }
    barrier.wait().await;
    tokio::time::timeout(Duration::from_secs(10), gate.wait_for_request())
        .await
        .expect("one racing event POST reached the wire");
    assert_eq!(
        audits(db.pool(), GUILD, E1).await,
        [pending_attempt(GUILD, E1)]
    );
    gate.release();
    for task in tasks {
        tokio::time::timeout(Duration::from_secs(20), task)
            .await
            .expect("racing tick terminates")
            .expect("worker joined")
            .expect("tick succeeds");
    }
    assert_eq!(
        posts(&mock).len(),
        1,
        "simultaneous EVENT ticks, not scheduled messages, have one winner"
    );
    assert_outcome(
        &audits(db.pool(), GUILD, E1).await,
        GUILD,
        E1,
        "posted",
        None,
    );
    peer1.close().await;
    peer2.close().await;
    mock.shutdown().await;
    db.close().await.expect("verified teardown");
}

#[tokio::test]
async fn local_admission_refusal_has_zero_wire_posts_but_durable_attempt_and_result() {
    use two_bot_discord::ratelimit_guard::RateLimitGuard;

    let Some(db) = fixture().await else { return };
    seed_one(db.pool()).await;
    let mock = success_mock().await;
    crate::gateway::ensure_crypto_provider();
    let guard = Arc::new(RateLimitGuard::new(Default::default()).expect("isolated rate guard"));
    guard.observe_status(401, true);
    let rest = ActionExecutor::with_proxy_and_guard(
        "synthetic-reminder-regression-token".to_owned(),
        Some(mock.origin()),
        guard,
    )
    .expect("executor with latched local admission guard");
    for offset in [0, 15_000, 30_000] {
        successful_tick(db.pool(), &rest, GUILD, T0 + offset).await;
    }
    assert!(
        mock.requests().is_empty(),
        "local admission refusal never reaches transport"
    );
    let before = audits(db.pool(), GUILD, E1).await;
    assert_outcome(&before, GUILD, E1, "failed", Some("discord_guard_refused"));
    let peer = db
        .independent_pool()
        .await
        .expect("independent restart pool");
    // The replacement executor has a HEALTHY guard, so silence proves the
    // durable fence, not the continued presence of the original local refusal.
    successful_tick(&peer, &executor(&mock), GUILD, T0 + 45_000).await;
    assert!(mock.requests().is_empty());
    assert_eq!(audits(&peer, GUILD, E1).await, before);
    peer.close().await;
    mock.shutdown().await;
    db.close().await.expect("verified teardown");
}

// Reject first()/last()-only rendering and duplicated/missing attendee mentions.
#[tokio::test]
async fn complete_eligible_list_is_posted_once_with_separate_immutable_audit() {
    let Some(db) = fixture().await else { return };
    seed_one(db.pool()).await;
    rsvp(db.pool(), GUILD, E1, U2, RsvpStatus::Interested).await;
    rsvp(db.pool(), GUILD, E1, U3, RsvpStatus::Going).await;
    rsvp(db.pool(), GUILD, E1, DECLINED, RsvpStatus::Declined).await;
    let mock = success_mock().await;
    successful_tick(db.pool(), &executor(&mock), GUILD, T0).await;
    let wire = posts(&mock);
    assert_eq!(wire.len(), 1);
    assert_complete_mentions(&wire[0], &[U1.to_owned(), U2.to_owned(), U3.to_owned()]);
    let before = audits(db.pool(), GUILD, E1).await;
    assert_outcome(&before, GUILD, E1, "posted", None);
    let peer = db.independent_pool().await.expect("fresh restart pool");
    successful_tick(&peer, &executor(&mock), GUILD, T0 + 1).await;
    assert_eq!(posts(&mock).len(), 1);
    assert_eq!(
        audits(&peer, GUILD, E1).await,
        before,
        "attempt stays immutable"
    );
    peer.close().await;
    mock.shutdown().await;
    db.close().await.expect("verified teardown");
}

// Reject an unbounded reminder batch and a fenced head starving the third row.
#[tokio::test]
async fn three_due_events_take_two_slots_then_the_remaining_slot_next_tick() {
    let Some(db) = fixture().await else { return };
    seed_events(
        db.pool(),
        GUILD,
        &[event(E1, "One"), event(E2, "Two"), event(E3, "Three")],
    )
    .await;
    for id in [E1, E2, E3] {
        rsvp(db.pool(), GUILD, id, U1, RsvpStatus::Going).await;
    }
    let mock = success_mock().await;
    let rest = executor(&mock);
    successful_tick(db.pool(), &rest, GUILD, T0).await;
    assert_eq!(posts(&mock).len(), 2, "exactly two attempts on first tick");
    for id in [E1, E2] {
        assert_outcome(
            &audits(db.pool(), GUILD, id).await,
            GUILD,
            id,
            "posted",
            None,
        );
    }
    assert!(audits(db.pool(), GUILD, E3).await.is_empty());
    successful_tick(db.pool(), &rest, GUILD, T0).await;
    assert_eq!(
        posts(&mock).len(),
        3,
        "tail is not starved by retained attempts"
    );
    assert_outcome(
        &audits(db.pool(), GUILD, E3).await,
        GUILD,
        E3,
        "posted",
        None,
    );
    successful_tick(db.pool(), &rest, GUILD, T0 + 1).await;
    assert_eq!(posts(&mock).len(), 3);
    mock.shutdown().await;
    db.close().await.expect("verified teardown");
}

// 78 nineteen-digit mentions plus spaces = 1,793 UTF-16 units. The literal
// framing contributes 30; a 177-unit name reaches exactly 2,000. The astral
// character contributes TWO units, exposing byte/scalar-count substitutions.
fn boundary_name(extra: usize) -> String {
    format!("{}🚀", "a".repeat(175 + extra))
}

async fn seed_attendees(pool: &PgPool, id: &str, count: i64) -> Vec<String> {
    // Direct fixture insertion deliberately covers rows beyond RSVP admission.
    sqlx::query(
        "INSERT INTO event_rsvps (guild_id, event_id, user_id, status, responded_at)
         SELECT $1, $2, (1546451670500650000::bigint + n)::text, 'going', $3::text::timestamptz
           FROM generate_series(1::bigint, $4::bigint) n",
    )
    .bind(GUILD)
    .bind(id)
    .bind(format_iso_ms(T0 - 60_000))
    .bind(count)
    .execute(pool)
    .await
    .expect("seed bounded attendee fixture");
    (1..=count)
        .map(|n| (1546451670500650000_i64 + n).to_string())
        .collect()
}

#[tokio::test]
async fn exact_two_thousand_utf16_units_with_astral_name_posts_complete_list() {
    let Some(db) = fixture().await else { return };
    seed_events(db.pool(), GUILD, &[event(E1, &boundary_name(0))]).await;
    let users = seed_attendees(db.pool(), E1, 78).await;
    let mock = success_mock().await;
    successful_tick(db.pool(), &executor(&mock), GUILD, T0).await;
    let wire = posts(&mock);
    assert_eq!(wire.len(), 1, "exact boundary must be accepted");
    let content = wire[0]["content"].as_str().expect("content");
    assert_eq!(content.encode_utf16().count(), 2000);
    assert!(content.contains('🚀'), "do not silently remove astral text");
    assert_complete_mentions(&wire[0], &users);
    assert_outcome(
        &audits(db.pool(), GUILD, E1).await,
        GUILD,
        E1,
        "posted",
        None,
    );
    mock.shutdown().await;
    db.close().await.expect("verified teardown");
}

async fn assert_oversize_fallback_without_partial_roster(name: &str, count: i64) {
    let Some(db) = fixture().await else { return };
    seed_events(
        db.pool(),
        GUILD,
        &[event(E1, name), event(E2, "Valid tail")],
    )
    .await;
    let users = seed_attendees(db.pool(), E1, count).await;
    rsvp(db.pool(), GUILD, E2, U3, RsvpStatus::Going).await;
    let mock = success_mock().await;
    let rest = executor(&mock);
    successful_tick(db.pool(), &rest, GUILD, T0).await;
    let wire = posts(&mock);
    assert_eq!(
        wire.len(),
        2,
        "oversized event emits its bounded fallback; valid tail progresses"
    );
    let tail = format!("Event reminder: Valid tail starts soon.\n<@{U3}>");
    let fallback = wire
        .iter()
        .find(|post| post["content"].as_str() != Some(tail.as_str()))
        .expect("one fallback alongside the valid tail");
    let tail_post = wire
        .iter()
        .find(|post| post["content"].as_str() == Some(tail.as_str()))
        .expect("valid tail keeps full mentions");
    assert_complete_mentions(tail_post, &[U3.to_owned()]);
    assert_fallback_shape(fallback, GUILD, E1, name, count as usize, &users);
    let before_e1 = audits(db.pool(), GUILD, E1).await;
    assert_outcome(&before_e1, GUILD, E1, "posted", None);
    let before_e2 = audits(db.pool(), GUILD, E2).await;
    assert_outcome(&before_e2, GUILD, E2, "posted", None);
    let peer = db.independent_pool().await.expect("restart pool");
    successful_tick(&peer, &executor(&mock), GUILD, T0 + 1).await;
    assert_eq!(
        posts(&mock).len(),
        2,
        "fallback attempts remain fenced across restart"
    );
    assert_eq!(audits(&peer, GUILD, E1).await, before_e1);
    assert_eq!(audits(&peer, GUILD, E2).await, before_e2);
    peer.close().await;
    mock.shutdown().await;
    db.close().await.expect("verified teardown");
}

#[tokio::test]
async fn two_thousand_and_one_utf16_units_never_posts_a_partial_attendee_list() {
    assert_oversize_fallback_without_partial_roster(&boundary_name(1), 78).await;
}

#[tokio::test]
async fn attendee_array_overflow_never_posts_a_truncated_prefix() {
    // 1,001 is one beyond the supported 1,000 RSVP rows, not a constant pin.
    assert_oversize_fallback_without_partial_roster("Overflow", 1001).await;
}

#[tokio::test]
async fn huge_astral_name_with_mass_mention_fallback_stays_bounded_and_safe() {
    let Some(db) = fixture().await else { return };
    // Astral + hostile mention first so any head truncation must preserve the
    // scalar and neutralize the ping; the tail forces the fallback path.
    let name = format!("🚀@everyone {}", "n".repeat(3000));
    seed_events(
        db.pool(),
        GUILD,
        &[event(E1, &name), event(E2, "Valid tail")],
    )
    .await;
    rsvp(db.pool(), GUILD, E1, U1, RsvpStatus::Going).await;
    rsvp(db.pool(), GUILD, E1, U2, RsvpStatus::Interested).await;
    rsvp(db.pool(), GUILD, E1, U3, RsvpStatus::Going).await;
    rsvp(db.pool(), GUILD, E2, U1, RsvpStatus::Going).await;
    let mock = success_mock().await;
    successful_tick(db.pool(), &executor(&mock), GUILD, T0).await;
    let wire = posts(&mock);
    assert_eq!(wire.len(), 2, "huge name falls back; valid tail progresses");
    let tail = format!("Event reminder: Valid tail starts soon.\n<@{U1}>");
    let fallback = wire
        .iter()
        .find(|post| post["content"].as_str() != Some(tail.as_str()))
        .expect("one fallback alongside the valid tail");
    let content = fallback["content"].as_str().expect("fallback content");
    assert!(content.contains('🚀'), "do not silently remove astral text");
    assert!(
        !content.contains(&name),
        "huge name must be bounded, not echoed whole"
    );
    assert_fallback_shape(
        fallback,
        GUILD,
        E1,
        "🚀",
        3,
        &[U1.to_owned(), U2.to_owned(), U3.to_owned()],
    );
    assert_outcome(
        &audits(db.pool(), GUILD, E1).await,
        GUILD,
        E1,
        "posted",
        None,
    );
    let peer = db.independent_pool().await.expect("restart pool");
    successful_tick(&peer, &executor(&mock), GUILD, T0 + 1).await;
    assert_eq!(posts(&mock).len(), 2, "fallback stays fenced");
    peer.close().await;
    mock.shutdown().await;
    db.close().await.expect("verified teardown");
}

// The fallback is one attempt: held response, aborted worker and restart must
// never authorize a second fallback POST or fabricate a result row.
#[tokio::test]
async fn fallback_attempt_survives_abort_restart_without_resend() {
    let Some(db) = fixture().await else { return };
    seed_events(db.pool(), GUILD, &[event(E1, &boundary_name(1))]).await;
    let users = seed_attendees(db.pool(), E1, 78).await;
    let (mock, gate) = MockRest::start_gated(
        vec![ScriptedResponse::json(200, json!({"id": "9001"}))],
        ScriptedResponse::json(200, json!({"id": "9002"})),
    )
    .await;
    let worker = db.independent_pool().await.expect("in-flight worker pool");
    let rest = executor(&mock);
    let task = tokio::spawn({
        let pool = worker.clone();
        async move { tick(&pool, &rest, GUILD, || T0).await }
    });
    tokio::time::timeout(Duration::from_secs(10), gate.wait_for_request())
        .await
        .expect("fallback POST received while response is withheld");
    let wire = posts(&mock);
    assert_eq!(wire.len(), 1);
    assert_fallback_shape(&wire[0], GUILD, E1, &boundary_name(1), 78, &users);
    task.abort();
    assert!(task
        .await
        .expect_err("in-flight tick was aborted")
        .is_cancelled());
    worker.close().await;
    let restarted = db
        .independent_pool()
        .await
        .expect("fresh pool after worker death");
    successful_tick(&restarted, &executor(&mock), GUILD, T0 + 1).await;
    assert_eq!(
        posts(&mock).len(),
        1,
        "crash window must never authorize a second fallback"
    );
    assert_eq!(
        audits(&restarted, GUILD, E1).await,
        [pending_attempt(GUILD, E1)]
    );
    gate.release();
    tokio::time::timeout(Duration::from_secs(10), gate.wait_for_completion())
        .await
        .expect("held response handler finished");
    successful_tick(&restarted, &executor(&mock), GUILD, T0 + 2).await;
    assert_eq!(posts(&mock).len(), 1);
    assert_eq!(
        audits(&restarted, GUILD, E1).await,
        [pending_attempt(GUILD, E1)],
        "cancelled tick must not fabricate a result"
    );
    restarted.close().await;
    mock.shutdown().await;
    db.close().await.expect("verified teardown");
}

async fn assert_invalid_source_fails_closed(id: &str, channel: Option<&str>, user: &str) {
    let Some(db) = fixture().await else { return };
    seed_events(
        db.pool(),
        GUILD,
        &[event_in_channel(id, "Invalid source", channel)],
    )
    .await;
    // Direct fixture insert: invalid ids bypass RSVP admission by construction.
    sqlx::query(
        "INSERT INTO event_rsvps (guild_id, event_id, user_id, status, responded_at)
         VALUES ($1, $2, $3, 'going', $4::text::timestamptz)",
    )
    .bind(GUILD)
    .bind(id)
    .bind(user)
    .bind(format_iso_ms(T0 - 60_000))
    .execute(db.pool())
    .await
    .expect("seed invalid-source attendee fixture");
    let mock = success_mock().await;
    let rest = executor(&mock);
    successful_tick(db.pool(), &rest, GUILD, T0).await;
    assert!(
        posts(&mock).is_empty(),
        "invalid sources fail closed: no fallback, no partial post"
    );
    let before = audits(db.pool(), GUILD, id).await;
    assert_outcome(
        &before,
        GUILD,
        id,
        "failed",
        Some("invalid_or_oversized_reminder"),
    );
    let peer = db.independent_pool().await.expect("restart pool");
    successful_tick(&peer, &executor(&mock), GUILD, T0 + 1).await;
    assert!(posts(&mock).is_empty(), "failed sources stay fenced");
    assert_eq!(audits(&peer, GUILD, id).await, before);
    peer.close().await;
    mock.shutdown().await;
    db.close().await.expect("verified teardown");
}

#[tokio::test]
async fn invalid_event_id_fails_closed_without_any_post() {
    assert_invalid_source_fails_closed("bad-event!!", Some(C1), U1).await;
}

#[tokio::test]
async fn invalid_channel_id_fails_closed_without_any_post() {
    assert_invalid_source_fails_closed(E1, Some("bad-channel!!"), U1).await;
}

#[tokio::test]
async fn invalid_attendee_id_fails_closed_without_any_post() {
    assert_invalid_source_fails_closed(E1, Some(C1), "not-a-user!!").await;
}

#[tokio::test]
async fn incomplete_sources_stay_silent_while_fallback_and_tail_progress() {
    let Some(db) = fixture().await else { return };
    seed_events(
        db.pool(),
        GUILD,
        &[
            event(E1, &boundary_name(1)),
            event(E2, "Valid tail"),
            event_in_channel(E3, "Channel-less", None),
            event(E4, "Nobody coming"),
        ],
    )
    .await;
    let users = seed_attendees(db.pool(), E1, 78).await;
    rsvp(db.pool(), GUILD, E2, U3, RsvpStatus::Going).await;
    rsvp(db.pool(), GUILD, E3, U1, RsvpStatus::Going).await;
    let mock = success_mock().await;
    let rest = executor(&mock);
    successful_tick(db.pool(), &rest, GUILD, T0).await;
    let wire = posts(&mock);
    assert_eq!(
        wire.len(),
        2,
        "fallback plus valid tail; incomplete sources post nothing"
    );
    let tail = format!("Event reminder: Valid tail starts soon.\n<@{U3}>");
    let fallback = wire
        .iter()
        .find(|post| post["content"].as_str() != Some(tail.as_str()))
        .expect("one fallback alongside the valid tail");
    assert_fallback_shape(fallback, GUILD, E1, &boundary_name(1), 78, &users);
    // A second tick proves the incomplete sources are unselectable, not queued.
    successful_tick(db.pool(), &rest, GUILD, T0).await;
    assert_eq!(posts(&mock).len(), 2);
    assert!(audits(db.pool(), GUILD, E3).await.is_empty());
    assert!(audits(db.pool(), GUILD, E4).await.is_empty());
    mock.shutdown().await;
    db.close().await.expect("verified teardown");
}

async fn assert_refusal_fenced(status: u16, reason: &str) {
    let Some(db) = fixture().await else { return };
    seed_one(db.pool()).await;
    let mock = MockRest::with_responder(move |_: &RestRequest| {
        ScriptedResponse::json(
            status,
            json!({"message": "synthetic refusal", "retry_after": 0.01}),
        )
    })
    .await;
    let rest = executor(&mock);
    for offset in [0, 15_000, 30_000] {
        successful_tick(db.pool(), &rest, GUILD, T0 + offset).await;
        assert_eq!(
            posts(&mock).len(),
            1,
            "definitive refusal must never authorize replay"
        );
    }
    let before = audits(db.pool(), GUILD, E1).await;
    assert_outcome(&before, GUILD, E1, "failed", Some(reason));
    let peer = db.independent_pool().await.expect("fresh restart pool");
    let restarted = executor(&mock);
    for offset in [45_000, 60_000, 75_000] {
        successful_tick(&peer, &restarted, GUILD, T0 + offset).await;
    }
    assert_eq!(
        posts(&mock).len(),
        1,
        "independent restart must preserve strict refusal fence"
    );
    assert_eq!(audits(&peer, GUILD, E1).await, before);
    peer.close().await;
    mock.shutdown().await;
    db.close().await.expect("verified teardown");
}

#[tokio::test]
async fn forbidden_posts_exactly_once_across_three_ticks_and_restart() {
    assert_refusal_fenced(403, "discord_rejected").await;
}

#[tokio::test]
async fn rate_limited_posts_exactly_once_across_three_ticks_and_restart() {
    assert_refusal_fenced(429, "discord_rate_limited").await;
}

#[tokio::test]
async fn lost_receipt_has_separate_uncertain_outcome_and_immutable_pending_fact() {
    let Some(db) = fixture().await else { return };
    seed_one(db.pool()).await;
    let mock = MockRest::with_responder(|_: &RestRequest| {
        ScriptedResponse::json(200, json!({"id": "9001"})).truncated_body()
    })
    .await;
    successful_tick(db.pool(), &executor(&mock), GUILD, T0).await;
    assert_eq!(posts(&mock).len(), 1);
    let before = audits(db.pool(), GUILD, E1).await;
    assert_outcome(
        &before,
        GUILD,
        E1,
        "delivery_uncertain",
        Some("discord_unavailable"),
    );
    let peer = db.independent_pool().await.expect("fresh restart pool");
    successful_tick(&peer, &executor(&mock), GUILD, T0 + 1).await;
    assert_eq!(posts(&mock).len(), 1);
    assert_eq!(audits(&peer, GUILD, E1).await, before);
    peer.close().await;
    mock.shutdown().await;
    db.close().await.expect("verified teardown");
}

#[tokio::test]
async fn selection_failure_is_returned_as_database_error_without_wire_or_audit() {
    let Some(db) = fixture().await else { return };
    seed_one(db.pool()).await;
    assert_eq!(
        due_event_reminders(db.pool(), GUILD, T0)
            .await
            .expect("selection control")
            .len(),
        1
    );
    // Disposable fixture only: make the event SELECT unavailable while keeping
    // the ordinary scheduled-message scan intact, so its error cannot mask this.
    sqlx::query("ALTER TABLE scheduled_events RENAME TO unavailable_events")
        .execute(db.pool())
        .await
        .expect("inject selection failure");
    let mock = success_mock().await;
    let result = tick(db.pool(), &executor(&mock), GUILD, || T0).await;
    assert!(
        matches!(result, Err(ErrorClass::Database)),
        "selection failure must escape tick: {result:?}"
    );
    assert!(posts(&mock).is_empty());
    assert!(audits(db.pool(), GUILD, E1).await.is_empty());
    sqlx::query("ALTER TABLE unavailable_events RENAME TO scheduled_events")
        .execute(db.pool())
        .await
        .expect("restore fixture table");
    successful_tick(db.pool(), &executor(&mock), GUILD, T0).await;
    assert_eq!(posts(&mock).len(), 1, "restored selection positive control");
    mock.shutdown().await;
    db.close().await.expect("verified teardown");
}

async fn refuse_attempt(pool: &PgPool) {
    sqlx::raw_sql(
        "CREATE FUNCTION refuse_reminder_insert() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'synthetic reservation failure'; END $$;
         CREATE TRIGGER fail_reminder_insert BEFORE INSERT ON automation_audit_log
         FOR EACH ROW WHEN (NEW.action = 'rsvp.reminder_attempt')
         EXECUTE FUNCTION refuse_reminder_insert();",
    )
    .execute(pool)
    .await
    .expect("install reservation fault in disposable fixture");
}

async fn refuse_result(pool: &PgPool) {
    sqlx::raw_sql(
        "CREATE FUNCTION refuse_reminder_insert() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'synthetic result audit failure'; END $$;
         CREATE TRIGGER fail_reminder_insert BEFORE INSERT ON automation_audit_log
         FOR EACH ROW WHEN (NEW.action = 'rsvp.reminder')
         EXECUTE FUNCTION refuse_reminder_insert();",
    )
    .execute(pool)
    .await
    .expect("install result-only fault in disposable fixture");
}

async fn restore_audit_writes(pool: &PgPool) {
    sqlx::raw_sql("DROP TRIGGER fail_reminder_insert ON automation_audit_log; DROP FUNCTION refuse_reminder_insert();")
        .execute(pool).await.expect("restore fixture audit writes");
}

#[tokio::test]
async fn reservation_failure_is_returned_as_database_error_before_any_post() {
    let Some(db) = fixture().await else { return };
    seed_one(db.pool()).await;
    refuse_attempt(db.pool()).await;
    let mock = success_mock().await;
    let result = tick(db.pool(), &executor(&mock), GUILD, || T0).await;
    assert!(
        matches!(result, Err(ErrorClass::Database)),
        "reservation failure must escape tick: {result:?}"
    );
    assert!(
        posts(&mock).is_empty(),
        "uncommitted attempt cannot authorize a POST"
    );
    assert!(audits(db.pool(), GUILD, E1).await.is_empty());
    restore_audit_writes(db.pool()).await;
    successful_tick(db.pool(), &executor(&mock), GUILD, T0).await;
    assert_eq!(
        posts(&mock).len(),
        1,
        "restored reservation positive control"
    );
    assert_outcome(
        &audits(db.pool(), GUILD, E1).await,
        GUILD,
        E1,
        "posted",
        None,
    );
    mock.shutdown().await;
    db.close().await.expect("verified teardown");
}

#[tokio::test]
async fn result_audit_failure_returns_database_error_but_committed_attempt_survives_restart() {
    let Some(db) = fixture().await else { return };
    seed_one(db.pool()).await;
    refuse_result(db.pool()).await;
    let mock = success_mock().await;
    let result = tick(db.pool(), &executor(&mock), GUILD, || T0).await;
    assert!(
        matches!(result, Err(ErrorClass::Database)),
        "result audit failure must escape tick: {result:?}"
    );
    assert_eq!(
        posts(&mock).len(),
        1,
        "wire effect happened before result-audit failure"
    );
    let peer = db
        .independent_pool()
        .await
        .expect("independent audit observer/restart");
    assert_eq!(
        audits(&peer, GUILD, E1).await,
        [pending_attempt(GUILD, E1)],
        "pre-wire fact committed independently of result"
    );
    restore_audit_writes(&peer).await;
    successful_tick(&peer, &executor(&mock), GUILD, T0 + 1).await;
    assert_eq!(
        posts(&mock).len(),
        1,
        "missing result must not cause resend"
    );
    assert_eq!(
        audits(&peer, GUILD, E1).await,
        [pending_attempt(GUILD, E1)],
        "do not fabricate a success outcome on restart"
    );
    peer.close().await;
    mock.shutdown().await;
    db.close().await.expect("verified teardown");
}
