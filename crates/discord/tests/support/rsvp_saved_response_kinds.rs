//! Protected test-author acceptance: first, changed and repeated RSVP receipts.
//!
//! Exercises the real shared command/runtime/store/executor; Discord is loopback.
//! The live `/rsvp` path must render three distinct private success receipts
//! from the exact committed transition already returned by the writer:
//! - first response: legacy `RSVP saved: {status}.` byte-identical;
//! - changed response: `RSVP saved: {current} (changed from {previous}).`;
//! - repeated choice: `RSVP saved: {status} (saved again; time updated).`
//!
//! No pre-read, follow-up query, extra write/audit or new Discord request may
//! choose the copy: each success is one ack, two membership/event GET pairs and
//! one private edit, with exactly one RSVP row and one audit row. The repeat
//! explicitly confirms the saved time was updated (the writer rewrites
//! `responded_at` and audits every response) and never says no-op,
//! deduplicated, unchanged or duplicate. No receipt promises attendance, seats,
//! reminders or live Discord state. Failures keep existing sanitized refusals.

use super::*;
use two_bot_core::{RsvpRecord, RsvpStatus};

const OLD_AT: &str = "2020-01-02T03:04:05.000Z";

fn first_text(status: &str) -> String {
    format!("RSVP saved: {status}.")
}

fn changed_text(previous: &str, current: &str) -> String {
    format!("RSVP saved: {current} (changed from {previous}).")
}

fn repeat_text(status: &str) -> String {
    format!("RSVP saved: {status} (saved again; time updated).")
}

async fn seed(pool: &Pool<Postgres>, status: RsvpStatus) {
    two_bot_core::put_rsvp(
        pool,
        &RsvpRecord {
            guild_id: GUILD.into(),
            event_id: EVENT.into(),
            user_id: USER.into(),
            status,
            responded_at: OLD_AT.into(),
        },
    )
    .await
    .expect("seed persisted prior response");
}

fn success_script() -> Vec<ScriptedResponse> {
    vec![
        ScriptedResponse::status(204),
        member(USER),
        event(1),
        member(USER),
        event(1),
        ScriptedResponse::json(200, json!({"id": "99"})),
    ]
}

/// One live save through the real dispatcher: returns the private edit text.
/// Pins the private ack/edit shape and the exact six-request footprint (no
/// extra lookup solely to choose copy).
async fn live_save(pool: &Pool<Postgres>, id: u64, status: &str) -> String {
    let mock = MockRest::start(success_script(), ScriptedResponse::status(500)).await;
    run(pool, &mock, &rsvp(id, status)).await;
    let requests = mock.requests();
    mock.shutdown().await;
    assert_eq!(requests.len(), 6, "ack + 4 fence GETs + edit, nothing more");
    let ack: Value = serde_json::from_slice(&requests[0].body).expect("ack JSON");
    assert_eq!(requests[0].method, "POST");
    assert_eq!(ack["type"], 5);
    assert_eq!(ack["data"]["flags"], 64, "success stays ephemeral");
    for (i, path) in [
        (1, guild_member_path(USER)),
        (2, scheduled_event_path(EVENT)),
        (3, guild_member_path(USER)),
        (4, scheduled_event_path(EVENT)),
    ] {
        assert_eq!(requests[i].method, "GET", "fence read {i}");
        assert_eq!(requests[i].path, path, "fence read {i}");
    }
    assert_eq!(requests[5].method, "PATCH");
    assert_eq!(
        requests[5].path,
        "/api/v10/webhooks/1111/mock-rsvp-token/messages/@original"
    );
    let edit: Value = serde_json::from_slice(&requests[5].body).expect("edit JSON");
    assert_eq!(edit["allowed_mentions"]["parse"], json!([]));
    let text = edit["content"].as_str().expect("text receipt").to_owned();
    assert!(
        !text.contains(USER) && !text.contains(EVENT),
        "receipt names no snowflake: {text}"
    );
    text
}

fn assert_no_forbidden_copy(text: &str) {
    for forbidden in [
        "no-op",
        "noop",
        "dedupl",
        "unchanged",
        "duplicate",
        "already",
        "no change",
        "attendance",
        "seat",
        "reminder",
        "live event",
        "reserved",
        "reservation",
        "waitlist",
    ] {
        assert!(
            !text.to_lowercase().contains(forbidden),
            "receipt promises or mislabels: {text}"
        );
    }
}

async fn stored(pool: &Pool<Postgres>) -> (String, String) {
    sqlx::query_as(
        "SELECT status, responded_at::text FROM event_rsvps WHERE guild_id=$1 AND event_id=$2 AND user_id=$3",
    )
    .bind(GUILD)
    .bind(EVENT)
    .bind(USER)
    .fetch_one(pool)
    .await
    .expect("stored response")
}

