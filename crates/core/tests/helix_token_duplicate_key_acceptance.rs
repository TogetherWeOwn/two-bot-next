//! Acceptance tests: Helix token refresh refuses duplicate JSON object keys.
//!
//! PROTECTED: the paired builder must satisfy these tests without editing
//! this file. Do not merge this file on its own while it is red.
//!
//! Contract for `HelixClient::refresh_token` (`twitch_helix.rs:530`): the
//! token endpoint's 2xx body is decoded with `serde_json::from_slice` into a
//! `Value`, so repeated object keys silently last-win today and the winning
//! expiry gates all Helix auth. After the fix, repeated keys at any depth —
//! conflicting values in both orders, identical values, nested objects,
//! escaped spellings of the same key, and unknown keys — are refused with the
//! existing fixed-vocabulary `HelixError::BadResponse(&'static str)` whose
//! message contains `duplicate JSON object key` (no new variant needed).
//! A refused body stores no token and clobbers no cached token, so no auth
//! gating decision can rest on it. The refusal renders no body bytes:
//! neither `Display` nor `Debug` may contain hostile keys, values, or bodies,
//! and both stay bounded. Unique-key bodies parse byte-identically, and a
//! syntactically invalid body keeps its existing non-duplicate `BadResponse`.
//!
//! Offline only: scripted [`HelixTransport`], no network, no env, no live
//! Twitch; credentials are fixed fixture strings. The `Value`-level
//! `parse_token_response` seam cannot see duplicates (the decoder already
//! collapsed them), so these tests pin the refusal at the client call.
//! RED on the baseline: every refusal test fails with `Ok(())` where `Err`
//! is expected (silent last-wins); the controls pass.

use std::collections::VecDeque;
use std::sync::Mutex;

use http::StatusCode;
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
/// Fixture bearer value (clearly synthetic; printable ASCII, under the cap).
const FIXTURE_TOKEN: &str = "dupkey-fixture-token-01";

fn fixture_creds() -> HelixCredentials {
    HelixCredentials {
        client_id: Secret::new("fixture-client-id".to_owned()),
        client_secret: Secret::new("fixture-client-secret".to_owned()),
    }
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
    inner: std::sync::Arc<ScriptTransport>,
}

impl HelixTransport for CountingTransport {
    async fn send(&self, request: HelixRequest) -> Result<HelixResponse, HelixError> {
        self.inner.send(request).await
    }
}

fn client_for(
    answers: Vec<Result<HelixResponse, HelixError>>,
) -> (
    HelixClient<CountingTransport>,
    std::sync::Arc<ScriptTransport>,
) {
    let transport = std::sync::Arc::new(ScriptTransport::new(answers));
    let client = HelixClient::new(
        CountingTransport {
            inner: transport.clone(),
        },
        fixture_creds(),
    )
    .with_clock(|| 0);
    (client, transport)
}

/// Assert the refresh refuses with the fixed-vocabulary error; return the
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

