//! Builder-owned pins: the pasted-event-link `/rsvp` receipt follows the
//! committed transition, not just the first-response sentence.
//!
//! The protected saved-response acceptance drives raw numeric ids and the
//! protected link acceptance only a first response, so neither notices if the
//! link branch fell back to the generic `RSVP saved: {status}. Event `id`.`
//! copy for a member who already has a saved response.

use super::*;
use two_bot_core::{put_rsvp, RsvpRecord, RsvpStatus};

fn link() -> String {
    format!("https://discord.com/events/{GUILD}/{EVENT}")
}

async fn seed(pool: &Pool<Postgres>, status: RsvpStatus) {
    put_rsvp(
        pool,
        &RsvpRecord {
            guild_id: GUILD.into(),
            event_id: EVENT.into(),
            user_id: USER.into(),
            status,
            responded_at: "2020-01-02T03:04:05.000Z".into(),
        },
    )
    .await
    .expect("seed persisted prior response");
}

/// One live `/rsvp` through the real dispatcher with a pasted link; returns the
/// private edit text. Same six-request footprint as a raw id: ack, two
/// membership/event fence pairs, one edit.
async fn link_save(pool: &Pool<Postgres>, status: &str) -> String {
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            member(USER),
            event(1),
            member(USER),
            event(1),
            ScriptedResponse::json(200, json!({"id": "99"})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let interaction = slash(
        76000,
        "rsvp",
        0,
        json!([
            {"name": "event-id", "type": 3, "value": link()},
            {"name": "status", "type": 3, "value": status}
        ]),
    );
    run(pool, &mock, &interaction).await;
    let requests = mock.requests();
    mock.shutdown().await;
    assert_eq!(requests.len(), 6, "a link adds no request to pick the copy");
    let edit: Value = serde_json::from_slice(&requests[5].body).expect("edit JSON");
    assert_eq!(edit["allowed_mentions"]["parse"], json!([]));
    edit["content"].as_str().expect("text receipt").to_owned()
}

#[tokio::test]
async fn pasted_link_first_response_keeps_legacy_sentence_with_event() {
    let Some(fixture) = pool().await else {
        return;
    };
    let text = link_save(fixture.pool(), "going").await;
    fixture.close().await;
    assert_eq!(text, format!("RSVP saved: going. Event `{EVENT}`."));
}

#[tokio::test]
async fn pasted_link_changed_response_names_both_choices_and_the_event() {
    let Some(fixture) = pool().await else {
        return;
    };
    seed(fixture.pool(), RsvpStatus::Going).await;
    let text = link_save(fixture.pool(), "interested").await;
    fixture.close().await;
    assert_eq!(
        text,
        format!("RSVP saved: interested (changed from going). Event `{EVENT}`.")
    );
}

#[tokio::test]
async fn pasted_link_repeated_choice_confirms_the_time_update_and_the_event() {
    let Some(fixture) = pool().await else {
        return;
    };
    seed(fixture.pool(), RsvpStatus::Declined).await;
    let text = link_save(fixture.pool(), "declined").await;
    fixture.close().await;
    assert_eq!(
        text,
        format!("RSVP saved: declined (saved again; time updated). Event `{EVENT}`.")
    );
}
