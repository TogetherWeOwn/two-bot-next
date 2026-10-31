//! Acceptance tests: a successful (2xx) channel-creation response is only
//! a receipt once it describes the channel that was actually requested.
//!
//! `create_room`, `create_companion` and `create_join_voice_channel` each
//! POST one create and deserialize the body. Before the returned channel
//! may be bound to a room reservation, moved into, persisted as owned, or
//! treated as deletable, the adapter must verify the receipt against the
//! request:
//!
//! - the channel id is present and non-zero;
//! - `guild_id` is present and equals the requested guild;
//! - the channel kind is the requested kind (voice for a room and for a
//!   Join channel, text for a companion);
//! - when the request named a parent/category, `parent_id` is present and
//!   equals it (a companion always names its category).
//!
//! A 2xx body that fails any of these checks — wrong or missing guild,
//! wrong kind, wrong or missing requested parent, zero id, malformed or
//! bodyless — is a semantically contradictory success: Discord may have
//! created something, so it is an uncertain mutation outcome
//! (`RoomHttpError::UnknownOutcome`), never proof of no effect and never
//! a usable channel. Exactly one POST is made per adapter invocation —
//! no retry inside the adapter. Downstream, `UnknownOutcome` already
//! means: do not bind, move, persist, delete, or refund the durable
//! create reservation (pinned by the bot acceptance controls).
//!
//! Legitimate optional-parent semantics are preserved: when no parent
//! was requested, a parentless receipt is valid. A receipt that carries
//! a parent although none was requested is deliberately NOT pinned
//! here; the builder documents the chosen behavior in the receipt
//! contract (see below).
//!
//! Builder contract (this file and the bot companion file are preserved
//! verbatim; see the manifest on the acceptance card):
//!
//! - Validate inside the three `RoomHttp` create adapters in
//!   `crates/discord/src/voice_rooms.rs`, after deserializing the body
//!   and before returning `Ok(channel)`. Any mismatch above maps to
//!   `RoomHttpError::UnknownOutcome` (not `InvalidRequest`/`Rejected`:
//!   the POST may have landed, and the worker's no-repost/no-refund
//!   posture keys on `UnknownOutcome`).
//! - Do not add a second POST, a reservation refund, or any new retry,
//!   release or reconciliation behavior. The downstream Join queue
//!   retry/adoption behavior stays exactly as it is today; document it
//!   as the separate known design gap in the receipt/uncertainty
//!   contract (docs/voice-rooms.md or the doc that owns it) instead of
//!   claiming it fixed.
//! - Existing fixture repair (not in a protected file): the
//!   `created_channel()` body in `voice_rooms_http_tests.rs` omits
//!   `parent_id` although `attributes()` requests parent 400, so under
//!   this contract it is a rejected receipt. Add `"parent_id": "400"`
//!   to that fixture so the pre-existing create tests keep exercising a
//!   valid receipt.
//! - Update the receipt/uncertainty contract documentation in the same
//!   PR: what is validated, that a contradictory 2xx is
//!   `UnknownOutcome`, and the Join retry/adoption gap note above.
//!
//! Tests named `control_` use only today's behavior and must stay green
//! before and after the change; every other test is RED on the baseline
//! (the adapter returns the contradictory channel as `Ok`).

use super::*;

fn receipt(id: &str, guild: Option<&str>, kind: u8, parent: Option<&str>) -> Value {
    let mut body = json!({ "id": id, "type": kind, "name": "receipt" });
    if let Some(guild) = guild {
        body["guild_id"] = json!(guild);
    }
    if let Some(parent) = parent {
        body["parent_id"] = json!(parent);
    }
    body
}

async fn room_result(
    body: Value,
    attributes: RoomChannelAttributes,
) -> (Result<Channel, RoomHttpError>, usize) {
    let mock = Mock::start(vec![response(201, body)]).await;
    let result = mock
        .api
        .create_room(100, "room", &attributes, || true)
        .await;
    let requests = mock.state.recorded.lock().unwrap().len();
    (result, requests)
}

fn companion_plan() -> TextChannelPlan {
    TextChannelPlan {
        room_id: 500,
        guild_id: 100,
        name: "voice-chat".to_owned(),
        category_id: 400,
        overwrites: Vec::new(),
        settings: two_bot_core::voice_text_channel::TextChannelSettings {
            enabled: true,
            configured_name: None,
            viewer_role_id: None,
        },
    }
}

