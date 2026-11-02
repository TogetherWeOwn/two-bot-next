//! Protected test-author acceptance, included by the existing RSVP runtime target.
//! Exercises the real shared command/runtime/store/executor; Discord is loopback.

use super::*;
use two_bot_core::{RsvpRecord, RsvpStatus};

const FOREIGN_GUILD: &str = "1545644954272137001";
const OTHER_EVENT: &str = "1546451670500642001";
const THIRD: &str = "1546451670500642666";
const SAVED_AT: &str = "2020-01-02T03:04:05.000Z";
const SAVED_TAG: &str = "<t:1577934245:f>";

async fn fixture() -> Option<Fixture> {
    pool().await
}

async fn seed(
    pool: &Pool<Postgres>,
    guild: &str,
    event: &str,
    user: &str,
    status: RsvpStatus,
    at: &str,
) {
    two_bot_core::put_rsvp(
        pool,
        &RsvpRecord {
            guild_id: guild.into(),
            event_id: event.into(),
            user_id: user.into(),
            status,
            responded_at: at.into(),
        },
    )
    .await
    .expect("seed persisted RSVP");
}

fn totals(id: u64, event: &str) -> Interaction {
    slash(
        id,
        "rsvp-attendance",
        0,
        json!([{"name": "event-id", "type": 3, "value": event}]),
    )
}

/// Include MVCC versions: a same-value rewrite still violates read-only behavior.
/// The audit rows are also the persisted RSVP write-budget ledger.
async fn snapshot(pool: &Pool<Postgres>) -> (String, String, (i64, i64, i64)) {
    let rows = sqlx::query_scalar("SELECT COALESCE(jsonb_agg(to_jsonb(r) ORDER BY guild_id,event_id,user_id), '[]'::jsonb)::text FROM (SELECT *, xmin::text AS version FROM event_rsvps) r")
        .fetch_one(pool).await.expect("snapshot RSVP rows");
    let audits = sqlx::query_scalar("SELECT COALESCE(jsonb_agg(to_jsonb(a) ORDER BY id), '[]'::jsonb)::text FROM announcements_audit_log a")
        .fetch_one(pool).await.expect("snapshot audit and quota ledger");
    (rows, audits, counts(pool).await)
}

/// A read may acknowledge privately and edit that reply, but may not GET the
/// event/member or send to a channel/DM. Check controls before the RED UX oracle.
async fn read(pool: &Pool<Postgres>, interaction: &Interaction) -> String {
    let before = snapshot(pool).await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, json!({"id": "99"})),
        ],
        ScriptedResponse::status(404),
    )
    .await;
    run(pool, &mock, interaction).await;
    let requests = mock.requests();
    let after = snapshot(pool).await;
    mock.shutdown().await;
    assert_eq!(
        after, before,
        "totals must not rewrite rows, audit, facts or write budget"
    );
    assert_eq!(
        requests.len(),
        2,
        "private callback and original edit only; no live lookup"
    );
    assert_eq!(requests[0].method, "POST");
    assert!(requests[0].path.ends_with("/callback"));
    let ack: Value = serde_json::from_slice(&requests[0].body).expect("callback JSON");
    assert_eq!(ack["type"], 5);
    assert_eq!(
        ack["data"]["flags"], 64,
        "personal details must stay ephemeral"
    );
    assert_eq!(requests[1].method, "PATCH");
    assert_eq!(
        requests[1].path,
        "/api/v10/webhooks/1111/mock-rsvp-token/messages/@original"
    );
    let edit: Value = serde_json::from_slice(&requests[1].body).expect("reply JSON");
    assert_eq!(edit["allowed_mentions"]["parse"], json!([]));
    edit["content"].as_str().expect("text reply").to_owned()
}

fn assert_saved(text: &str, status: &str, timestamp: &str, totals: &str) {
    assert!(text.contains(totals), "aggregate counts changed: {text}");
    assert!(
        text.contains(&format!("Your saved response: {status}")),
        "missing invoking member's saved choice: {text}"
    );
    assert!(
        text.contains(timestamp),
        "missing persisted Discord-localized timestamp: {text}"
    );
    assert!(
        text.contains(&format!("event `{EVENT}`")),
        "missing stable event identity: {text}"
    );
    assert!(
        text.contains("live event availability is not checked"),
        "stored history must not claim live availability: {text}"
    );
    for user in [USER, OTHER, THIRD] {
        assert!(
            !text.contains(user),
            "personal response must not become an attendee roster: {text}"
        );
    }
    // A link is optional, but if present its authority must be the trusted pair.
    if text.contains("https://discord.com/events/") {
        assert!(text.contains(&format!("https://discord.com/events/{GUILD}/{EVENT}")));
    }
}

