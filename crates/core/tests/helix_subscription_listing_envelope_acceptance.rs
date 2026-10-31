//! Acceptance tests: reject malformed successful Helix subscription listings.
//!
//! PROTECTED: the paired builder must satisfy these tests without editing
//! this file. Do not merge this file on its own while it is red.
//!
//! Contract: a 2xx Helix answer whose top level is not an object carrying a
//! `data` array is not evidence of an empty subscription list. Today `{}` and
//! every non-array `data` shape are read as "no subscriptions", so reconcile
//! can plan creates (or miss deletes) from invalid evidence, and a malformed
//! later page silently truncates an otherwise valid multipage listing into
//! partial success. The listing call must instead fail with a bounded,
//! non-payload `HelixError` (the existing `BadResponse` variant: a static
//! description, never bytes from the response).
//!
//! The behavioral locus is `HelixClient::list_subscriptions` over a scripted
//! [`HelixTransport`] (offline: no network, no credentials beyond fixtures,
//! no live Twitch). That is the only listing entry point the bot reconcile
//! path uses, and it already propagates a listing error before planning and
//! before reporting healthy, so these tests pin the envelope contract there
//! rather than the lenient per-entry parser: the builder may validate the
//! envelope in the list path and keep per-entry filtering (other types,
//! versions, transports and malformed entries are skipped) unchanged.
//!
//! Preserved behavior pinned as regression tests: valid empty listings,
//! valid multipage accumulation with per-entry filtering, the repeated-
//! cursor refusal and the page-limit bound.

use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex, OnceLock,
};
use std::time::{SystemTime, UNIX_EPOCH};

use http::{Method, StatusCode};
use serde_json::{json, Value};
use two_bot_core::twitch_eventsub::{CallbackUrl, SubscriptionAction, WebhookSecret};
use two_bot_core::twitch_helix::{
    plan_boot_actions, ActionOutcome, HelixClient, HelixCredentials, HelixError, HelixRequest,
    HelixResponse, HelixTransport, HELIX_MAX_SUBSCRIPTION_PAGES, HELIX_SUBSCRIPTIONS_URL,
    TOKEN_URL,
};
use two_bot_core::Secret;

/// The bot's own callback fixture; only subscriptions on this URL are ours.
const OUR_CALLBACK: &str = "https://hooks.example.com/twitch";
/// Marker embedded in malformed payloads; errors must never echo it.
const PAYLOAD_SENTINEL: &str = "PAYLOAD-SENTINEL-7f3a9c";

static NEXT_TEST_SECRET: AtomicU64 = AtomicU64::new(1);

/// Runtime-generated signing secret, so no literal key material is committed.
fn test_secret() -> WebhookSecret {
    static SECRET: OnceLock<String> = OnceLock::new();
    let value = SECRET.get_or_init(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time is after the Unix epoch")
            .as_nanos();
        let nonce = NEXT_TEST_SECRET.fetch_add(1, Ordering::Relaxed);
        format!("synthetic-webhook-secret-{nanos:032x}-{nonce:016x}")
    });
    WebhookSecret::new(value.clone()).expect("generated secret must be valid")
}

fn callback() -> CallbackUrl {
    CallbackUrl::new(OUR_CALLBACK).expect("fixture callback must parse")
}

fn fixture_creds() -> HelixCredentials {
    HelixCredentials {
        client_id: Secret::new("fixture-client-id".to_owned()),
        client_secret: Secret::new("fixture-client-secret".to_owned()),
    }
}

fn subscription_entry(id: &str, status: &str, broadcaster: &str, callback: &str) -> Value {
    json!({
        "id": id,
        "status": status,
        "type": "stream.online",
        "version": "1",
        "condition": {"broadcaster_user_id": broadcaster},
        "transport": {"method": "webhook", "callback": callback},
    })
}

/// Shared request log, cloned before the client takes ownership of the
/// transport, so tests can count mutation calls after the run.
#[derive(Clone, Default)]
struct CallLog {
    inner: std::sync::Arc<Mutex<Vec<HelixRequest>>>,
}

impl CallLog {
    fn requests(&self) -> Vec<HelixRequest> {
        self.inner.lock().expect("call log must unlock").clone()
    }

    /// Subscription mutations only: creates (`POST` to the subscriptions
    /// URL) and deletes (`DELETE`). Token posts are not mutations.
    fn mutation_count(&self) -> usize {
        self.requests()
            .iter()
            .filter(|request| {
                request.method == Method::DELETE
                    || (request.method == Method::POST && request.url == HELIX_SUBSCRIPTIONS_URL)
            })
            .count()
    }

