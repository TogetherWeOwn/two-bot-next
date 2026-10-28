//! Private backfill progress route: `GET /ops/backfill/progress` on the existing listener.
//!
//! Slice 8 of TOG-22852, covering its AC3 ("progress + ETA exposed for
//! `/ops/backfill/progress`"). Follows the [`crate::metrics_http`] pattern: a private
//! route on the health/metrics listener (never the internal-action receiver),
//! rendering JSON like `/readyz` instead of Prometheus text.
//!
//! Edge ingress: the Worker forwards this exact path to the container with
//! the `x-two-*` headers passed through unchanged (the container owns the
//! HMAC; the scrape bearer never applies here), so operators reach it at the
//! same `/ops/backfill/progress` path they pin in tests.
//!
//! ## Contract (pinned by `crates/bot/tests/ops_backfill.rs`)
//!
//! * `GET /ops/backfill/progress` with a `{"families": {...}}` body: one entry per
//!   family of the bounded nine-family vocabulary from
//!   [`two_bot_store::backfill_cursors::BACKFILL_FAMILIES`]. Family keys never
//!   come from cursors or request-unique values (fixed cardinality), and bodies
//!   carry ids/counts/timestamps/enums only — no message bodies, no
//!   custom-status text (privacy fence, same as event-store S1).
//! * Auth reuses the existing `TWO_INTERNAL_KEYS` ring and `x-two-*` signed
//!   headers, but over a path-bound canonical string
//!   `GET\n/ops/backfill/progress\n{ts}\n{nonce}\n{sha256_hex(body)}`, so a signature
//!   cut for `/internal/actions` never authorizes a backfill read (and vice
//!   versa). Every auth failure — missing, malformed, unknown-key, forged,
//!   stale or replayed — is one indistinguishable `401` carrying exactly the
//!   shared [`AUTH_FAILURE_MESSAGE`][two_bot_core::internal_actions::AUTH_FAILURE_MESSAGE].
//!   Uniform refusal is deliberate: the receiver distinguishes stale signatures
//!   with a `stale_request` code, but a read-only route has no reason to tell
//!   a caller which check failed.
//!
//! ## v1 derivation (ruling)
//!
//! The cursor store carries `{family, cursor, updated_at}` rows and no totals,
//! so the only honest per-family numbers are a proven-completion lower bound:
//! `progress` is `0.0` (nothing proven complete) with or without a checkpoint,
//! and `eta_seconds` is `null` (no rate model yet). The actionable signal is
//! the frontier itself — `cursor` plus `updated_at` (epoch seconds; stale
//! means stuck) — which is why both are exposed verbatim. Progress becomes a
//! real fraction once a worker slice records totals; until then any non-zero
//! value would be fiction (reporting completion that was never proven costs
//! an operator their watch).
//!
//! Reads never touch staging or production Discord, and the snapshot query
//! selects the three fence columns only. Without a database (parked boot, like
//! the acceptance fixture) every family reports the empty-store baseline.
//! A configured-but-unreadable database is a `503`, never a zeroed `200`: a
//! blip must not read as "not started".

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use hmac::{Hmac, KeyInit as _, Mac as _};
use serde_json::{json, Value};
use two_bot_core::{
    clock_guard::ClockGuard,
    internal_actions::{self, AUTH_FAILURE_MESSAGE, NONCE_TTL_SECONDS, SKEW_SECONDS},
};

use crate::backfill_retention_chains::{render_backfill_failure, BackfillStep};
use crate::gateway_failure::FailureClass;

/// Route under test. Keep in sync with `crate::server::TRACED_PATHS`.
pub const BACKFILL_PATH: &str = "/ops/backfill/progress";

/// Failure class for the debug log only: every class renders the same 401, so
/// these tokens never reach the wire.
#[derive(Clone, Copy, Debug)]
enum Denial {
    Missing,
    Malformed,
    Mismatch,
    Stale,
    Replay,
}

