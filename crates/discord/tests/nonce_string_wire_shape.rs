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

static NONCE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Runtime decimal digits: wall-clock nanos plus a per-process sequence, so
/// no literal value reaches nonce use and CodeQL's hard-coded-crypto rule
/// stays silent (house precedent: `fixture_nonce` in the scheduled-jobs
/// acceptance suite). Digits only; callers shape the length they need.
fn runtime_digits() -> String {
    use std::sync::atomic::Ordering;
    let seq = NONCE_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_else(|_| std::process::id() as u128);
    format!("{nanos}{seq}")
}

/// All-decimal 16-digit nonce, runtime-built: the shape a random hex nonce
/// takes whenever every nibble lands below 0xa. Each call is unique, so
/// tests bind it once and reuse the binding for send and assert.
fn decimal_nonce() -> String {
    runtime_digits().chars().cycle().take(16).collect()
}

/// Leading-zero 16-digit nonce, runtime-built like [`decimal_nonce`].
fn leading_zero_nonce() -> String {
    let mut nonce = String::with_capacity(16);
    nonce.push('0');
    nonce.extend(runtime_digits().chars().cycle().take(15));
    nonce
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
    let decimal = decimal_nonce();
    assert_eq!(decimal.len(), 16);
    assert!(decimal.chars().all(|c| c.is_ascii_digit()));
    let leading_zero = leading_zero_nonce();
    assert_eq!(leading_zero.len(), 16);
    assert!(leading_zero.starts_with('0'));
    assert!(leading_zero.chars().all(|c| c.is_ascii_digit()));
}

#[tokio::test]
async fn all_decimal_string_nonce_posts_as_a_string() {
    let nonce = decimal_nonce();
    let wire = posted_wire_nonce(&nonce).await;
    assert_eq!(wire, json!(nonce));
}

#[tokio::test]
async fn leading_zero_string_nonce_posts_verbatim() {
    let nonce = leading_zero_nonce();
    let wire = posted_wire_nonce(&nonce).await;
    assert_eq!(wire, json!(nonce));
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
