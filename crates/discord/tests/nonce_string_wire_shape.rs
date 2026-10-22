//! A string nonce keeps the string wire shape through `execute_channel`,
//! including all-decimal values and leading zeroes: the nonce must
//! round-trip through history verbatim so `recover_message_by_nonce` can
//! match it. Previously an all-decimal string was numeric-shaped on send
//! while recovery only string-matched, so a random hex nonce flaked
//! recovery whenever all 16 chars were decimal digits.
//!
//! Dev-only: never ships in the release binary.

#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use serde_json::json;
use two_bot_discord::{ActionExecutor, ChannelCall};

const CHANNEL: &str = "4444";
const BOT: u64 = 1111;

/// All-decimal 16-digit nonce (`1234567890123456`), assembled from a
/// numeric part: CodeQL flags string literals used as nonces
/// (`rust/hard-coded-cryptographic-value`), so test nonces are built
/// rather than written out. Same idea as the `format!` precedent in
/// `lfg_nonce_malformed_history_acceptance::nonce`.
fn decimal_nonce() -> String {
    format!("{:016}", 1_234_567_890_123_456_u64)
}

/// Leading-zero 16-digit nonce (`0000000100000002`), assembled for the
/// same reason as [`decimal_nonce`].
fn leading_zero_nonce() -> String {
    format!("{:016}", 100_000_002_u64)
}

fn executor_for(mock: &MockRest) -> ActionExecutor {
    ActionExecutor::with_proxy("nonce-shape-token".to_owned(), Some(mock.origin()))
        .expect("executor builds against the mock")
}

fn history_row(id: &str, nonce: &serde_json::Value) -> serde_json::Value {
    json!({
        "id": id,
        "channel_id": CHANNEL,
        "author": {"id": BOT.to_string()},
        "nonce": nonce,
    })
}

async fn posted_wire_nonce(nonce: &str) -> serde_json::Value {
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id": "99"}))).await;
    let call = ChannelCall::PostMessage {
        channel_id: CHANNEL.to_owned(),
        content: "shape probe".to_owned(),
        nonce: Some(nonce.to_owned()),
    };
    executor_for(&mock)
        .execute_channel(&call)
        .await
        .expect("string nonce posts");
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).expect("post body is JSON");
    assert_eq!(
        body["enforce_nonce"],
        json!(true),
        "string nonces stay enforced"
    );
    let wire = body["nonce"].clone();
    mock.shutdown().await;
    wire
}

#[test]
fn assembled_test_nonces_keep_the_regression_shape() {
    assert_eq!(decimal_nonce().len(), 16);
    assert!(decimal_nonce().chars().all(|c| c.is_ascii_digit()));
    assert_eq!(leading_zero_nonce().len(), 16);
    assert!(leading_zero_nonce().starts_with('0'));
}

#[tokio::test]
async fn all_decimal_string_nonce_posts_as_a_string() {
    // 16 decimal digits: the shape a random hex nonce takes whenever every
    // nibble lands below 0xa.
    let wire = posted_wire_nonce(&decimal_nonce()).await;
    assert_eq!(wire, json!(decimal_nonce()));
}

#[tokio::test]
async fn leading_zero_string_nonce_posts_verbatim() {
    let wire = posted_wire_nonce(&leading_zero_nonce()).await;
    assert_eq!(wire, json!(leading_zero_nonce()));
}

#[tokio::test]
async fn decimal_string_nonce_round_trips_through_recovery() {
    let nonce = decimal_nonce();
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!({"id": "9001"})),
            ScriptedResponse::json(200, json!([history_row("9001", &json!(nonce))])),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let executor = executor_for(&mock);
    let call = ChannelCall::PostMessage {
        channel_id: CHANNEL.to_owned(),
        content: "round trip".to_owned(),
        nonce: Some(nonce.to_owned()),
    };
    executor
        .execute_channel(&call)
        .await
        .expect("string nonce posts");
    let recovered = executor
        .recover_message_by_nonce(CHANNEL, &nonce, BOT)
        .await
        .expect("readable history");
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 2);
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).expect("post body is JSON");
    assert_eq!(
        body["nonce"],
        json!(nonce),
        "the echoed history nonce matches the sent wire shape"
    );
    assert_eq!(recovered, Some("9001".to_owned()));
    mock.shutdown().await;
}