impl Denial {
    fn as_str(self) -> &'static str {
        match self {
            Denial::Missing => "missing_credentials",
            Denial::Malformed => "malformed",
            Denial::Mismatch => "bad_signature",
            Denial::Stale => "stale",
            Denial::Replay => "replay",
        }
    }
}

struct RingInner {
    /// Caller id to raw secret, from `TWO_INTERNAL_KEYS` via `parse_keys`
    /// (which already refuses duplicate ids and reused secrets).
    keys: HashMap<String, Vec<u8>>,
    /// Decoy secret for unknown key ids: both failures cost the same HMAC, so
    /// an unknown id is indistinguishable from a wrong signature by timing.
    decoy: [u8; 32],
    nonces: internal_actions::NonceCache,
    clock: ClockGuard,
}

/// Shared verifier for the backfill route. Built once per router from the
/// process environment in production, or from an explicit key spec in tests
/// (no process-env mutation, so tests stay parallel-safe). Lives in
/// [`crate::server::SharedState`] — not an `Extension` layer — so the
/// handler stays a `State + Request` pair like the internal-action
/// receiver's `handle`.
#[derive(Clone)]
pub struct SharedRing(Arc<Mutex<RingInner>>);

impl SharedRing {
    fn new(keys: Vec<two_bot_core::internal_actions::SigningKey>) -> Self {
        Self(Arc::new(Mutex::new(RingInner {
            keys: keys
                .into_iter()
                .map(|key| (key.id, key.secret.expose().clone()))
                .collect(),
            decoy: rand::random(),
            nonces: internal_actions::NonceCache::new(NONCE_TTL_SECONDS),
            clock: ClockGuard::new(),
        })))
    }

    /// Fail-closed empty ring: every read refuses. Default until production
    /// attaches [`SharedRing::from_env`] or a test attaches an explicit spec.
    pub(crate) fn empty() -> Self {
        Self(Arc::new(Mutex::new(RingInner {
            keys: HashMap::new(),
            decoy: rand::random(),
            nonces: internal_actions::NonceCache::new(NONCE_TTL_SECONDS),
            clock: ClockGuard::new(),
        })))
    }

    /// Production wiring: parse `TWO_INTERNAL_KEYS`, fail closed to an empty
    /// ring (every read refuses) when it is missing or invalid. Silent at
    /// INFO: `router_with_guard` runs under test dispatchers that capture
    /// INFO+ events (the shutdown-correlation test pins exactly three
    /// lifecycle lines), and the internal-action receiver likewise stays
    /// quiet when dark. Per-read denials already log at debug with a reason.
    pub(crate) fn from_env() -> Self {
        match std::env::var("TWO_INTERNAL_KEYS")
            .ok()
            .and_then(|spec| internal_actions::parse_keys(&spec).ok())
        {
            Some(keys) => Self::new(keys),
            None => {
                tracing::debug!(
                    msg = "ops_backfill_keys_invalid",
                    "TWO_INTERNAL_KEYS missing or invalid; backfill reads will refuse"
                );
                Self::empty()
            }
        }
    }

    /// Test seam: an explicit `id:secret` spec, never the process environment.
    #[cfg(test)]
    fn from_spec(spec: &str) -> Self {
        Self::new(internal_actions::parse_keys(spec).expect("test key spec parses"))
    }
}

/// One snapshot row: the three fence columns, nothing else.
struct FamilyRow {
    family: String,
    cursor: String,
    updated_at: i64,
}

/// Build the route. The pool and the key ring both arrive via listener state
/// (no module slot, no `Extension` layer: the database handle is already
/// there, and the ring rides in [`crate::server::SharedState`]), so the
/// handler stays a `State + Request` pair like the internal-action
/// receiver's `handle`. Tests inject an explicit spec through
/// [`crate::server::SharedState::with_backfill_ring`].
pub(crate) fn router() -> Router<(crate::server::SharedState, crate::jobs::SharedStatus)> {
    Router::<(crate::server::SharedState, crate::jobs::SharedStatus)>::new()
        .route(BACKFILL_PATH, get(handler))
}