async fn saved_choice(status: RsvpStatus, label: &str, counts: &str) {
    let Some(fixture) = fixture().await else {
        return;
    };
    seed(fixture.pool(), GUILD, EVENT, USER, status, SAVED_AT).await;
    let text = read(fixture.pool(), &totals(9100, EVENT)).await;
    fixture.close().await;
    assert_saved(&text, label, SAVED_TAG, counts);
}

#[tokio::test]
async fn saved_going_shows_original_timestamp_without_refreshing_it() {
    saved_choice(
        RsvpStatus::Going,
        "Going",
        "Going: 1\nInterested: 0\nDeclined: 0",
    )
    .await;
}

#[tokio::test]
async fn saved_interested_is_not_rendered_as_going() {
    saved_choice(
        RsvpStatus::Interested,
        "Interested",
        "Going: 0\nInterested: 1\nDeclined: 0",
    )
    .await;
}

#[tokio::test]
async fn saved_decline_is_not_rendered_as_no_response() {
    saved_choice(
        RsvpStatus::Declined,
        "Declined",
        "Going: 0\nInterested: 0\nDeclined: 1",
    )
    .await;
}

#[tokio::test]
async fn personal_lookup_is_scoped_to_guild_event_and_invoker() {
    let Some(fixture) = fixture().await else {
        return;
    };
    // Different timestamps and choices make first-row/last-row selection wrong.
    for (guild, event, user, status, at) in [
        (
            GUILD,
            EVENT,
            OTHER,
            RsvpStatus::Going,
            "2019-01-01T00:00:00.000Z",
        ),
        (GUILD, EVENT, USER, RsvpStatus::Declined, SAVED_AT),
        (
            GUILD,
            EVENT,
            THIRD,
            RsvpStatus::Interested,
            "2021-01-01T00:00:00.000Z",
        ),
        (
            FOREIGN_GUILD,
            EVENT,
            USER,
            RsvpStatus::Going,
            "2022-01-01T00:00:00.000Z",
        ),
        (
            GUILD,
            OTHER_EVENT,
            USER,
            RsvpStatus::Interested,
            "2023-01-01T00:00:00.000Z",
        ),
    ] {
        seed(fixture.pool(), guild, event, user, status, at).await;
    }
    let text = read(fixture.pool(), &totals(9101, EVENT)).await;
    fixture.close().await;
    assert_saved(
        &text,
        "Declined",
        SAVED_TAG,
        "Going: 1\nInterested: 1\nDeclined: 1",
    );
    assert_eq!(
        text.matches("<t:").count(),
        1,
        "never show another member's timestamp: {text}"
    );
    for forbidden in [
        "1546300800",
        "1609459200",
        "1640995200",
        "1672531200",
        FOREIGN_GUILD,
        OTHER_EVENT,
    ] {
        assert!(
            !text.contains(forbidden),
            "cross-member/event/guild disclosure: {text}"
        );
    }
}

#[tokio::test]
async fn no_personal_row_does_not_borrow_another_member_or_guild_response() {
    let Some(fixture) = fixture().await else {
        return;
    };
    seed(
        fixture.pool(),
        GUILD,
        EVENT,
        OTHER,
        RsvpStatus::Going,
        SAVED_AT,
    )
    .await;
    seed(
        fixture.pool(),
        FOREIGN_GUILD,
        EVENT,
        USER,
        RsvpStatus::Declined,
        SAVED_AT,
    )
    .await;
    seed(
        fixture.pool(),
        GUILD,
        OTHER_EVENT,
        USER,
        RsvpStatus::Interested,
        SAVED_AT,
    )
    .await;
    let text = read(fixture.pool(), &totals(9102, EVENT)).await;
    fixture.close().await;
    assert!(
        text.contains("Going: 1\nInterested: 0\nDeclined: 0"),
        "{text}"
    );
    assert!(
        text.contains("You have no saved response for this event."),
        "absence must be explicit: {text}"
    );
    assert!(!text.contains("<t:"), "no other member's timestamp: {text}");
    assert!(!text.contains(OTHER), "no other member's identity: {text}");
    assert!(!text.contains("Your saved response:"), "{text}");
}

#[tokio::test]
async fn empty_event_has_zero_totals_and_explicit_personal_absence() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let text = read(fixture.pool(), &totals(9103, EVENT)).await;
    fixture.close().await;
    assert!(
        text.contains("Going: 0\nInterested: 0\nDeclined: 0"),
        "{text}"
    );
    assert!(
        text.contains("You have no saved response for this event."),
        "{text}"
    );
    assert!(text.contains(&format!("event `{EVENT}`")), "{text}");
    assert!(
        text.contains("live event availability is not checked"),
        "{text}"
    );
    assert!(
        !text.contains("<t:"),
        "absence has no saved timestamp: {text}"
    );
}

