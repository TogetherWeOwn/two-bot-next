//! Acceptance tests: EventSub subscription reconcile at startup (M3.3).
//!
//! The Twitch EventSub receiver is served when `TWO_EVENTSUB=1`, but nothing
//! calls the pure-core reconciler yet (`create_subscription_body`,
//! `parse_subscriptions`, `reconcile` in `two_bot_core::twitch_eventsub`,
//! driven over HTTP by `two_bot_core::twitch_helix`). These tests pin the
//! contract the startup wiring must satisfy; see `docs/stream-funnel.md`
//! ("Subscription management is pure request building plus a diff ... runs at
//! boot and after a `revocation`").
//!
//! Each test has two halves: a runtime contract proven over a scripted
//! [`HelixTransport`] (offline: no database, no live Twitch, no prod
//! mutation), and a wiring gate asserting the bot actually calls that path at
//! startup. The gates are red until the Builder lands the wiring on this
//! branch; the Builder must satisfy them **without editing this file**:
//!
//! - gate on `TWO_EVENTSUB=1` with the Operator-provisioned Twitch
//!   credentials and signing secret, plus the bot's own callback URL and the
//!   desired broadcaster list;
//! - list subscriptions, diff with `reconcile`/`plan_boot_actions` (deletes
//!   before creates, only our callback), execute with `execute_actions` so a
//!   409 create counts only after a follow-up listing confirms a healthy
//!   subscription on our callback;
//! - keep the signing secret inside the HMAC and the create-subscription body
//!   only: scalar log fields, never the value.
//!
//! Fixture secrets are runtime-generated (`test_secret_value`), never
//! literals, and every callback/host is an `example.com` fixture.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex, OnceLock,
};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::{Method, StatusCode};
use serde_json::{json, Value};
use two_bot_core::twitch_eventsub::{
    create_subscription_body, verify_delivery, CallbackUrl, DeliveryHeaders, EventSubError,
    SubscriptionAction, WebhookSecret,
};
use two_bot_core::twitch_helix::{
    plan_boot_actions, ActionOutcome, HelixClient, HelixCredentials, HelixError, HelixRequest,
    HelixResponse, HelixTransport, HELIX_SUBSCRIPTIONS_URL, TOKEN_URL, TWITCH_CLIENT_ID_ENV,
    TWITCH_CLIENT_SECRET_ENV, TWITCH_WEBHOOK_SECRET_ENV,
};
use two_bot_core::{format_iso_millis, Secret};

/// The bot's own callback: only subscriptions on this URL may be touched.
const OUR_CALLBACK: &str = "https://hooks.example.com/twitch";
/// A foreign callback: the reconciler must never create, delete or verify
/// against it.
const FOREIGN_CALLBACK: &str = "https://other.example.com/hook";

static NEXT_TEST_SECRET: AtomicU64 = AtomicU64::new(1);

/// Runtime-generated signing secret: no literal key material reaches the HMAC,
/// so secret scanners stay silent. Shared via `OnceLock` so every helper in
/// this binary agrees on the value.
fn test_secret_value() -> &'static str {
    static SECRET: OnceLock<String> = OnceLock::new();
    SECRET.get_or_init(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time is after the Unix epoch")
            .as_nanos();
        let nonce = NEXT_TEST_SECRET.fetch_add(1, Ordering::Relaxed);
        format!("synthetic-webhook-secret-{nanos:032x}-{nonce:016x}")
    })
}