async fn companion_result(body: Value) -> (Result<Channel, RoomHttpError>, usize) {
    let mock = Mock::start(vec![response(201, body)]).await;
    let result = mock
        .api
        .create_companion(&companion_plan(), 999, || true)
        .await;
    let requests = mock.state.recorded.lock().unwrap().len();
    (result, requests)
}

async fn join_result(
    body: Value,
    parent_id: Option<u64>,
) -> (Result<Channel, RoomHttpError>, usize) {
    let mock = Mock::start(vec![response(201, body)]).await;
    let result = mock
        .api
        .create_join_voice_channel(100, "join", parent_id, None, || true)
        .await;
    let requests = mock.state.recorded.lock().unwrap().len();
    (result, requests)
}

#[tokio::test]
async fn control_valid_room_receipt_is_accepted() {
    let (result, requests) =
        room_result(receipt("600", Some("100"), 2, Some("400")), attributes()).await;
    assert_eq!(result.unwrap().id.get(), 600);
    assert_eq!(requests, 1);
}

#[tokio::test]
async fn room_receipt_from_another_guild_is_an_unknown_outcome() {
    let (result, requests) =
        room_result(receipt("600", Some("101"), 2, Some("400")), attributes()).await;
    assert_eq!(result, Err(RoomHttpError::UnknownOutcome));
    assert_eq!(requests, 1, "exactly one POST; the adapter never retries");
}

#[tokio::test]
async fn room_receipt_without_a_guild_is_an_unknown_outcome() {
    let (result, requests) = room_result(receipt("600", None, 2, Some("400")), attributes()).await;
    assert_eq!(result, Err(RoomHttpError::UnknownOutcome));
    assert_eq!(requests, 1);
}

#[tokio::test]
async fn room_receipt_with_the_wrong_kind_is_an_unknown_outcome() {
    // Type 0 is a text channel; a room create asked for voice (type 2).
    let (result, requests) =
        room_result(receipt("600", Some("100"), 0, Some("400")), attributes()).await;
    assert_eq!(result, Err(RoomHttpError::UnknownOutcome));
    assert_eq!(requests, 1);
}

#[tokio::test]
async fn room_receipt_under_the_wrong_parent_is_an_unknown_outcome() {
    let (result, requests) =
        room_result(receipt("600", Some("100"), 2, Some("401")), attributes()).await;
    assert_eq!(result, Err(RoomHttpError::UnknownOutcome));
    assert_eq!(requests, 1);
}

#[tokio::test]
async fn room_receipt_missing_the_requested_parent_is_an_unknown_outcome() {
    let (result, requests) = room_result(receipt("600", Some("100"), 2, None), attributes()).await;
    assert_eq!(result, Err(RoomHttpError::UnknownOutcome));
    assert_eq!(requests, 1);
}

#[tokio::test]
async fn control_room_receipt_with_channel_id_zero_is_an_unknown_outcome() {
    let (result, requests) =
        room_result(receipt("0", Some("100"), 2, Some("400")), attributes()).await;
    assert_eq!(result, Err(RoomHttpError::UnknownOutcome));
    assert_eq!(requests, 1);
}

#[tokio::test]
async fn control_room_receipt_with_no_body_fields_is_an_unknown_outcome() {
    let (result, requests) = room_result(json!({}), attributes()).await;
    assert_eq!(result, Err(RoomHttpError::UnknownOutcome));
    assert_eq!(requests, 1);
}

#[tokio::test]
async fn control_parentless_room_receipt_is_valid_when_no_parent_was_requested() {
    let mut attributes = attributes();
    attributes.parent_id = None;
    let (result, requests) = room_result(receipt("600", Some("100"), 2, None), attributes).await;
    assert_eq!(result.unwrap().id.get(), 600);
    assert_eq!(requests, 1);
}

#[tokio::test]
async fn control_valid_companion_receipt_is_accepted() {
    let (result, requests) = companion_result(receipt("700", Some("100"), 0, Some("400"))).await;
    assert_eq!(result.unwrap().id.get(), 700);
    assert_eq!(requests, 1);
}

#[tokio::test]
async fn companion_receipt_from_another_guild_is_an_unknown_outcome() {
    let (result, requests) = companion_result(receipt("700", Some("101"), 0, Some("400"))).await;
    assert_eq!(result, Err(RoomHttpError::UnknownOutcome));
    assert_eq!(requests, 1);
}