#[tokio::test]
async fn first_responses_keep_legacy_sentence_for_all_statuses() {
    for (i, status) in ["going", "interested", "declined"].iter().enumerate() {
        let Some(fixture) = pool().await else {
            return;
        };
        let text = live_save(fixture.pool(), 9100 + i as u64, status).await;
        let row = stored(fixture.pool()).await;
        let count = counts(fixture.pool()).await;
        let audits: Vec<(String, String)> =
            sqlx::query_as("SELECT action, outcome FROM announcements_audit_log ORDER BY id")
                .fetch_all(fixture.pool())
                .await
                .unwrap();
        fixture.close().await;
        assert_eq!(text, first_text(status), "first {status}");
        assert_eq!(row.0, *status);
        assert_eq!(count, (1, 1, 0));
        assert_eq!(
            audits,
            vec![("event.rsvp".to_owned(), (*status).to_owned())]
        );
        assert_no_forbidden_copy(&text);
        assert!(
            !text.contains("changed from") && !text.contains("saved again"),
            "first carries no rewrite marker: {text}"
        );
    }
}

#[tokio::test]
async fn changed_responses_reject_old_generic_sentence_and_name_both_choices() {
    let pairs = [
        ("going", "interested"),
        ("going", "declined"),
        ("interested", "going"),
        ("interested", "declined"),
        ("declined", "going"),
        ("declined", "interested"),
    ];
    for (i, (previous, current)) in pairs.iter().enumerate() {
        let Some(fixture) = pool().await else {
            return;
        };
        seed(
            fixture.pool(),
            RsvpStatus::parse(previous).expect("seeded status"),
        )
        .await;
        let text = live_save(fixture.pool(), 9200 + i as u64, current).await;
        let row = stored(fixture.pool()).await;
        let count = counts(fixture.pool()).await;
        let audits: Vec<String> = sqlx::query_scalar("SELECT outcome FROM announcements_audit_log")
            .fetch_all(fixture.pool())
            .await
            .unwrap();
        fixture.close().await;
        // Mutant detector: the old generic sentence without the previous choice.
        assert_ne!(
            text,
            first_text(current),
            "changed {previous}->{current} renders old generic copy"
        );
        assert_eq!(text, changed_text(previous, current));
        assert_eq!(row.0, *current);
        assert_ne!(row.1, OLD_AT, "rewrite stamps a new saved time");
        assert_eq!(count, (1, 1, 0), "seeded row rewritten, one audit");
        assert_eq!(audits, vec![(*current).to_owned()]);
        assert_no_forbidden_copy(&text);
    }
}

#[tokio::test]
async fn repeated_choices_reject_old_generic_sentence_and_confirm_time_update() {
    for (i, status) in ["going", "interested", "declined"].iter().enumerate() {
        let Some(fixture) = pool().await else {
            return;
        };
        seed(
            fixture.pool(),
            RsvpStatus::parse(status).expect("seeded status"),
        )
        .await;
        let text = live_save(fixture.pool(), 9300 + i as u64, status).await;
        let row = stored(fixture.pool()).await;
        let count = counts(fixture.pool()).await;
        let audits: Vec<String> = sqlx::query_scalar("SELECT outcome FROM announcements_audit_log")
            .fetch_all(fixture.pool())
            .await
            .unwrap();
        fixture.close().await;
        // Mutant detector: the old generic sentence without the again marker.
        assert_ne!(
            text,
            first_text(status),
            "repeat {status} renders old generic copy"
        );
        assert_eq!(text, repeat_text(status));
        assert_eq!(row.0, *status);
        assert_ne!(row.1, OLD_AT, "repeat rewrites saved time");
        assert_eq!(count, (1, 1, 0), "repeat audits like any save");
        assert_eq!(audits, vec![(*status).to_owned()]);
        assert_no_forbidden_copy(&text);
        assert!(
            text.contains("time updated"),
            "repeat confirms time: {text}"
        );
    }
}

#[tokio::test]
async fn failure_never_renders_success_branch_control() {
    let Some(fixture) = pool().await else {
        return;
    };
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            member(USER),
            ScriptedResponse::status(404),
            ScriptedResponse::json(200, json!({"id": "99"})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    run(fixture.pool(), &mock, &rsvp(9400, "going")).await;
    let requests = mock.requests();
    let last = requests.last().expect("refusal edit");
    let body: Value = serde_json::from_slice(&last.body).expect("refusal JSON");
    let text = body["content"].as_str().expect("refusal text").to_owned();
    let count = counts(fixture.pool()).await;
    mock.shutdown().await;
    fixture.close().await;
    assert_eq!(
        text,
        "No scheduled event with that id exists in this server."
    );
    assert_eq!(count, (0, 0, 0));
    assert!(
        !text.contains("RSVP saved:"),
        "failure renders success: {text}"
    );
}
