//! Acceptance tests: Helix subscription listings refuse duplicate JSON keys.
//!
//! PROTECTED: the paired builder must satisfy these tests without editing
//! this file. Do not merge this file on its own while it is red.
//!
//! Contract for `HelixClient::list_subscriptions` (`twitch_helix.rs:608`):
//! every listing page's 2xx body is decoded with `serde_json::from_slice`
//! into a `Value`, so repeated object keys silently last-win today and the
//! winning list feeds reconcile create/delete planning. After the fix,
//! repeated keys at any depth — conflicting values in both orders, identical
//! values, nested entry objects, escaped spellings of the same key, and
//! unknown keys, on any page — are refused with the existing
//! fixed-vocabulary `HelixError::BadResponse(&'static str)` whose message
//! contains `duplicate JSON object key` (no new variant needed). A refused
//! page fails the whole listing: no partial list is ever returned for
//! planning. The refusal renders no body bytes: neither `Display` nor `Debug`
//! may contain hostile keys, values, or bodies, and both stay bounded.
//! Unique-key listings behave byte-identically, and malformed envelopes keep
//! their existing non-duplicate `BadResponse`.
//!
//! Offline only: scripted [`HelixTransport`], no network, no env, no live
//! Twitch; credentials are fixed fixture strings.
//! RED on the baseline: every refusal test fails with `Ok(list)` where `Err`
//! is expected (silent last-wins); the controls pass.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use http::StatusCode;
use two_bot_core::twitch_eventsub::ExistingSubscription;
use two_bot_core::twitch_helix::{
    HelixClient, HelixCredentials, HelixError, HelixRequest, HelixResponse, HelixTransport,
};
use two_bot_core::Secret;

/// Fixed refusal vocabulary (Python dup-key precedent). The singular
/// substring also matches a plural message.
const DUP_TOKEN: &str = "duplicate JSON object key";
/// Marker embedded in hostile keys/values; refusals must never echo it.
const SENTINEL: &str = "DUPKEY-SENTINEL-9c4e1a";
/// Refusal renderings stay bounded: a static description, never body bytes.
const MAX_RENDERED: usize = 1024;

fn fixture_creds() -> HelixCredentials {
    HelixCredentials {
        client_id: Secret::new("fixture-client-id".to_owned()),
        client_secret: Secret::new("fixture-client-secret".to_owned()),
    }
}

fn entry(id: &str) -> String {
    format!(
        r#"{{"id":"{id}","status":"enabled","type":"stream.online","version":"1","condition":{{"broadcaster_user_id":"1337"}},"transport":{{"method":"webhook","callback":"https://hooks.example.com/twitch"}}}}"#
    )
}

fn token_answer() -> Result<HelixResponse, HelixError> {
    Ok(HelixResponse {
        status: StatusCode::OK,
        body: br#"{"access_token":"dupkey-fixture-token-01","expires_in":3600}"#.to_vec(),
    })
}

/// Scripted transport: canned answers in order, every request recorded.
struct ScriptTransport {
    answers: Mutex<VecDeque<Result<HelixResponse, HelixError>>>,
    calls: Mutex<Vec<HelixRequest>>,
}