fn header_value<'a>(headers: &'a HeaderMap, name: &str, max: usize) -> Option<&'a str> {
    let value = headers.get(name)?.to_str().ok()?;
    if value.is_empty() || value.len() > max {
        return None;
    }
    Some(value)
}

fn expected_signature(secret: &[u8], timestamp: &str, nonce: &str, raw: &[u8]) -> String {
    let canonical = format!(
        "GET\n{BACKFILL_PATH}\n{timestamp}\n{nonce}\n{}",
        internal_actions::body_hash(raw)
    );
    let mut mac =
        Hmac::<sha2::Sha256>::new_from_slice(secret).expect("HMAC accepts keys of any length");
    mac.update(canonical.as_bytes());
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// Mirror of `AuthenticatedRequest::verify` with the backfill canonical: same
/// check order (signature before the clock, so a forgery cannot move the
/// high-water mark; freshness before the nonce offer, so a stale capture
/// cannot pollute the replay window), uniform denial instead of typed errors.
fn authenticate(
    ring: &mut RingInner,
    headers: &HeaderMap,
    raw: &[u8],
    now_ms: u64,
) -> Result<(), Denial> {
    // Same bounds as the receiver's wire headers: ids travel, secrets never do.
    let key_id = header_value(headers, "x-two-key-id", 128).ok_or(Denial::Missing)?;
    let timestamp = header_value(headers, "x-two-timestamp", 15).ok_or(Denial::Missing)?;
    let nonce = header_value(headers, "x-two-nonce", 32).ok_or(Denial::Missing)?;
    let signature = header_value(headers, "x-two-signature", 71).ok_or(Denial::Missing)?;
    if !internal_actions::valid_nonce_format(nonce) {
        return Err(Denial::Malformed);
    }
    // Evaluate the constant-time compare unconditionally, exactly like
    // `KeyRing::verify`: an unknown id still costs one HMAC, and only the
    // `known &&` term below can refuse it.
    let secret: &[u8] = ring
        .keys
        .get(key_id)
        .map_or(ring.decoy.as_slice(), Vec::as_slice);
    let matched = internal_actions::signatures_match(
        &expected_signature(secret, timestamp, nonce, raw),
        signature,
    );
    if !(ring.keys.contains_key(key_id) && matched)
        || crate::internal_action_admin::requires_scope(key_id)
    {
        return Err(Denial::Mismatch);
    }
    let guarded = ring.clock.evaluate(now_ms).map_err(|_| Denial::Stale)?;
    if !internal_actions::within_skew(timestamp, SKEW_SECONDS, guarded / 1000) {
        return Err(Denial::Stale);
    }
    // Burn at the guarded mark, mirroring the receiver: `offer` already
    // sweeps, so no separate sweep call.
    if !ring.nonces.offer(nonce, guarded) {
        return Err(Denial::Replay);
    }
    Ok(())
}

async fn fetch_snapshot(pool: &sqlx::PgPool) -> Result<Vec<FamilyRow>, sqlx::Error> {
    sqlx::query_as::<_, (String, String, i64)>(
        "SELECT family, cursor, extract(epoch FROM updated_at)::bigint FROM backfill_cursors",
    )
    .fetch_all(pool)
    .await
    .map(|rows| {
        rows.into_iter()
            .map(|(family, cursor, updated_at)| FamilyRow {
                family,
                cursor,
                updated_at,
            })
            .collect()
    })
}

/// Per-family projection: vocabulary keys only (unknown stored families never
/// become exposition labels), ids/counts/timestamps only. See the module docs
/// for the v1 `progress` ruling.
fn families_body(rows: &[FamilyRow]) -> Value {
    let mut known = HashMap::new();
    for row in rows {
        if two_bot_store::backfill_cursors::BACKFILL_FAMILIES.contains(&row.family.as_str()) {
            known.insert(row.family.as_str(), row);
        }
    }
    let mut families = serde_json::Map::new();
    for family in two_bot_store::backfill_cursors::BACKFILL_FAMILIES {
        let (cursor, updated_at) = match known.get(family) {
            Some(row) => (
                Value::String(row.cursor.clone()),
                Value::Number(row.updated_at.into()),
            ),
            None => (Value::Null, Value::Null),
        };
        families.insert(
            (*family).to_owned(),
            json!({
                "progress": 0.0,
                "eta_seconds": null,
                "cursor": cursor,
                "updated_at": updated_at,
            }),
        );
    }
    Value::Object(families)
}

fn refusal(id: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "ok": false,
            "error": {
                "code": "unauthorized",
                "message": AUTH_FAILURE_MESSAGE,
                "retryable": false,
            },
            "request_id": id,
        })),
    )
        .into_response()
}