#[tokio::test]
async fn repeated_historical_reads_leave_a_full_write_budget_untouched() {
    let Some(fixture) = fixture().await else {
        return;
    };
    seed(
        fixture.pool(),
        GUILD,
        EVENT,
        USER,
        RsvpStatus::Declined,
        SAVED_AT,
    )
    .await;
    // A saturated write ledger must not prevent an existing historical read.
    sqlx::query("INSERT INTO announcements_audit_log (id,guild_id,actor_id,action,target_key,outcome,created_at) SELECT 'budget-' || n, $1,$2,'event.rsvp',$3,'declined',now() FROM generate_series(1,20) n")
        .bind(GUILD).bind(USER).bind(EVENT).execute(fixture.pool()).await.expect("seed full write budget");
    let first = read(fixture.pool(), &totals(9104, EVENT)).await;
    let second = read(fixture.pool(), &totals(9105, EVENT)).await;
    fixture.close().await;
    assert_eq!(
        first, second,
        "read must show persisted time, not the time of reading"
    );
    assert_saved(
        &first,
        "Declined",
        SAVED_TAG,
        "Going: 0\nInterested: 0\nDeclined: 1",
    );
}

#[tokio::test]
async fn analytics_opt_out_does_not_rewrite_transactional_rsvp_history() {
    let Some(fixture) = fixture().await else {
        return;
    };
    seed(
        fixture.pool(),
        GUILD,
        EVENT,
        USER,
        RsvpStatus::Interested,
        SAVED_AT,
    )
    .await;
    // Existing analytics opt-out deliberately does not erase transactional RSVPs.
    let db = two_bot_cutover::db::CutoverDb::from_pool(fixture.pool().clone());
    two_bot_cutover::privacy::opt_out_member(&db, GUILD, USER, "acceptance-test")
        .await
        .expect("apply real analytics opt-out");
    let text = read(fixture.pool(), &totals(9106, EVENT)).await;
    fixture.close().await;
    assert_saved(
        &text,
        "Interested",
        SAVED_TAG,
        "Going: 0\nInterested: 1\nDeclined: 0",
    );
}

#[tokio::test]
async fn full_erasure_removes_personal_detail_without_hiding_other_members_totals() {
    let Some(fixture) = fixture().await else {
        return;
    };
    seed(
        fixture.pool(),
        GUILD,
        EVENT,
        USER,
        RsvpStatus::Declined,
        SAVED_AT,
    )
    .await;
    seed(
        fixture.pool(),
        GUILD,
        EVENT,
        OTHER,
        RsvpStatus::Going,
        "2021-01-01T00:00:00.000Z",
    )
    .await;
    use two_bot_cutover::member_erasure::{erase_member, ErasureMode};
    erase_member(
        fixture.pool(),
        GUILD,
        USER,
        ErasureMode::Execute {
            actor: "acceptance-test",
        },
    )
    .await
    .expect("execute real member erasure in disposable fixture");
    let text = read(fixture.pool(), &totals(9107, EVENT)).await;
    fixture.close().await;
    assert!(
        text.contains("Going: 1\nInterested: 0\nDeclined: 0"),
        "{text}"
    );
    assert!(
        text.contains("You have no saved response for this event."),
        "{text}"
    );
    assert!(
        !text.contains("<t:"),
        "neither erased nor other member time may leak: {text}"
    );
    assert!(!text.contains(USER) && !text.contains(OTHER), "{text}");
}

#[tokio::test]
async fn full_event_still_finds_last_members_saved_response_with_bounded_reply() {
    let Some(fixture) = fixture().await else {
        return;
    };
    sqlx::query("INSERT INTO event_rsvps (guild_id,event_id,user_id,status,responded_at) SELECT $1,$2,'fixture-' || n,'going','2019-01-01T00:00:00Z'::timestamptz FROM generate_series(1,999) n")
        .bind(GUILD).bind(EVENT).execute(fixture.pool()).await.expect("seed full event");
    seed(
        fixture.pool(),
        GUILD,
        EVENT,
        USER,
        RsvpStatus::Declined,
        SAVED_AT,
    )
    .await;
    let text = read(fixture.pool(), &totals(9108, EVENT)).await;
    fixture.close().await;
    assert_saved(
        &text,
        "Declined",
        SAVED_TAG,
        "Going: 999\nInterested: 0\nDeclined: 1",
    );
    assert!(
        text.encode_utf16().count() <= 2000,
        "summary must remain sendable"
    );
    assert!(
        !text.contains("fixture-"),
        "bounded summary, never an attendee list"
    );
}