fn valid_body() -> Vec<u8> {
    format!(r#"{{"access_token":"{FIXTURE_TOKEN}","expires_in":3600,"token_type":"bearer"}}"#)
        .into_bytes()
}

#[tokio::test]
async fn token_duplicate_access_token_is_refused_in_both_orders() {
    // Both winners are valid-shaped, so last-wins refreshes successfully
    // today — with a different stored token per order.
    let sentinel_first = format!(
        r#"{{"access_token":"{SENTINEL}","access_token":"{FIXTURE_TOKEN}","expires_in":3600}}"#
    );
    let valid_first = format!(
        r#"{{"access_token":"{FIXTURE_TOKEN}","access_token":"{SENTINEL}","expires_in":3600}}"#
    );
    let (mut client, _) = client_for(vec![ScriptTransport::ok_raw(sentinel_first.into_bytes())]);
    let first = client.refresh_token().await;
    let a = match first {
        Ok(()) => panic!("sentinel-first duplicate must be refused, refresh succeeded"),
        Err(error) => assert_refused(&error, "access_token duplicate, sentinel first"),
    };
    let (mut client, _) = client_for(vec![ScriptTransport::ok_raw(valid_first.into_bytes())]);
    let second = client.refresh_token().await;
    let b = match second {
        Ok(()) => panic!("valid-first duplicate must be refused, refresh succeeded"),
        Err(error) => assert_refused(&error, "access_token duplicate, sentinel second"),
    };
    assert_eq!(a, b, "both orders share one fixed refusal");
}

#[tokio::test]
async fn token_identical_duplicate_is_refused() {
    let body = format!(
        r#"{{"access_token":"{FIXTURE_TOKEN}","access_token":"{FIXTURE_TOKEN}","expires_in":3600}}"#
    );
    let (mut client, _) = client_for(vec![ScriptTransport::ok_raw(body.into_bytes())]);
    match client.refresh_token().await {
        Ok(()) => panic!("identical duplicate must be refused, refresh succeeded"),
        Err(error) => {
            assert_refused(&error, "access_token repeated with the identical value");
        }
    }
}

#[tokio::test]
async fn token_nested_duplicate_in_unknown_object_is_refused() {
    // Unknown fields stay ignored for forward compatibility, but a repeated
    // key inside one is still refused: refusal is decoder-level, any depth.
    let body = format!(
        r#"{{"access_token":"{FIXTURE_TOKEN}","expires_in":3600,"extra":{{"x":1,"x":2}}}}"#
    );
    let (mut client, _) = client_for(vec![ScriptTransport::ok_raw(body.into_bytes())]);
    match client.refresh_token().await {
        Ok(()) => panic!("nested duplicate must be refused, refresh succeeded"),
        Err(error) => {
            assert_refused(&error, "nested duplicate inside unknown object");
        }
    }
}

#[tokio::test]
async fn token_escaped_key_spelling_counts_as_duplicate() {
    // `\u0061` decodes to `a`: same key twice after decoding.
    let body = format!(
        "{{\"access_token\":\"{FIXTURE_TOKEN}\",\"\\u0061ccess_token\":\"{SENTINEL}\",\"expires_in\":3600}}"
    );
    let (mut client, _) = client_for(vec![ScriptTransport::ok_raw(body.into_bytes())]);
    match client.refresh_token().await {
        Ok(()) => panic!("escaped duplicate must be refused, refresh succeeded"),
        Err(error) => {
            assert_refused(&error, "escaped spelling of access_token");
        }
    }
}

#[tokio::test]
async fn token_unknown_duplicate_key_is_refused_without_echo() {
    let body = format!(
        r#"{{"{SENTINEL}":1,"{SENTINEL}":2,"access_token":"{FIXTURE_TOKEN}","expires_in":3600}}"#
    );
    let (mut client, _) = client_for(vec![ScriptTransport::ok_raw(body.into_bytes())]);
    match client.refresh_token().await {
        Ok(()) => panic!("unknown duplicate must be refused, refresh succeeded"),
        Err(error) => {
            assert_refused(&error, "unknown key repeated");
        }
    }
}

#[tokio::test]
async fn every_duplicate_shape_shares_one_fixed_error() {
    let shapes: Vec<(&str, Vec<u8>)> = vec![
        (
            "conflicting",
            format!(
                r#"{{"access_token":"{SENTINEL}","access_token":"{FIXTURE_TOKEN}","expires_in":3600}}"#
            )
            .into_bytes(),
        ),
        (
            "identical",
            format!(
                r#"{{"access_token":"{FIXTURE_TOKEN}","access_token":"{FIXTURE_TOKEN}","expires_in":3600}}"#
            )
            .into_bytes(),
        ),
        (
            "nested",
            format!(
                r#"{{"access_token":"{FIXTURE_TOKEN}","expires_in":3600,"extra":{{"x":1,"x":2}}}}"#
            )
            .into_bytes(),
        ),
        (
            "escaped",
            format!(
                "{{\"access_token\":\"{FIXTURE_TOKEN}\",\"\\u0061ccess_token\":\"{SENTINEL}\",\"expires_in\":3600}}"
            )
            .into_bytes(),
        ),
        (
            "unknown-key",
            format!(
                r#"{{"{SENTINEL}":1,"{SENTINEL}":2,"access_token":"{FIXTURE_TOKEN}","expires_in":3600}}"#
            )
            .into_bytes(),
        ),
    ];
    let mut fixed: Option<String> = None;
    for (shape, body) in shapes {
        let (mut client, _) = client_for(vec![ScriptTransport::ok_raw(body)]);
        let error = match client.refresh_token().await {
            Ok(()) => panic!("{shape} duplicate must be refused, refresh succeeded"),
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
async fn refused_body_stores_no_token() {
    // A refused refresh leaves the cache empty: the next gated call fails
    // instead of resting on poisoned bytes. Two scripted answers because the
    // gated call retries the refresh once through the same transport.
    let body = format!(
        r#"{{"access_token":"{SENTINEL}","access_token":"{FIXTURE_TOKEN}","expires_in":3600}}"#
    )
    .into_bytes();
    let (mut client, transport) = client_for(vec![
        ScriptTransport::ok_raw(body.clone()),
        ScriptTransport::ok_raw(body),
    ]);
    match client.refresh_token().await {
        Ok(()) => panic!("duplicate body must be refused, refresh succeeded"),
        Err(error) => {
            assert_refused(&error, "refused refresh");
        }
    }
    let gated = client.app_token().await;
    assert!(
        gated.is_err(),
        "no usable token may survive a refused refresh"
    );
    assert_eq!(
        transport.call_count(),
        2,
        "exactly the refused refresh and the one gated retry"
    );
}

#[tokio::test]
async fn refused_body_does_not_clobber_a_cached_token() {
    // Seed a valid cached token, then refuse a duplicate body: the cached
    // token stays usable and no re-fetch happens.
    let poison = format!(
        r#"{{"access_token":"{SENTINEL}","access_token":"{FIXTURE_TOKEN}","expires_in":3600}}"#
    )
    .into_bytes();
    let (mut client, transport) = client_for(vec![
        ScriptTransport::ok_raw(valid_body()),
        ScriptTransport::ok_raw(poison),
    ]);
    client.refresh_token().await.expect("seed refresh succeeds");
    match client.refresh_token().await {
        Ok(()) => panic!("duplicate body must be refused, refresh succeeded"),
        Err(error) => {
            assert_refused(&error, "refused refresh over a cached token");
        }
    }
    let token = client
        .app_token()
        .await
        .expect("cached token survives the refusal");
    assert_eq!(
        token.expose().as_str(),
        FIXTURE_TOKEN,
        "the cached token is byte-identical, not poisoned"
    );
    assert_eq!(
        transport.call_count(),
        2,
        "seed refresh plus refused refresh; the gated call serves cache"
    );
}

#[tokio::test]
async fn valid_token_response_parses_byte_identically() {
    let (mut client, transport) = client_for(vec![ScriptTransport::ok_raw(valid_body())]);
    client.refresh_token().await.expect("valid body refreshes");
    let token = client.app_token().await.expect("valid token is gated");
    assert_eq!(token.expose().as_str(), FIXTURE_TOKEN);
    assert_eq!(transport.call_count(), 1);

    // A syntactically invalid body keeps its existing non-duplicate error:
    // refusal never swallows a syntax error.
    let (mut client, _) = client_for(vec![ScriptTransport::ok_raw(b"not json".to_vec())]);
    match client.refresh_token().await {
        Ok(()) => panic!("invalid JSON must fail the refresh"),
        Err(error @ HelixError::BadResponse(_)) => {
            assert!(
                !format!("{error}").contains(DUP_TOKEN),
                "syntax errors are not duplicate refusals: {error}"
            );
        }
        Err(error) => panic!("invalid JSON must fail bounded, got: {error:?}"),
    }
}