    fn delete_count(&self) -> usize {
        self.requests()
            .iter()
            .filter(|request| request.method == Method::DELETE)
            .count()
    }
}

/// Scripted transport: canned answers in order, every request recorded.
struct ScriptTransport {
    answers: Mutex<VecDeque<Result<HelixResponse, HelixError>>>,
    log: CallLog,
}

impl ScriptTransport {
    fn new(answers: Vec<Result<HelixResponse, HelixError>>, log: CallLog) -> Self {
        Self {
            answers: Mutex::new(answers.into()),
            log,
        }
    }

    fn token_answer() -> Result<HelixResponse, HelixError> {
        Ok(HelixResponse {
            status: StatusCode::OK,
            body: json!({"access_token": "synthetic-token", "expires_in": 3_600})
                .to_string()
                .into_bytes(),
        })
    }

    /// A 200 answer carrying `document` verbatim (already-parsed JSON value
    /// re-serialized, so the bytes are always valid JSON).
    fn ok_json(document: &Value) -> Result<HelixResponse, HelixError> {
        Ok(HelixResponse {
            status: StatusCode::OK,
            body: document.to_string().into_bytes(),
        })
    }

    /// A 200 answer carrying raw `body` bytes verbatim (for inputs that are
    /// not necessarily decodable JSON, e.g. over-nested payloads).
    fn ok_raw(body: Vec<u8>) -> Result<HelixResponse, HelixError> {
        Ok(HelixResponse {
            status: StatusCode::OK,
            body,
        })
    }

    fn status(status: StatusCode) -> Result<HelixResponse, HelixError> {
        Ok(HelixResponse {
            status,
            body: Vec::new(),
        })
    }
}