impl ScriptTransport {
    fn new(answers: Vec<Result<HelixResponse, HelixError>>) -> Self {
        Self {
            answers: Mutex::new(answers.into()),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn ok_raw(body: Vec<u8>) -> Result<HelixResponse, HelixError> {
        Ok(HelixResponse {
            status: StatusCode::OK,
            body,
        })
    }

    fn call_count(&self) -> usize {
        self.calls.lock().expect("call log must unlock").len()
    }
}

impl HelixTransport for ScriptTransport {
    async fn send(&self, request: HelixRequest) -> Result<HelixResponse, HelixError> {
        self.calls
            .lock()
            .expect("call log must unlock")
            .push(request);
        self.answers
            .lock()
            .expect("script must unlock")
            .pop_front()
            .expect("script must answer every call")
    }
}

/// Wrapper so tests can read the call count after the client owns the
/// transport. The client takes the `Arc`; the test keeps its clone.
struct CountingTransport {
    inner: Arc<ScriptTransport>,
}

impl HelixTransport for CountingTransport {
    async fn send(&self, request: HelixRequest) -> Result<HelixResponse, HelixError> {
        self.inner.send(request).await
    }
}

fn client_for(
    answers: Vec<Result<HelixResponse, HelixError>>,
) -> (HelixClient<CountingTransport>, Arc<ScriptTransport>) {
    let transport = Arc::new(ScriptTransport::new(answers));
    let client = HelixClient::new(
        CountingTransport {
            inner: transport.clone(),
        },
        fixture_creds(),
    )
    .with_clock(|| 0);
    (client, transport)
}

/// Assert the listing refuses with the fixed-vocabulary error; return the
/// refusal `Display` so callers can pin one fixed error across fixtures.
fn assert_refused(error: &HelixError, label: &str) -> String {
    assert!(
        matches!(error, HelixError::BadResponse(_)),
        "{label}: duplicate keys must fail with the bounded BadResponse, got: {error:?}"
    );
    let display = format!("{error}");
    let debug = format!("{error:?}");
    assert!(
        display.contains(DUP_TOKEN),
        "{label}: refusal must carry the fixed vocabulary, got: {display}"
    );
    for rendered in [&display, &debug] {
        assert!(
            !rendered.contains(SENTINEL),
            "{label}: refusal echoes hostile bytes: {rendered}"
        );
        assert!(
            rendered.len() < MAX_RENDERED,
            "{label}: refusal is not bounded: {rendered}"
        );
    }
    display
}

#[tokio::test]
async fn listing_duplicate_data_is_refused_in_both_orders() {
    // Both winners are valid lists, so last-wins plans from one of them
    // today — a different reconcile input per order.
    let one_first = format!(
        r#"{{"data":[{}],"data":[{}]}}"#,
        entry("sub-1"),
        entry("sub-2")
    );
    let two_first = format!(
        r#"{{"data":[{}],"data":[{}]}}"#,
        entry("sub-2"),
        entry("sub-1")
    );
    let (mut client, _) = client_for(vec![
        token_answer(),
        ScriptTransport::ok_raw(one_first.into_bytes()),
    ]);
    let a = match client.list_subscriptions().await {
        Ok(_) => panic!("sub-1-first duplicate must be refused"),
        Err(error) => assert_refused(&error, "data duplicate, sub-1 first"),
    };
    let (mut client, _) = client_for(vec![
        token_answer(),
        ScriptTransport::ok_raw(two_first.into_bytes()),
    ]);
    let b = match client.list_subscriptions().await {
        Ok(_) => panic!("sub-2-first duplicate must be refused"),
        Err(error) => assert_refused(&error, "data duplicate, sub-2 first"),
    };
    assert_eq!(a, b, "both orders share one fixed refusal");
}

#[tokio::test]
async fn listing_identical_duplicate_is_refused() {
    let body = format!(
        r#"{{"data":[{}],"data":[{}]}}"#,
        entry("sub-9"),
        entry("sub-9")
    );
    let (mut client, _) = client_for(vec![
        token_answer(),
        ScriptTransport::ok_raw(body.into_bytes()),
    ]);
    match client.list_subscriptions().await {
        Ok(_) => panic!("identical duplicate must be refused"),
        Err(error) => {
            assert_refused(&error, "data repeated with the identical value");
        }
    }
}

#[tokio::test]
async fn listing_nested_entry_duplicate_is_refused() {
    // The per-entry parser is lenient, so a nested duplicate still lists
    // today; refusal is decoder-level, any depth.
    let nested = r#"{"id":"sub-1","id":"sub-2","status":"enabled","type":"stream.online","version":"1","condition":{"broadcaster_user_id":"1337"},"transport":{"method":"webhook","callback":"https://hooks.example.com/twitch"}}"#;
    let body = format!(r#"{{"data":[{nested}]}}"#);
    let (mut client, _) = client_for(vec![
        token_answer(),
        ScriptTransport::ok_raw(body.into_bytes()),
    ]);
    match client.list_subscriptions().await {
        Ok(_) => panic!("nested duplicate must be refused"),
        Err(error) => {
            assert_refused(&error, "nested duplicate entry id");
        }
    }
}

#[tokio::test]
async fn listing_escaped_key_spelling_counts_as_duplicate() {
    // `\u0064` decodes to `d`: same key twice after decoding.
    let body = format!(
        "{{\"data\":[{}],\"\\u0064ata\":[{}]}}",
        entry("sub-1"),
        entry("sub-2")
    );
    let (mut client, _) = client_for(vec![
        token_answer(),
        ScriptTransport::ok_raw(body.into_bytes()),
    ]);
    match client.list_subscriptions().await {
        Ok(_) => panic!("escaped duplicate must be refused"),
        Err(error) => {
            assert_refused(&error, "escaped spelling of data");
        }
    }
}

#[tokio::test]
async fn listing_unknown_duplicate_key_is_refused_without_echo() {
    let body = format!(
        r#"{{"{SENTINEL}":1,"{SENTINEL}":2,"data":[{}]}}"#,
        entry("sub-9")
    );
    let (mut client, _) = client_for(vec![
        token_answer(),
        ScriptTransport::ok_raw(body.into_bytes()),
    ]);
    match client.list_subscriptions().await {
        Ok(_) => panic!("unknown duplicate must be refused"),
        Err(error) => {
            assert_refused(&error, "unknown key repeated");
        }
    }
}

#[tokio::test]
async fn duplicate_on_second_page_refuses_without_partial_success() {
    // A refused later page fails the whole listing: reconcile never plans
    // from a truncated page prefix.
    let page_one = format!(
        r#"{{"data":[{}],"pagination":{{"cursor":"c1"}}}}"#,
        entry("sub-1")
    );
    let page_two = format!(
        r#"{{"data":[{}],"data":[{}]}}"#,
        entry("sub-2"),
        entry("sub-3")
    );
    let (mut client, transport) = client_for(vec![
        token_answer(),
        ScriptTransport::ok_raw(page_one.into_bytes()),
        ScriptTransport::ok_raw(page_two.into_bytes()),
    ]);
    match client.list_subscriptions().await {
        Ok(_) => panic!("page-two duplicate must be refused"),
        Err(error) => {
            assert_refused(&error, "duplicate on the second page");
        }
    }
    assert_eq!(
        transport.call_count(),
        3,
        "token plus both pages, no retry past the refusal"
    );
}

#[tokio::test]
async fn every_duplicate_shape_shares_one_fixed_error() {
    let shapes: Vec<(&str, Vec<u8>)> = vec![
        (
            "conflicting",
            format!(r#"{{"data":[{}],"data":[{}]}}"#, entry("sub-1"), entry("sub-2")).into_bytes(),
        ),
        (
            "identical",
            format!(r#"{{"data":[{}],"data":[{}]}}"#, entry("sub-9"), entry("sub-9")).into_bytes(),
        ),
        (
            "nested",
            r#"{"data":[{"id":"sub-1","id":"sub-2","status":"enabled","type":"stream.online","version":"1","condition":{"broadcaster_user_id":"1337"},"transport":{"method":"webhook","callback":"https://hooks.example.com/twitch"}}]}"#
                .as_bytes()
                .to_vec(),
        ),
        (
            "escaped",
            format!(
                "{{\"data\":[{}],\"\\u0064ata\":[{}]}}",
                entry("sub-1"),
                entry("sub-2")
            )
            .into_bytes(),
        ),
        (
            "unknown-key",
            format!(
                r#"{{"{SENTINEL}":1,"{SENTINEL}":2,"data":[{}]}}"#,
                entry("sub-9")
            )
            .into_bytes(),
        ),
    ];
    let mut fixed: Option<String> = None;
    for (shape, body) in shapes {
        let (mut client, _) = client_for(vec![token_answer(), ScriptTransport::ok_raw(body)]);
        let error = match client.list_subscriptions().await {
            Ok(_) => panic!("{shape} duplicate must be refused"),
            Err(error) => error,
        };
        let display = assert_refused(&error, shape);
        match &fixed {
            None => fixed = Some(display),
            Some(first) => assert_eq!(
                *first, display,
                "{shape} refusal must equal the fixed error"
            ),
        }
    }
}

#[tokio::test]
async fn valid_listings_behave_byte_identically() {
    let (mut client, _) = client_for(vec![
        token_answer(),
        ScriptTransport::ok_raw(br#"{"data":[]}"#.to_vec()),
    ]);
    assert_eq!(
        client.list_subscriptions().await.expect("empty listing"),
        Vec::new()
    );

    let (mut client, _) = client_for(vec![
        token_answer(),
        ScriptTransport::ok_raw(format!(r#"{{"data":[{}]}}"#, entry("sub-9")).into_bytes()),
    ]);
    assert_eq!(
        client
            .list_subscriptions()
            .await
            .expect("one-entry listing"),
        vec![ExistingSubscription {
            id: "sub-9".to_owned(),
            status: "enabled".to_owned(),
            broadcaster_user_id: "1337".to_owned(),
            callback: "https://hooks.example.com/twitch".to_owned(),
        }]
    );

    // A malformed envelope keeps its existing non-duplicate error: refusal
    // never swallows a shape error.
    let (mut client, _) = client_for(vec![
        token_answer(),
        ScriptTransport::ok_raw(br#"{"data":{"leak":1}}"#.to_vec()),
    ]);
    match client.list_subscriptions().await {
        Ok(_) => panic!("malformed envelope must fail"),
        Err(error @ HelixError::BadResponse(_)) => {
            assert!(
                !format!("{error}").contains(DUP_TOKEN),
                "shape errors are not duplicate refusals: {error}"
            );
        }
        Err(error) => panic!("malformed envelope must fail bounded, got: {error:?}"),
    }
}