#[tokio::test]
async fn companion_receipt_without_a_guild_is_an_unknown_outcome() {
    let (result, requests) = companion_result(receipt("700", None, 0, Some("400"))).await;
    assert_eq!(result, Err(RoomHttpError::UnknownOutcome));
    assert_eq!(requests, 1);
}

#[tokio::test]
async fn companion_receipt_with_the_wrong_kind_is_an_unknown_outcome() {
    // Type 2 is a voice channel; a companion create asked for text (type 0).
    let (result, requests) = companion_result(receipt("700", Some("100"), 2, Some("400"))).await;
    assert_eq!(result, Err(RoomHttpError::UnknownOutcome));
    assert_eq!(requests, 1);
}

#[tokio::test]
async fn companion_receipt_under_the_wrong_category_is_an_unknown_outcome() {
    let (result, requests) = companion_result(receipt("700", Some("100"), 0, Some("401"))).await;
    assert_eq!(result, Err(RoomHttpError::UnknownOutcome));
    assert_eq!(requests, 1);
}

#[tokio::test]
async fn companion_receipt_missing_its_category_is_an_unknown_outcome() {
    let (result, requests) = companion_result(receipt("700", Some("100"), 0, None)).await;
    assert_eq!(result, Err(RoomHttpError::UnknownOutcome));
    assert_eq!(requests, 1);
}

#[tokio::test]
async fn control_companion_receipt_with_channel_id_zero_is_an_unknown_outcome() {
    let (result, requests) = companion_result(receipt("0", Some("100"), 0, Some("400"))).await;
    assert_eq!(result, Err(RoomHttpError::UnknownOutcome));
    assert_eq!(requests, 1);
}

#[tokio::test]
async fn control_valid_join_receipt_is_accepted() {
    let (result, requests) =
        join_result(receipt("800", Some("100"), 2, Some("400")), Some(400)).await;
    assert_eq!(result.unwrap().id.get(), 800);
    assert_eq!(requests, 1);
}

#[tokio::test]
async fn join_receipt_from_another_guild_is_an_unknown_outcome() {
    let (result, requests) =
        join_result(receipt("800", Some("101"), 2, Some("400")), Some(400)).await;
    assert_eq!(result, Err(RoomHttpError::UnknownOutcome));
    assert_eq!(requests, 1);
}

#[tokio::test]
async fn join_receipt_without_a_guild_is_an_unknown_outcome() {
    let (result, requests) = join_result(receipt("800", None, 2, Some("400")), Some(400)).await;
    assert_eq!(result, Err(RoomHttpError::UnknownOutcome));
    assert_eq!(requests, 1);
}

#[tokio::test]
async fn join_receipt_with_the_wrong_kind_is_an_unknown_outcome() {
    let (result, requests) =
        join_result(receipt("800", Some("100"), 0, Some("400")), Some(400)).await;
    assert_eq!(result, Err(RoomHttpError::UnknownOutcome));
    assert_eq!(requests, 1);
}

#[tokio::test]
async fn join_receipt_under_the_wrong_parent_is_an_unknown_outcome() {
    let (result, requests) =
        join_result(receipt("800", Some("100"), 2, Some("401")), Some(400)).await;
    assert_eq!(result, Err(RoomHttpError::UnknownOutcome));
    assert_eq!(requests, 1);
}

#[tokio::test]
async fn join_receipt_missing_the_requested_parent_is_an_unknown_outcome() {
    let (result, requests) = join_result(receipt("800", Some("100"), 2, None), Some(400)).await;
    assert_eq!(result, Err(RoomHttpError::UnknownOutcome));
    assert_eq!(requests, 1);
}

#[tokio::test]
async fn control_join_receipt_with_channel_id_zero_is_an_unknown_outcome() {
    let (result, requests) =
        join_result(receipt("0", Some("100"), 2, Some("400")), Some(400)).await;
    assert_eq!(result, Err(RoomHttpError::UnknownOutcome));
    assert_eq!(requests, 1);
}

#[tokio::test]
async fn control_join_receipt_with_no_body_fields_is_an_unknown_outcome() {
    let (result, requests) = join_result(json!({}), Some(400)).await;
    assert_eq!(result, Err(RoomHttpError::UnknownOutcome));
    assert_eq!(requests, 1);
}

#[tokio::test]
async fn control_parentless_join_receipt_is_valid_when_no_parent_was_requested() {
    let (result, requests) = join_result(receipt("800", Some("100"), 2, None), None).await;
    assert_eq!(result.unwrap().id.get(), 800);
    assert_eq!(requests, 1);
}