fn test_secret() -> WebhookSecret {
    WebhookSecret::new(test_secret_value().to_owned()).expect("generated secret must be valid")
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

/// Scripted transport: canned answers in order, shared call log for order and
/// body assertions. The log is shared by clone because `HelixClient` owns the
/// transport and never exposes it.
#[derive(Clone, Default)]
struct CallLog {
    inner: std::sync::Arc<Mutex<Vec<HelixRequest>>>,
}

impl CallLog {
    fn requests(&self) -> Vec<HelixRequest> {
        self.inner.lock().expect("call log must unlock").clone()
    }

    fn urls(&self) -> Vec<String> {
        self.requests()
            .into_iter()
            .map(|request| request.url)
            .collect()
    }
}

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

    fn token_answer(token: &str) -> Result<HelixResponse, HelixError> {
        Ok(HelixResponse {
            status: StatusCode::OK,
            body: json!({"access_token": token, "expires_in": 3_600})
                .to_string()
                .into_bytes(),
        })
    }

    fn listing_answer(document: &Value) -> Result<HelixResponse, HelixError> {
        Ok(HelixResponse {
            status: StatusCode::OK,
            body: document.to_string().into_bytes(),
        })
    }

    fn no_content() -> Result<HelixResponse, HelixError> {
        Ok(HelixResponse {
            status: StatusCode::NO_CONTENT,
            body: Vec::new(),
        })
    }

    fn conflict() -> Result<HelixResponse, HelixError> {
        Ok(HelixResponse {
            status: StatusCode::CONFLICT,
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

/// Every `.rs` file under `crates/bot/src`, so the Builder may place the
/// startup wiring in any module (including a new one) without editing tests.
fn bot_rs_files() -> Vec<PathBuf> {
    fn visit(dir: &Path, out: &mut Vec<PathBuf>) {
        let entries = std::fs::read_dir(dir).expect("bot src dir must be readable");
        for entry in entries {
            let path = entry.expect("dir entry must be readable").path();
            if path.is_dir() {
                visit(&path, out);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }

    let mut files = Vec::new();
    visit(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    files.sort();
    files
}

fn joined_bot_sources() -> String {
    let mut joined = String::new();
    for path in bot_rs_files() {
        joined.push_str(&std::fs::read_to_string(&path).expect("bot source must be readable"));
        joined.push('\n');
    }
    joined
}

/// Red until the Builder wires startup reconcile: the bot must drive the pure
/// plan (`reconcile`/`plan_boot_actions`) through the verifying executor.
/// Any of the accepted entry names satisfies the gate; a raw Helix hand-roll
/// does not (see the no-bypass guard in the 409 test).
fn assert_startup_reconcile_wired(sources: &str, criterion: &str) {
    assert!(
        [
            "execute_actions",
            "plan_boot_actions",
            "reconcile_all",
            "startup_reconcile",
        ]
        .into_iter()
        .any(|marker| sources.contains(marker)),
        "M3.3 startup reconcile is not wired ({criterion}): with TWO_EVENTSUB=1 \
         the bot must list subscriptions, diff with reconcile/plan_boot_actions \
         and execute with execute_actions (or an equivalent reconcile_all / \
         startup_reconcile helper that delegates to them)"
    );
}

fn hmac_sign(message_id: &str, timestamp: &str, body: &[u8]) -> String {
    use hmac::{Hmac, KeyInit, Mac as _};
    use sha2::Sha256;

    let mut mac = <Hmac<Sha256>>::new_from_slice(test_secret_value().as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(message_id.as_bytes());
    mac.update(timestamp.as_bytes());
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// Startup with `TWO_EVENTSUB=1` reconciles subscriptions on the bot's own
/// callback: missing subscriptions are created, stale ones deleted, deletes
/// run before creates (Twitch answers 409 to a duplicate create), and
/// subscriptions on any other callback are never touched.
#[tokio::test]
async fn startup_reconciles_own_callback_deletes_before_creates() {
    let sources = joined_bot_sources();
    assert_startup_reconcile_wired(&sources, "own-callback deletes-before-creates");
    assert!(
        sources.contains("CallbackUrl"),
        "M3.3 startup reconcile must thread the bot's own CallbackUrl \
         (callback-scoped diff at boot, never a foreign callback)"
    );
    assert!(
        ["list_subscriptions", "reconcile_all", "startup_reconcile"]
            .into_iter()
            .any(|marker| sources.contains(marker)),
        "M3.3 startup reconcile must list subscriptions at boot \
         (directly, or through a reconcile_all / startup_reconcile helper)"
    );

    let callback = callback();
    let secret = test_secret();
    let listing = json!({
        "data": [
            subscription_entry("stale-id", "enabled", "111", OUR_CALLBACK),
            subscription_entry("keep-id", "enabled", "222", OUR_CALLBACK),
            subscription_entry("foreign-id", "enabled", "333", FOREIGN_CALLBACK),
        ],
        "pagination": {},
    });
    let log = CallLog::default();
    let transport = ScriptTransport::new(
        vec![
            ScriptTransport::token_answer("startup-token"),
            ScriptTransport::listing_answer(&listing),
            ScriptTransport::no_content(),
            ScriptTransport::no_content(),
        ],
        log.clone(),
    );
    let mut client = HelixClient::new(transport, fixture_creds()).with_clock(|| 0);

    // The startup sequence: list everything, diff against desired, execute.
    let existing = client
        .list_subscriptions()
        .await
        .expect("startup listing must work");
    assert_eq!(existing.len(), 3, "all three fixture subs must parse");
    let desired = vec!["222".to_owned(), "444".to_owned()];
    let plan = plan_boot_actions(&desired, &callback, &existing).expect("plan must build");
    assert_eq!(
        plan,
        vec![
            SubscriptionAction::Delete {
                id: "stale-id".to_owned()
            },
            SubscriptionAction::Create {
                broadcaster_user_id: "444".to_owned()
            },
        ],
        "healthy keep-id stays, foreign-id is never touched, stale-id is deleted, 444 is created"
    );
    let outcomes = client
        .execute_actions(&plan, &callback, &secret)
        .await
        .expect("startup execution must work");
    assert_eq!(
        outcomes,
        vec![
            ActionOutcome::Deleted {
                id: "stale-id".to_owned()
            },
            ActionOutcome::Created {
                broadcaster_user_id: "444".to_owned()
            },
        ]
    );

    // Deletes precede creates on the wire, on our callback only.
    let seen = log.requests();
    assert_eq!(seen.len(), 4, "token, listing, delete, create");
    assert_eq!(seen[0].method, Method::POST);
    assert_eq!(seen[0].url, TOKEN_URL);
    assert_eq!(seen[1].method, Method::GET);
    assert_eq!(
        seen[1].url,
        format!("{HELIX_SUBSCRIPTIONS_URL}?type=stream.online")
    );
    assert_eq!(seen[2].method, Method::DELETE);
    assert_eq!(
        seen[2].url,
        format!("{HELIX_SUBSCRIPTIONS_URL}?id=stale-id")
    );
    assert_eq!(seen[3].method, Method::POST);
    assert_eq!(seen[3].url, HELIX_SUBSCRIPTIONS_URL);
    let body = seen[3]
        .body
        .as_ref()
        .expect("create carries a body")
        .expose()
        .clone();
    let document: Value = serde_json::from_str(&body).expect("create body must be JSON");
    assert_eq!(document["condition"]["broadcaster_user_id"], "444");
    assert_eq!(document["transport"]["callback"], OUR_CALLBACK);
    assert_eq!(document["transport"]["method"], "webhook");
    for request in &seen {
        assert!(
            !request.url.contains("other.example.com"),
            "startup must never call a foreign callback: {}",
            request.url
        );
    }
    assert!(
        !body.contains(FOREIGN_CALLBACK),
        "create body must reference only our callback"
    );
}

/// A 409 create counts as done only after a follow-up listing confirms a
/// healthy subscription for that broadcaster on our callback; a conflict that
/// cannot be confirmed (foreign callback, failed status) stays an error.
#[tokio::test]
async fn subscription_conflict_counts_as_done_only_after_followup_listing_confirms_healthy() {
    let sources = joined_bot_sources();
    assert_startup_reconcile_wired(&sources, "409 verified by follow-up listing");
    assert!(
        !sources.contains("helix/eventsub/subscriptions"),
        "M3.3 startup must execute through the verifying Helix client \
         (execute_actions), never a hand-rolled subscription create that skips \
         the follow-up listing"
    );
    let helix = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../core/src/twitch_helix.rs"),
    )
    .expect("helix client source must be readable");
    assert!(
        helix.contains("list_subscriptions_for_broadcaster"),
        "the executor must confirm a 409 with a follow-up broadcaster listing"
    );

    let callback = callback();
    let secret = test_secret();
    let actions = vec![SubscriptionAction::Create {
        broadcaster_user_id: "222".to_owned(),
    }];

    // Verified: the follow-up listing shows a healthy sub on our callback.
    let confirmed = json!({
        "data": [subscription_entry("existing-id", "enabled", "222", OUR_CALLBACK)],
        "pagination": {},
    });
    let log = CallLog::default();
    let transport = ScriptTransport::new(
        vec![
            ScriptTransport::token_answer("exec-token"),
            ScriptTransport::conflict(),
            ScriptTransport::listing_answer(&confirmed),
        ],
        log.clone(),
    );
    let mut client = HelixClient::new(transport, fixture_creds()).with_clock(|| 0);
    let outcomes = client
        .execute_actions(&actions, &callback, &secret)
        .await
        .expect("verified 409 must count as done");
    assert_eq!(
        outcomes,
        vec![ActionOutcome::AlreadyExists {
            broadcaster_user_id: "222".to_owned()
        }]
    );
    let urls = log.urls();
    assert_eq!(urls.len(), 3, "token, create, follow-up listing");
    assert_eq!(urls[1], HELIX_SUBSCRIPTIONS_URL);
    assert_eq!(
        urls[2],
        format!("{HELIX_SUBSCRIPTIONS_URL}?user_id=222"),
        "the follow-up listing filters by broadcaster only (Twitch rejects multi-filter listings)"
    );

    // Unverifiable conflicts stay errors: foreign callback, then failed status.
    for (callback_url, status) in [
        (FOREIGN_CALLBACK, "enabled"),
        (OUR_CALLBACK, "notification_failures_exceeded"),
    ] {
        let unconfirmed = json!({
            "data": [subscription_entry("existing-id", status, "222", callback_url)],
            "pagination": {},
        });
        let log = CallLog::default();
        let transport = ScriptTransport::new(
            vec![
                ScriptTransport::token_answer("exec-token"),
                ScriptTransport::conflict(),
                ScriptTransport::listing_answer(&unconfirmed),
            ],
            log.clone(),
        );
        let mut client = HelixClient::new(transport, fixture_creds()).with_clock(|| 0);
        let error = client
            .execute_actions(&actions, &callback, &secret)
            .await
            .expect_err("unconfirmed 409 must fail");
        assert!(
            matches!(
                error,
                HelixError::HelixRejected {
                    status: StatusCode::CONFLICT,
                    ..
                }
            ),
            "unconfirmed conflict must surface, not count as done: {error}"
        );
        assert_eq!(log.urls().len(), 3, "token, create, follow-up listing");
    }
}

/// The signing secret appears only inside the HMAC and the create-subscription
/// body, never in logs: every `Secret` debug renders `[REDACTED]`, every
/// Helix error names variables and statuses rather than values, and no
/// tracing callsite in the bot interpolates an exposed secret.
#[test]
fn webhook_secret_reaches_only_hmac_and_create_body_never_logs() {
    let sources = joined_bot_sources();
    assert_startup_reconcile_wired(&sources, "secret hygiene of the startup path");
    for (line_no, line) in sources.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            continue;
        }
        assert!(
            !(line.contains("tracing::") && line.contains(".expose()")),
            "bot source line {} logs an exposed value: {line}",
            line_no + 1
        );
    }

    let secret = test_secret();
    let rendered = format!("{secret:?} {secret:#?}");
    assert!(
        !rendered.contains(test_secret_value()),
        "WebhookSecret Debug must redact the signing secret"
    );
    assert!(rendered.contains("[REDACTED]"));

    // Allowed location 1: the HMAC over message-id + timestamp + body.
    let body = br#"{"subscription":{"id":"s","type":"stream.online","version":"1"}}"#;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time is after the Unix epoch")
        .as_millis() as i64;
    let timestamp = format_iso_millis(now_ms);
    let signature = hmac_sign("synthetic-delivery-0001", &timestamp, body);
    let headers = DeliveryHeaders::from_lookup(|name| match name {
        "twitch-eventsub-message-id" => Some("synthetic-delivery-0001"),
        "twitch-eventsub-message-timestamp" => Some(timestamp.as_str()),
        "twitch-eventsub-message-signature" => Some(signature.as_str()),
        "twitch-eventsub-message-type" => Some("notification"),
        _ => None,
    })
    .expect("fixture headers must parse");
    assert_eq!(
        verify_delivery(&secret, &headers, body, now_ms),
        Ok(()),
        "the HMAC path must accept the runtime-generated secret"
    );

    // Allowed location 2: the create-subscription body, itself `Secret`-typed.
    let body_secret =
        create_subscription_body("222", &callback(), &secret).expect("create body must build");
    assert_eq!(format!("{body_secret:?}"), "[REDACTED]");
    let document: Value =
        serde_json::from_str(body_secret.expose()).expect("create body must be JSON");
    assert_eq!(
        document["transport"]["secret"],
        test_secret_value(),
        "Twitch receives the secret only inside the create body"
    );
    assert_eq!(
        create_subscription_body("not-a-broadcaster", &callback(), &secret).unwrap_err(),
        EventSubError::InvalidBroadcasterId
    );

    // Errors name variables and statuses, never values.
    let creds = fixture_creds();
    let rendered = format!("{creds:?} {creds:#?}");
    assert!(
        !rendered.contains("fixture-client-secret"),
        "leaked: {rendered}"
    );
    assert!(rendered.contains("[REDACTED]"));
    for error in [
        HelixError::Missing(TWITCH_WEBHOOK_SECRET_ENV),
        HelixError::Missing(TWITCH_CLIENT_ID_ENV),
        HelixError::Missing(TWITCH_CLIENT_SECRET_ENV),
        HelixError::InvalidCredential {
            name: TWITCH_WEBHOOK_SECRET_ENV,
            reason: "value must be 10-100 printable ASCII characters",
        },
        HelixError::TokenRejected(StatusCode::FORBIDDEN),
        HelixError::HelixRejected {
            status: StatusCode::CONFLICT,
            action: "create subscription",
        },
    ] {
        let rendered = format!("{error}");
        assert!(
            !rendered.contains(test_secret_value()),
            "error leaks the signing secret: {rendered}"
        );
        assert!(
            !rendered.contains("fixture-client-secret"),
            "error leaks the client secret: {rendered}"
        );
    }
    let request = HelixRequest {
        method: Method::POST,
        url: HELIX_SUBSCRIPTIONS_URL.to_owned(),
        headers: vec![(
            "authorization".to_owned(),
            Secret::new("Bearer synthetic-token".to_owned()),
        )],
        body: Some(Secret::new("synthetic-create-body".to_owned())),
    };
    let rendered = format!("{request:?}");
    assert!(!rendered.contains("synthetic-token"), "leaked: {rendered}");
    assert!(
        !rendered.contains("synthetic-create-body"),
        "leaked: {rendered}"
    );
    assert!(rendered.contains("[REDACTED]"));

    // The startup path must load the Operator-provisioned credentials: either
    // by naming the env vars or through the typed loaders (which cite the
    // same names in their errors).
    for (env_name, loader) in [
        (TWITCH_CLIENT_ID_ENV, "HelixCredentials"),
        (TWITCH_CLIENT_SECRET_ENV, "HelixCredentials"),
        (TWITCH_WEBHOOK_SECRET_ENV, "webhook_secret_from_env"),
    ] {
        assert!(
            sources.contains(env_name) || sources.contains(loader),
            "the startup path must load the Operator-provisioned {env_name} \
             (or use {loader})"
        );
    }
}