fn unavailable(id: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "ok": false,
            "error": {
                "code": "unavailable",
                "message": "Backfill status unavailable",
                "retryable": true,
            },
            "request_id": id,
        })),
    )
        .into_response()
}

/// Render the snapshot-read failure line for the 503 path below. One helper
/// owns the step attribution so it is pinned in one place:
/// `snapshot_failure_pins_snapshot_read_step` fails if the site ever renders
/// a different step, where the substring-only wiring pin would stay green.
fn snapshot_store_failure(chain: &str) -> String {
    render_backfill_failure(
        FailureClass::StoreUnavailable,
        BackfillStep::SnapshotRead,
        chain,
    )
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            elapsed.as_millis().try_into().unwrap_or(u64::MAX)
        })
}

async fn handler(
    State((shared, _jobs)): State<(crate::server::SharedState, crate::jobs::SharedStatus)>,
    request: axum::extract::Request,
) -> Response {
    // `State + Request` pair (proven by `internal_action_http::handle` and
    // `server::readyz`): the ring rides in listener state, never an
    // `Extension` layer, so one body-consuming extractor is enough.
    // The std-mutex guard never crosses an await: the whole authenticate
    // section is lexically scoped so the handler future stays Send.
    let (parts, body) = request.into_parts();
    let headers = parts.headers;
    let body_bytes = match axum::body::to_bytes(body, 64 * 1024).await {
        Ok(bytes) => bytes,
        Err(_) => {
            // Fail closed: an unreadable body can never authenticate.
            return refusal(&internal_actions::new_request_id(now_ms(), &rand::random()));
        }
    };
    let id = internal_actions::new_request_id(now_ms(), &rand::random());
    let denial = {
        let ring = &shared.backfill_ring;
        let mut guard = ring
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        authenticate(&mut guard, &headers, &body_bytes, now_ms()).err()
    };
    if let Some(denial) = denial {
        tracing::debug!(
            msg = "ops_backfill_refused",
            reason = denial.as_str(),
            "backfill read refused"
        );
        return refusal(&id);
    }
    match &shared.database {
        None => (
            StatusCode::OK,
            [(header::CACHE_CONTROL, "no-store")],
            Json(json!({
                "ok": true,
                "families": families_body(&[]),
                "request_id": id,
            })),
        )
            .into_response(),
        Some(pool) => match fetch_snapshot(pool).await {
            Ok(rows) => (
                StatusCode::OK,
                [(header::CACHE_CONTROL, "no-store")],
                Json(json!({
                    "ok": true,
                    "families": families_body(&rows),
                    "request_id": id,
                })),
            )
                .into_response(),
            Err(err) => {
                let failure = snapshot_store_failure(crate::db_log::db_class(&err));
                tracing::warn!(
                    msg = "ops_backfill_snapshot_failed",
                    class = FailureClass::StoreUnavailable.as_str(),
                    step = BackfillStep::SnapshotRead.as_str(),
                    failure = %failure,
                    "backfill snapshot unreadable"
                );
                unavailable(&id)
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::sync::RwLock as AsyncRwLock;

    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use sha2::Digest as _;
    use tower::ServiceExt as _;

    use crate::gateway::GatewayState;

    /// Low-entropy fixture secrets (test-only, never credentials): repeating
    /// characters carry no entropy for secret scanners to flag.
    fn fixture_spec() -> String {
        format!("fixture-ops-key:{}-secret", "x".repeat(40))
    }

    fn test_ring() -> SharedRing {
        SharedRing::from_spec(&fixture_spec())
    }

    fn test_app(ring: SharedRing) -> Router {
        Router::new().merge(router()).with_state((
            crate::server::SharedState::new(
                Arc::new(AsyncRwLock::new(GatewayState::Unconfigured)),
                None,
            )
            .with_backfill_ring(ring),
            crate::jobs::statuses(&[], true),
        ))
    }

    fn fixture_secret() -> Vec<u8> {
        format!("{}-secret", "x".repeat(40)).into_bytes()
    }

    /// Mint the route's signature, exactly like the acceptance target does.
    fn signed_headers(secret: &[u8]) -> Vec<(String, String)> {
        signed_headers_for("fixture-ops-key", secret, BACKFILL_PATH)
    }

    fn mint_signature(secret: &[u8], path: &str, timestamp: &str, nonce: &str) -> String {
        let body_hash = hex::encode(sha2::Sha256::digest(b""));
        let canonical = format!("GET\n{path}\n{timestamp}\n{nonce}\n{body_hash}");
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret).expect("test HMAC key");
        mac.update(canonical.as_bytes());
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    fn fresh_timestamp() -> String {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("test clock")
            .as_secs()
            .to_string()
    }

    fn signed_headers_for(key_id: &str, secret: &[u8], path: &str) -> Vec<(String, String)> {
        signed_headers_for_at(key_id, secret, path, &fresh_timestamp())
    }

    fn signed_headers_for_at(
        key_id: &str,
        secret: &[u8],
        path: &str,
        timestamp: &str,
    ) -> Vec<(String, String)> {
        let nonce = format!("{:032x}", rand::random::<u128>());
        let signature = mint_signature(secret, path, timestamp, &nonce);
        vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            ("x-two-key-id".to_owned(), key_id.to_owned()),
            ("x-two-timestamp".to_owned(), timestamp.to_owned()),
            ("x-two-nonce".to_owned(), nonce),
            ("x-two-signature".to_owned(), signature),
        ]
    }

    async fn get(app: &Router, headers: Vec<(String, String)>) -> (StatusCode, Value) {
        let mut request = Request::builder().uri(BACKFILL_PATH).method("GET");
        for (name, value) in &headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::empty()).expect("test request"))
            .await
            .expect("oneshot");
        let status = response.status();
        let body = to_bytes(response.into_body(), 65_536).await.expect("body");
        (status, serde_json::from_slice(&body).expect("JSON body"))
    }

    fn all_keys(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                for (key, nested) in map {
                    out.push(key.clone());
                    all_keys(nested, out);
                }
            }
            Value::Array(items) => {
                for item in items {
                    all_keys(item, out);
                }
            }
            _ => {}
        }
    }

    fn all_strings(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::String(text) => out.push(text.clone()),
            Value::Object(map) => {
                for nested in map.values() {
                    all_strings(nested, out);
                }
            }
            Value::Array(items) => {
                for item in items {
                    all_strings(item, out);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn snapshot_failure_pins_snapshot_read_step() {
        // The 503 path renders through `snapshot_store_failure`, so this pins
        // the wired step: a mutant swapping in any other step fails here,
        // where the substring-only wiring pin would stay green.
        let line = snapshot_store_failure("pool_timeout");
        assert!(
            line.contains("step=snapshot_read"),
            "snapshot 503 must attribute SnapshotRead: {line:?}"
        );
        for other in [
            "rate_limit_client",
            "cursor_checkpoint",
            "audit_catch_up",
            "history_put",
            "history_count",
        ] {
            assert!(
                !line.contains(other),
                "snapshot 503 must not attribute {other}: {line:?}"
            );
        }
    }

    #[test]
    fn admin_service_key_cannot_authenticate_a_legacy_backfill_read() {
        let secret = fixture_secret();
        let key_id = "fixture-admin-service";
        let ring = SharedRing::new(vec![two_bot_core::internal_actions::SigningKey {
            id: key_id.to_owned(),
            secret: two_bot_core::Secret::new(secret.clone()),
        }]);
        let mut headers = HeaderMap::new();
        for (name, value) in signed_headers_for(key_id, &secret, BACKFILL_PATH) {
            headers.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(&value).unwrap(),
            );
        }
        let mut inner = ring.0.lock().unwrap();
        assert!(authenticate(&mut inner, &headers, b"", now_ms()).is_err());
    }

    #[tokio::test]
    async fn signed_read_reports_empty_baseline() {
        let app = test_app(test_ring());
        let (status, body) = get(&app, signed_headers(&fixture_secret())).await;
        assert_eq!(status, StatusCode::OK);
        let families = body["families"].as_object().expect("families map");
        let mut seen: Vec<&str> = families.keys().map(String::as_str).collect();
        seen.sort_unstable();
        let mut expected: Vec<&str> = two_bot_store::backfill_cursors::BACKFILL_FAMILIES.to_vec();
        expected.sort_unstable();
        assert_eq!(seen, expected);
        for family in two_bot_store::backfill_cursors::BACKFILL_FAMILIES {
            assert_eq!(families[*family]["progress"], 0.0);
            assert!(families[*family]["eta_seconds"].is_null());
            assert!(families[*family]["cursor"].is_null());
            assert!(families[*family]["updated_at"].is_null());
        }
        assert!(!body["request_id"].as_str().unwrap_or_default().is_empty());
    }

    #[tokio::test]
    async fn bare_and_wrong_key_share_one_refusal() {
        let app = test_app(test_ring());
        let (bare_status, bare) = get(&app, Vec::new()).await;
        assert_eq!(bare_status, StatusCode::UNAUTHORIZED);
        let other_secret = format!("{}-wrong", "y".repeat(40)).into_bytes();
        let wrong = signed_headers_for("fixture-unknown-key", &other_secret, BACKFILL_PATH);
        let (wrong_status, refused) = get(&app, wrong).await;
        assert_eq!(wrong_status, StatusCode::UNAUTHORIZED);
        assert_eq!(bare["error"], refused["error"]);
        assert_eq!(bare["error"]["code"], "unauthorized");
        assert_eq!(bare["error"]["message"], AUTH_FAILURE_MESSAGE);
        assert!(bare.get("families").is_none());
    }

    #[tokio::test]
    async fn actions_signature_never_authorizes_a_backfill_read() {
        // The canonical string is path-bound: a signature cut for the
        // receiver's path must refuse here even under a known key id.
        let app = test_app(test_ring());
        let forged = signed_headers_for("fixture-ops-key", &fixture_secret(), "/internal/actions");
        let (status, body) = get(&app, forged).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "unauthorized");
    }

    #[tokio::test]
    async fn malformed_and_stale_signatures_refuse() {
        let app = test_app(test_ring());
        // A nonce outside the 32-hex shape refuses before any HMAC runs.
        let mut malformed = signed_headers(&fixture_secret());
        if let Some(nonce) = malformed.iter_mut().find(|(name, _)| name == "x-two-nonce") {
            nonce.1 = "not-hex".to_owned();
        }
        let (malformed_status, malformed_body) = get(&app, malformed).await;
        assert_eq!(malformed_status, StatusCode::UNAUTHORIZED);
        assert_eq!(malformed_body["error"]["code"], "unauthorized");
        // A correctly signed but day-old capture refuses like any other denial.
        let past = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("test clock")
            .as_secs()
            - 86_400;
        let stale = signed_headers_for_at(
            "fixture-ops-key",
            &fixture_secret(),
            BACKFILL_PATH,
            &past.to_string(),
        );
        let (stale_status, stale_body) = get(&app, stale).await;
        assert_eq!(stale_status, StatusCode::UNAUTHORIZED);
        assert_eq!(stale_body["error"]["code"], "unauthorized");
    }

    #[tokio::test]
    async fn replayed_nonce_refuses() {
        let app = test_app(test_ring());
        let headers = signed_headers(&fixture_secret());
        let (first, _) = get(&app, headers.clone()).await;
        assert_eq!(first, StatusCode::OK);
        let (second, body) = get(&app, headers).await;
        assert_eq!(second, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "unauthorized");
    }

    #[tokio::test]
    async fn regressed_clock_burns_nonce_at_guarded_mark() {
        // A 3 s regression sits inside the 5 s clock tolerance, so freshness
        // still evaluates against the high-water mark. The nonce must burn at
        // that mark: burning at the regressed read would expire the entry
        // early and reopen a replay window for a capture near the +120 s edge.
        fn headers_for(nonce: &str, timestamp: &str, signature: &str) -> HeaderMap {
            let mut headers = HeaderMap::new();
            headers.insert("x-two-key-id", "fixture-ops-key".parse().expect("key id"));
            headers.insert("x-two-timestamp", timestamp.parse().expect("timestamp"));
            headers.insert("x-two-nonce", nonce.parse().expect("nonce"));
            headers.insert("x-two-signature", signature.parse().expect("signature"));
            headers
        }

        let ring = test_ring();
        // Millisecond clock with whole-second boundary for exact skew math.
        let base_ms = 1_700_000_000_000u64;
        // Near the +120 s future edge, so the capture stays fresh at replay.
        let timestamp = ((base_ms / 1000) + 119).to_string();
        let mut guard = ring
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // First read advances the high-water mark to base_ms.
        // Random nonces only: hard-coded hex would trip the crypto-value gate.
        let first_nonce = format!("{:032x}", rand::random::<u128>());
        let first_sig = mint_signature(&fixture_secret(), BACKFILL_PATH, &timestamp, &first_nonce);
        authenticate(
            &mut guard,
            &headers_for(&first_nonce, &timestamp, &first_sig),
            b"",
            base_ms,
        )
        .expect("first read burns");
        // Second read with the clock 3 s behind: freshness still passes
        // against the mark, and the nonce must burn at the mark.
        let regressed_ms = base_ms - 3_000;
        let second_nonce = loop {
            let candidate = format!("{:032x}", rand::random::<u128>());
            if candidate != first_nonce {
                break candidate;
            }
        };
        let second_sig =
            mint_signature(&fixture_secret(), BACKFILL_PATH, &timestamp, &second_nonce);
        let second_headers = headers_for(&second_nonce, &timestamp, &second_sig);
        authenticate(&mut guard, &second_headers, b"", regressed_ms).expect("regressed read burns");
        // Just past the regressed entry's TTL but inside the guarded entry's:
        // the capture must still refuse as a replay.
        let replay_ms = base_ms + 238_001;
        let replay = authenticate(&mut guard, &second_headers, b"", replay_ms);
        assert!(
            matches!(replay, Err(Denial::Replay)),
            "regressed nonce must stay live on the guarded timeline"
        );
    }

    #[tokio::test]
    async fn projection_filters_unknown_families_and_keeps_frontier() {
        let rows = vec![
            FamilyRow {
                family: "audit".to_owned(),
                cursor: "123456789".to_owned(),
                updated_at: 1_700_000_000,
            },
            FamilyRow {
                family: "messages".to_owned(),
                cursor: "999".to_owned(),
                updated_at: 1_700_000_001,
            },
        ];
        let body = families_body(&rows);
        let families = body.as_object().expect("families map");
        assert_eq!(families.len(), 9);
        assert!(families.get("messages").is_none());
        assert_eq!(families["audit"]["cursor"], "123456789");
        assert_eq!(families["audit"]["updated_at"], 1_700_000_000);
        assert_eq!(families["audit"]["progress"], 0.0);
        assert!(families["members"]["cursor"].is_null());
    }

    #[tokio::test]
    async fn exposition_keys_and_strings_stay_vocabulary_shaped() {
        let app = test_app(test_ring());
        let (_, first) = get(&app, signed_headers(&fixture_secret())).await;
        let (_, second) = get(&app, signed_headers(&fixture_secret())).await;
        let mut first_keys = Vec::new();
        let mut second_keys = Vec::new();
        all_keys(&first, &mut first_keys);
        all_keys(&second, &mut second_keys);
        first_keys.sort();
        second_keys.sort();
        assert_eq!(first_keys, second_keys);
        for key in &first_keys {
            assert!(
                !key.bytes().any(|b| b.is_ascii_digit()),
                "exposition key {key:?} carries value-shaped content"
            );
            assert!(
                key.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "exposition key {key:?} must be fixed vocabulary"
            );
            assert!(
                ![
                    "message",
                    "body",
                    "text",
                    "content",
                    "status_text",
                    "custom_status",
                    "subject"
                ]
                .contains(&key.as_str()),
                "per-user field {key:?} must never be exposed"
            );
        }
        let mut strings = Vec::new();
        all_strings(&first, &mut strings);
        assert!(!strings.is_empty(), "body must carry id/count content");
        for text in &strings {
            assert!(
                !text.chars().any(|c| c.is_whitespace()),
                "free-text content {text:?} must never be exposed"
            );
        }
    }

    /// Paperclip Review #927: the configured-database branch is pinned by a
    /// real Postgres. Seeds one family and checks its cursor/updated_at come
    /// back verbatim while untouched families stay null. Every other test
    /// runs database-None and only exercises the empty baseline.
    #[tokio::test]
    #[ignore = "requires guarded agent-testdb; ignored-db-runtime runs explicitly"]
    async fn configured_database_reports_seeded_family_frontier() {
        let url =
            std::env::var("TWO_TEST_DATABASE_URL").expect("explicit guarded test URL required");
        let db = two_bot_testsupport::TestDatabase::create(
            &url,
            &sqlx::migrate!("../cutover/migrations"),
        )
        .await
        .expect("create guarded test fixture");
        sqlx::query(
            "INSERT INTO backfill_cursors (family, cursor)
             VALUES ('members', '222222222222222222')",
        )
        .execute(db.pool())
        .await
        .expect("seed members frontier");
        let app = Router::new().merge(router()).with_state((
            crate::server::SharedState::new(
                Arc::new(AsyncRwLock::new(GatewayState::Unconfigured)),
                Some(db.pool().clone()),
            )
            .with_backfill_ring(test_ring()),
            crate::jobs::statuses(&[], true),
        ));
        let (status, body) = get(&app, signed_headers(&fixture_secret())).await;
        assert_eq!(status, StatusCode::OK);
        let families = body["families"].as_object().expect("families map");
        assert_eq!(families["members"]["cursor"], "222222222222222222");
        assert!(
            families["members"]["updated_at"].as_i64().unwrap_or(0) > 0,
            "seeded frontier must carry its write instant"
        );
        assert!(
            families["audit"]["cursor"].is_null(),
            "untouched families stay null"
        );
    }

    /// Paperclip Review #927: a configured-but-unreadable database is a 503
    /// with no-store, never a zeroed 200. The lazy pool points at a refused
    /// loopback port, so the first query fails at once and no database is
    /// needed.
    #[tokio::test]
    async fn unreadable_database_is_503_never_zeroed_200() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://127.0.0.1:1/unreadable")
            .expect("lazy pool");
        let app = Router::new().merge(router()).with_state((
            crate::server::SharedState::new(
                Arc::new(AsyncRwLock::new(GatewayState::Unconfigured)),
                Some(pool),
            )
            .with_backfill_ring(test_ring()),
            crate::jobs::statuses(&[], true),
        ));
        let mut request = Request::builder().uri(BACKFILL_PATH).method("GET");
        for (name, value) in &signed_headers(&fixture_secret()) {
            request = request.header(name.as_str(), value.as_str());
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::empty()).expect("test request"))
            .await
            .expect("oneshot");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response
                .headers()
                .get("cache-control")
                .map(|v| v.to_str().unwrap_or("")),
            Some("no-store"),
            "the 503 must not be cached"
        );
        let body = to_bytes(response.into_body(), 65_536).await.expect("body");
        let json: Value = serde_json::from_slice(&body).expect("JSON body");
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "unavailable");
    }
}