#[tokio::test]
async fn equivalent_offset_timestamp_uses_the_persisted_instant() {
    let Some(fixture) = fixture().await else {
        return;
    };
    seed(
        fixture.pool(),
        GUILD,
        EVENT,
        USER,
        RsvpStatus::Going,
        "2020-01-02T05:04:05.000+02:00",
    )
    .await;
    let text = read(fixture.pool(), &totals(9109, EVENT)).await;
    fixture.close().await;
    assert_saved(
        &text,
        "Going",
        SAVED_TAG,
        "Going: 1\nInterested: 0\nDeclined: 0",
    );
}

#[tokio::test]
async fn invalid_event_identifiers_refuse_without_link_or_personal_data() {
    let Some(fixture) = fixture().await else {
        return;
    };
    seed(
        fixture.pool(),
        GUILD,
        EVENT,
        USER,
        RsvpStatus::Going,
        SAVED_AT,
    )
    .await;
    let mut replies = Vec::new();
    for (i, event) in [
        "",
        "not-an-event",
        "00000000000000000000/../../other",
        "https://discord.com/events/999/123",
        "<@123>",
    ]
    .iter()
    .enumerate()
    {
        replies.push(read(fixture.pool(), &totals(9200 + i as u64, event)).await);
    }
    fixture.close().await;
    for text in replies {
        assert_eq!(text, "event id must be a Discord id.");
        assert!(!text.contains("https://") && !text.contains("<t:"));
    }
}

#[tokio::test]
async fn wrong_guild_and_dm_are_unhandled_before_any_callback_or_store_read() {
    let Some(fixture) = fixture().await else {
        return;
    };
    // A closed pool ensures any accidental store access fails, not just reads empty.
    fixture.pool().close().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    for guild in [
        Some(twilight_model::id::Id::new(FOREIGN_GUILD.parse().unwrap())),
        None,
    ] {
        let mut interaction = totals(9300, EVENT);
        interaction.guild_id = guild;
        assert!(!handle_rsvp_interaction(
            &router(),
            fixture.pool(),
            &executor(&mock),
            &ClassifierConfig::default(),
            &interaction
        )
        .await
        .expect("guild refusal before store or network"));
    }
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
    fixture.close().await;
}

#[tokio::test]
async fn store_failure_is_not_reported_as_no_saved_response_or_zero_totals() {
    let Some(fixture) = fixture().await else {
        return;
    };
    seed(
        fixture.pool(),
        GUILD,
        EVENT,
        USER,
        RsvpStatus::Going,
        SAVED_AT,
    )
    .await;
    // Rename instead of deleting data: the historical read cannot access its table.
    sqlx::query("ALTER TABLE event_rsvps RENAME TO unavailable_event_rsvps")
        .execute(fixture.pool())
        .await
        .expect("inject disposable read failure");
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, json!({"id": "99"})),
        ],
        ScriptedResponse::status(404),
    )
    .await;
    run(fixture.pool(), &mock, &totals(9400, EVENT)).await;
    let requests = mock.requests();
    let edit: Value =
        serde_json::from_slice(&requests.last().expect("failure reply").body).expect("JSON");
    let text = edit["content"]
        .as_str()
        .expect("failure content")
        .to_owned();
    mock.shutdown().await;
    sqlx::query("ALTER TABLE unavailable_event_rsvps RENAME TO event_rsvps")
        .execute(fixture.pool())
        .await
        .expect("restore fixture table");
    assert_eq!(counts(fixture.pool()).await, (1, 0, 0));
    fixture.close().await;
    assert_eq!(
        requests.len(),
        2,
        "failure must not fall back to live Discord"
    );
    let ack: Value = serde_json::from_slice(&requests[0].body).expect("callback");
    assert_eq!(ack["data"]["flags"], 64);
    assert_eq!(text, "Unable to read RSVP totals.");
    assert!(!text.contains("no saved response") && !text.contains("Going: 0"));
    assert!(!text.contains("<t:") && !text.contains(USER));
}

#[test]
fn published_totals_command_and_help_explain_personal_saved_response() {
    let defs = router().publish_set(&[]).expect("publish set");
    let command = defs
        .iter()
        .find(|d| d.name == "rsvp-attendance")
        .expect("existing command");
    assert_eq!(
        command.description,
        "Show RSVP totals and your saved response for an event"
    );
    assert!(
        two_bot_core::help_text(&defs)
            .to_lowercase()
            .contains("saved response"),
        "help must make the read discoverable without invoking a writer"
    );
}