impl HelixTransport for ScriptTransport {
    async fn send(&self, request: HelixRequest) -> Result<HelixResponse, HelixError> {
        self.log
            .inner
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

fn client_with(
    answers: Vec<Result<HelixResponse, HelixError>>,
    log: CallLog,
) -> HelixClient<ScriptTransport> {
    HelixClient::new(ScriptTransport::new(answers, log), fixture_creds()).with_clock(|| 0)
}

/// The error contract for a malformed envelope: the bounded `BadResponse`
/// variant (a static description), with no response payload echoed into
/// either the `Display` or the `Debug` rendering.
fn assert_bounded_listing_error(error: &HelixError, shape: &str) {
    assert!(
        matches!(error, HelixError::BadResponse(_)),
        "{shape}: malformed successful listing must fail with a bounded \
         BadResponse, got: {error:?}"
    );
    for rendered in [format!("{error}"), format!("{error:?}")] {
        assert!(
            !rendered.contains(PAYLOAD_SENTINEL),
            "{shape}: error must not echo response payload bytes: {rendered}"
        );
    }
}

/// Every malformed top-level / `data` shape on a 200 answer is rejected as a
/// listing error, never interpreted as an empty subscription list.
#[tokio::test]
async fn malformed_successful_listing_envelopes_are_rejected() {
    let shapes: Vec<(&str, Value)> = vec![
        ("empty object (missing data)", json!({})),
        ("null top level", json!(null)),
        ("array top level", json!([])),
        ("string top level", json!(PAYLOAD_SENTINEL)),
        ("number top level", json!(42)),
        ("boolean top level", json!(true)),
        ("data is null", json!({"data": null})),
        ("data is a string", json!({"data": PAYLOAD_SENTINEL})),
        ("data is a number", json!({"data": 42})),
        ("data is a boolean", json!({"data": false})),
        (
            "data is an object",
            json!({"data": {"leak": PAYLOAD_SENTINEL}}),
        ),
        (
            "pagination without data",
            json!({"pagination": {"cursor": "c1"}}),
        ),
    ];
    for (shape, document) in shapes {
        let log = CallLog::default();
        let mut client = client_with(
            vec![
                ScriptTransport::token_answer(),
                ScriptTransport::ok_json(&document),
            ],
            log.clone(),
        );
        let error = client.list_subscriptions().await.expect_err(&format!(
            "{shape}: malformed listing must not read as empty"
        ));
        assert_bounded_listing_error(&error, shape);
        assert_eq!(
            log.requests().len(),
            2,
            "{shape}: exactly the token call and one listing call, no retries"
        );
        assert_eq!(
            log.requests()[1].method,
            Method::GET,
            "{shape}: the failing call is the listing GET"
        );
    }
}

/// Regression pin: a structurally valid empty listing stays a valid empty
/// result, with or without `pagination` and with unrelated extra fields.
#[tokio::test]
async fn valid_empty_listing_remains_a_valid_empty_result() {
    for (shape, document) in [
        ("bare empty data", json!({"data": []})),
        (
            "empty data with pagination",
            json!({"data": [], "pagination": {}}),
        ),
        (
            "empty data with extra fields",
            json!({"data": [], "total": 0, "max_total_cost": 0, "pagination": {}}),
        ),
    ] {
        let log = CallLog::default();
        let mut client = client_with(
            vec![
                ScriptTransport::token_answer(),
                ScriptTransport::ok_json(&document),
            ],
            log.clone(),
        );
        let subscriptions = client
            .list_subscriptions()
            .await
            .expect("valid empty listing must succeed");
        assert!(
            subscriptions.is_empty(),
            "{shape}: data:[] is a valid empty listing"
        );
        assert_eq!(
            log.requests().len(),
            2,
            "{shape}: token plus one listing page"
        );
    }
}

/// A malformed second page rejects the whole listing: the accumulated first
/// page is never returned as partial success.
#[tokio::test]
async fn malformed_later_page_rejects_the_entire_listing() {
    let first_page = json!({
        "data": [subscription_entry("first-id", "enabled", "111", OUR_CALLBACK)],
        "pagination": {"cursor": "cursor-1"},
    });
    for (shape, second_page) in [
        ("second page is an empty object", json!({})),
        (
            "second page data is a string",
            json!({"data": PAYLOAD_SENTINEL}),
        ),
        ("second page data is null", json!({"data": null})),
    ] {
        let log = CallLog::default();
        let mut client = client_with(
            vec![
                ScriptTransport::token_answer(),
                ScriptTransport::ok_json(&first_page),
                ScriptTransport::ok_json(&second_page),
            ],
            log.clone(),
        );
        let error = client.list_subscriptions().await.expect_err(&format!(
            "{shape}: a malformed later page must reject the listing, \
             not truncate it to the accumulated first page"
        ));
        assert_bounded_listing_error(&error, shape);
        assert_eq!(
            log.requests().len(),
            3,
            "{shape}: token plus both listing pages were fetched"
        );
    }
}

/// A listing body nested past the JSON recursion limit fails the whole listing
/// closed: no panic, no partial reads, zero subscription mutations.
#[tokio::test]
async fn deeply_nested_listing_body_fails_closed_without_mutations() {
    // Arrays nest without a key so depth alone triggers the parser's limit.
    let deep_body = format!("{{\"data\": {}1{}}}", "[".repeat(300), "]".repeat(300)).into_bytes();
    let log = CallLog::default();
    let mut client = client_with(
        vec![
            ScriptTransport::token_answer(),
            ScriptTransport::ok_raw(deep_body),
        ],
        log.clone(),
    );
    let error = client
        .list_subscriptions()
        .await
        .expect_err("an over-nested listing body must fail closed, not read as empty");
    assert_bounded_listing_error(&error, "over-nested listing body");
    assert_eq!(
        log.mutation_count(),
        0,
        "invalid evidence must cause zero create/delete calls"
    );
    assert_eq!(
        log.requests().len(),
        2,
        "exactly the token call and one listing call, no retries"
    );
}

/// Regression pin: a valid multipage listing still accumulates every page,
/// and intentional per-entry filtering is unchanged — entries of another
/// type, version or transport, and malformed entries, are skipped without
/// failing the (envelope-valid) listing.
#[tokio::test]
async fn valid_multipage_listing_accumulates_and_filters_per_entry() {
    let mut wrong_type = subscription_entry("wrong-type-id", "enabled", "911", OUR_CALLBACK);
    wrong_type["type"] = json!("stream.offline");
    let mut wrong_version = subscription_entry("wrong-version-id", "enabled", "912", OUR_CALLBACK);
    wrong_version["version"] = json!("2");
    let mut wrong_transport =
        subscription_entry("wrong-transport-id", "enabled", "913", OUR_CALLBACK);
    wrong_transport["transport"] = json!({"method": "websocket", "session_id": "s"});
    let malformed_entry = json!({"type": "stream.online", "version": "1"});

    let first_page = json!({
        "data": [
            subscription_entry("keep-first", "enabled", "111", OUR_CALLBACK),
            wrong_type,
            wrong_version,
            wrong_transport,
            malformed_entry,
        ],
        "pagination": {"cursor": "cursor-1"},
    });
    let second_page = json!({
        "data": [subscription_entry("keep-second", "enabled", "222", OUR_CALLBACK)],
        "pagination": {},
    });
    let log = CallLog::default();
    let mut client = client_with(
        vec![
            ScriptTransport::token_answer(),
            ScriptTransport::ok_json(&first_page),
            ScriptTransport::ok_json(&second_page),
        ],
        log.clone(),
    );
    let subscriptions = client
        .list_subscriptions()
        .await
        .expect("valid multipage listing must succeed");
    let ids: Vec<&str> = subscriptions
        .iter()
        .map(|subscription| subscription.id.as_str())
        .collect();
    assert_eq!(
        ids,
        ["keep-first", "keep-second"],
        "only stream.online v1 webhook entries survive, from every page"
    );
    assert_eq!(log.requests().len(), 3, "token plus two listing pages");
    assert_eq!(
        log.requests()[2].url,
        format!("{HELIX_SUBSCRIPTIONS_URL}?type=stream.online&after=cursor-1"),
        "the second page follows the first page's cursor"
    );
}

/// Regression pin: the repeated-cursor refusal still fails the listing
/// closed instead of looping or truncating.
#[tokio::test]
async fn repeated_cursor_still_rejects_the_listing() {
    let page = json!({
        "data": [subscription_entry("only-id", "enabled", "111", OUR_CALLBACK)],
        "pagination": {"cursor": "cursor-1"},
    });
    let log = CallLog::default();
    let mut client = client_with(
        vec![
            ScriptTransport::token_answer(),
            ScriptTransport::ok_json(&page),
            ScriptTransport::ok_json(&page),
        ],
        log.clone(),
    );
    let error = client
        .list_subscriptions()
        .await
        .expect_err("a repeated pagination cursor must fail closed");
    assert_bounded_listing_error(&error, "repeated cursor");
    assert_eq!(log.requests().len(), 3, "token plus two listing pages");
}

/// Regression pin: the bounded page limit still fails the listing closed
/// after exactly `HELIX_MAX_SUBSCRIPTION_PAGES` pages.
#[tokio::test]
async fn page_limit_still_bounds_the_listing() {
    let mut answers = vec![ScriptTransport::token_answer()];
    for page in 0..HELIX_MAX_SUBSCRIPTION_PAGES {
        answers.push(ScriptTransport::ok_json(&json!({
            "data": [],
            "pagination": {"cursor": format!("cursor-{page}")},
        })));
    }
    let log = CallLog::default();
    let mut client = client_with(answers, log.clone());
    let error = client
        .list_subscriptions()
        .await
        .expect_err("an endless cursor chain must hit the page limit");
    assert!(
        matches!(error, HelixError::SubscriptionPageLimit),
        "expected the page-limit error, got: {error:?}"
    );
    assert_eq!(
        log.requests().len(),
        HELIX_MAX_SUBSCRIPTION_PAGES + 1,
        "token plus exactly the bounded number of pages"
    );
}

/// Envelope validation applies on the retried request too: the single 401
/// refresh-and-retry still happens exactly once, and the malformed 200 that
/// follows is a listing error, not an empty result.
#[tokio::test]
async fn envelope_validation_applies_after_the_single_401_retry() {
    let log = CallLog::default();
    let mut client = client_with(
        vec![
            ScriptTransport::token_answer(),
            ScriptTransport::status(StatusCode::UNAUTHORIZED),
            ScriptTransport::token_answer(),
            ScriptTransport::ok_json(&json!({})),
        ],
        log.clone(),
    );
    let error = client
        .list_subscriptions()
        .await
        .expect_err("a malformed envelope after the 401 retry must still be rejected");
    assert_bounded_listing_error(&error, "malformed envelope after 401 retry");
    let seen = log.requests();
    assert_eq!(
        seen.len(),
        4,
        "token, rejected listing, refresh, retried listing — one retry only"
    );
    assert_eq!(seen[0].url, TOKEN_URL);
    assert_eq!(seen[2].url, TOKEN_URL, "the 401 triggers one token refresh");
    assert_eq!(
        seen[3].method,
        Method::GET,
        "the retry re-issues the listing"
    );
}

/// One reconcile pass, mirroring the bot's startup sequence: list, plan from
/// that evidence, execute. The listing error aborts the pass before any
/// planning, exactly as the production pass propagates it.
async fn reconcile_pass(
    client: &mut HelixClient<ScriptTransport>,
    desired: &[String],
) -> Result<Vec<ActionOutcome>, HelixError> {
    let existing = client.list_subscriptions().await?;
    let plan = plan_boot_actions(desired, &callback(), &existing)?;
    client
        .execute_actions(&plan, &callback(), &test_secret())
        .await
}

/// An invalid initial listing causes zero create/delete subscription calls:
/// reconcile must not plan mutations from invalid evidence. The script
/// supplies answers for the creates the buggy "treat as empty" reading
/// would issue, so the failure is a clean assertion, not a fixture panic.
#[tokio::test]
async fn invalid_initial_listing_causes_zero_subscription_mutations() {
    let log = CallLog::default();
    let mut client = client_with(
        vec![
            ScriptTransport::token_answer(),
            ScriptTransport::ok_json(&json!({})),
            ScriptTransport::status(StatusCode::NO_CONTENT),
            ScriptTransport::status(StatusCode::NO_CONTENT),
        ],
        log.clone(),
    );
    let desired = vec!["222".to_owned(), "444".to_owned()];
    let error = reconcile_pass(&mut client, &desired)
        .await
        .expect_err("reconcile must fail on invalid initial evidence, not create from it");
    assert_bounded_listing_error(&error, "invalid initial listing");
    assert_eq!(
        log.mutation_count(),
        0,
        "invalid initial evidence must cause zero create/delete calls; saw: {:?}",
        log.requests()
            .iter()
            .map(|request| (request.method.clone(), request.url.clone()))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        log.requests().len(),
        2,
        "only the token call and the rejected listing were issued"
    );
}

/// Invalid post-mutation confirmation cannot report healthy: after a create
/// the remote side already accepted, a malformed confirmation listing fails
/// the pass. No compensating delete is promised or issued — the accepted
/// mutation stands, the pass simply must not claim success.
#[tokio::test]
async fn invalid_post_mutation_confirmation_cannot_report_healthy() {
    let log = CallLog::default();
    let mut client = client_with(
        vec![
            ScriptTransport::token_answer(),
            ScriptTransport::ok_json(&json!({"data": [], "pagination": {}})),
            ScriptTransport::status(StatusCode::NO_CONTENT),
            ScriptTransport::ok_json(&json!({"data": 13})),
        ],
        log.clone(),
    );

    // List (valid, empty), plan, execute the create, then confirm by listing
    // again — the production pass's healthy check reads that second listing.
    let existing = client
        .list_subscriptions()
        .await
        .expect("the initial valid listing must succeed");
    let plan = plan_boot_actions(&["444".to_owned()], &callback(), &existing)
        .expect("plan must build from valid evidence");
    assert_eq!(
        plan,
        vec![SubscriptionAction::Create {
            broadcaster_user_id: "444".to_owned()
        }]
    );
    let outcomes = client
        .execute_actions(&plan, &callback(), &test_secret())
        .await
        .expect("the create itself was accepted");
    assert_eq!(
        outcomes,
        vec![ActionOutcome::Created {
            broadcaster_user_id: "444".to_owned()
        }]
    );

    let error = client.list_subscriptions().await.expect_err(
        "a malformed confirmation listing must fail the pass, \
         never read as a healthy (or empty-but-converged) state",
    );
    assert_bounded_listing_error(&error, "malformed confirmation listing");
    assert_eq!(
        log.mutation_count(),
        1,
        "exactly the accepted create happened"
    );
    assert_eq!(
        log.delete_count(),
        0,
        "no compensating delete may be issued for a remotely accepted create"
    );
}

/// The 409 confirmation path reads a follow-up listing through the same
/// contract: a malformed follow-up envelope surfaces as the listing error
/// itself — not `AlreadyExists`, and not a misleading bare conflict.
#[tokio::test]
async fn malformed_conflict_confirmation_surfaces_the_listing_error() {
    let log = CallLog::default();
    let mut client = client_with(
        vec![
            ScriptTransport::token_answer(),
            ScriptTransport::status(StatusCode::CONFLICT),
            ScriptTransport::ok_json(&json!({})),
        ],
        log.clone(),
    );
    let actions = vec![SubscriptionAction::Create {
        broadcaster_user_id: "222".to_owned(),
    }];
    let error = client
        .execute_actions(&actions, &callback(), &test_secret())
        .await
        .expect_err("a 409 confirmed only by a malformed listing must fail");
    assert_bounded_listing_error(&error, "malformed 409 confirmation listing");
    assert_eq!(
        log.requests().len(),
        3,
        "token, create, follow-up confirmation listing"
    );
}
