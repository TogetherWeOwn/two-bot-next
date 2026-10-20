//! Authenticated per-delivery refresh of the Twitch webhook secret (staging).
//!
//! The container's process environment is fixed at startup, so a rotated
//! Operator secret cannot reach the running receiver by re-reading it. Instead
//! the Durable Object attaches the current Worker secret to every EventSub
//! delivery it forwards, authenticated by a dedicated Worker-to-receiver HMAC
//! that is independent of Twitch's delivery signature. The receiver validates
//! the refresh, atomically swaps its in-memory [`WebhookSecret`] snapshot, and
//! only then verifies the delivery — all without dropping the gateway or the
//! receiver socket.
//!
//! Refresh authentication (this module):
//!
//! - The four `x-two-eventsub-*` headers carry the candidate secret plus a
//!   timestamp, a single-use nonce and an HMAC. They are never part of Twitch's
//!   delivery allowlist: the Worker's edge ingress refuses any caller-supplied
//!   value for them, and only the fenced DO attaches server-side values.
//! - [`canonical_string`] binds the refresh to the request: method, fixed
//!   callback path, timestamp, nonce, the flow's `x-request-id`, and the
//!   SHA-256 hex of the candidate secret. It mirrors `signEventsubRefresh` in
//!   `wrangler/src/eventsub.ts` byte for byte; a mismatch fails closed.
//! - [`refresh_before_verify`] authenticates (constant-time HMAC), evaluates
//!   the freshness clock against a [`ClockGuard`] high-water mark (threat-model
//!   F8, as internal actions do), checks freshness ([`SKEW_SECONDS`], same
//!   window as internal actions), burns the nonce ([`NONCE_TTL_SECONDS`]
//!   memory at the guarded time, so a replay — including after a swept nonce
//!   plus clock rollback — can never re-arm an old secret), validates the
//!   candidate as a [`WebhookSecret`], and swaps the
//!   complete snapshot under one write lock. Every failure maps to a bare
//!   [`RefreshRejection`] variant: no secret, digest, key id or header value
//!   travels with the error, and the caller answers one generic retryable
//!   refusal without verifying against the stale snapshot.
//!
//! Each request swaps-then-verifies, so concurrent deliveries each use one
//! consistent snapshot: a delivery whose secret was superseded mid-flight is a
//! deterministic 401, never a 500, and the Twitch retry lands on the new
//! snapshot. A poisoned lock fails closed ([`RefreshRejection::Unavailable`])
//! rather than panicking the request path.

use std::sync::{Mutex, RwLock};

use hmac::{Hmac, KeyInit, Mac as _};
use sha2::{Digest as _, Sha256};

use crate::{
    clock_guard::ClockGuard,
    internal_actions::{
        signatures_match, valid_nonce_format, within_skew, NonceCache, NONCE_TTL_SECONDS,
        SKEW_SECONDS,
    },
    secret::Secret,
    twitch_eventsub::WebhookSecret,
};

/// Fixed callback path the canonical string signs. Mirrors
/// `EVENTSUB_PATH` in `crates/bot/src/eventsub_http.rs` and in
/// `wrangler/src/eventsub.ts`; the three must stay identical.
const EVENTSUB_CALLBACK_PATH: &str = "/hooks/twitch";

/// Candidate secret material, attached by the fenced DO from the Worker's
/// live secret binding. Never caller-supplied (the edge ingress refuses it).
pub const HEADER_SECRET: &str = "x-two-eventsub-secret";
/// Unix seconds the DO signed this refresh, ASCII digits.
pub const HEADER_TIMESTAMP: &str = "x-two-eventsub-timestamp";
/// Single-use 32 lowercase-or-uppercase hex characters.
pub const HEADER_NONCE: &str = "x-two-eventsub-nonce";
/// `sha256=` + hex(HMAC-SHA256(reload key, canonical)).
pub const HEADER_SIGNATURE: &str = "x-two-eventsub-signature";

const SIGNATURE_PREFIX: &str = "sha256=";
const SIGNATURE_HEX_LEN: usize = 64;

/// `sha256` of the candidate secret bytes, hex. The canonical string signs
/// this digest rather than the secret so the signed text stays fixed-shape;
/// the digest itself never reaches logs, traces or errors.
#[must_use]
pub fn secret_sha_hex(secret: &[u8]) -> String {
    hex::encode(Sha256::digest(secret))
}

/// `POST\n/hooks/twitch\n{timestamp}\n{nonce}\n{request_id}\n{sha256_hex(secret)}`.
///
/// Binds the refresh to one receiver operation: the timestamp and nonce give
/// freshness plus replay protection, the request id ties it to the delivery
/// being verified, and the secret digest ties it to the exact candidate.
/// Must match `signEventsubRefresh` in `wrangler/src/eventsub.ts` exactly.
#[must_use]
pub fn canonical_string(
    timestamp: &str,
    nonce: &str,
    request_id: &str,
    secret_sha_hex: &str,
) -> String {
    format!("POST\n{EVENTSUB_CALLBACK_PATH}\n{timestamp}\n{nonce}\n{request_id}\n{secret_sha_hex}")
}

/// `sha256=` + hex(HMAC-SHA256(key, canonical)).
#[must_use]
pub fn sign(key: &[u8], timestamp: &str, nonce: &str, request_id: &str, secret: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts keys of any length");
    mac.update(canonical_string(timestamp, nonce, request_id, &secret_sha_hex(secret)).as_bytes());
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// Why a refresh was refused. Bare variants only: nothing secret-bearing,
/// and no digest of secret-bearing material, ever travels with the error.
/// Callers map every variant to one generic retryable refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshRejection {
    /// A refresh header is missing or empty.
    MissingHeader,
    /// A nonce or signature is malformed.
    MalformedHeader,
    /// The HMAC does not verify under the reload key.
    BadSignature,
    /// The signed timestamp is outside the freshness window.
    Stale,
    /// The nonce was already burned.
    Replay,
    /// The candidate fails [`WebhookSecret`] validation (10-100 graphic ASCII).
    InvalidSecret,
    /// A lock was poisoned; fail closed rather than serve a torn snapshot.
    Unavailable,
}

/// Verifies per-delivery refresh headers under the Operator-provisioned
/// reload key (`TWO_EVENTSUB_RELOAD_KEY`, at least 32 bytes, read once at
/// startup like every other container secret).
///
/// No derived `Debug`: the hand-written impl names only the key length and
/// the live nonce count — safe scalars, never key material, secrets,
/// nonces or digests.
pub struct RefreshVerifier {
    key: Secret<Vec<u8>>,
    nonces: Mutex<NonceCache>,
    /// Fail-closed freshness clock (threat-model F8): nonce memory drops
    /// entries past [`NONCE_TTL_SECONDS`], so expiry followed by wall-clock
    /// rollback could otherwise reopen a signed window whose nonce is already
    /// forgotten. Freshness and the nonce burn both decide at the guarded
    /// time, never at a regressed reading.
    clock: Mutex<ClockGuard>,
}

impl std::fmt::Debug for RefreshVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RefreshVerifier")
            .field("key_len", &self.key.expose().len())
            .field(
                "pending_nonces",
                &self.nonces.lock().map(|c| c.len()).unwrap_or(0),
            )
            .finish()
    }
}

impl RefreshVerifier {
    /// Build from the validated startup key. The nonce memory covers the
    /// whole freshness window ([`NONCE_TTL_SECONDS`] over [`SKEW_SECONDS`]),
    /// so a nonce stays burned at every instant its timestamp is still fresh,
    /// and the [`ClockGuard`] high-water mark keeps it burned across clock
    /// rollback: freshness and the burn both decide at the guarded time.
    #[must_use]
    pub fn new(key: Secret<Vec<u8>>) -> Self {
        Self {
            key,
            nonces: Mutex::new(NonceCache::new(NONCE_TTL_SECONDS)),
            clock: Mutex::new(ClockGuard::new()),
        }
    }
}

/// Authenticate the DO-attached refresh headers and atomically replace
/// `current` before the caller verifies the delivery.
///
/// Order: presence → nonce/signature shape → HMAC (constant-time) →
/// clock-guard evaluation → freshness → nonce burn → secret validation →
/// swap. Only an authenticated, fresh, unreplayed, valid candidate swaps;
/// anything else leaves `current` untouched for the caller to refuse
/// generically. The swap holds one write lock, so concurrent requests each
/// verify against one complete snapshot.
pub fn refresh_before_verify<'headers>(
    verifier: &RefreshVerifier,
    current: &RwLock<WebhookSecret>,
    header: impl Fn(&str) -> Option<&'headers str>,
    request_id: &str,
    now_ms: u64,
) -> Result<(), RefreshRejection> {
    let secret = header(HEADER_SECRET)
        .filter(|value| !value.is_empty())
        .ok_or(RefreshRejection::MissingHeader)?;
    let timestamp = header(HEADER_TIMESTAMP)
        .filter(|value| !value.is_empty())
        .ok_or(RefreshRejection::MissingHeader)?;
    let nonce = header(HEADER_NONCE)
        .filter(|value| !value.is_empty())
        .ok_or(RefreshRejection::MissingHeader)?;
    let signature = header(HEADER_SIGNATURE)
        .filter(|value| !value.is_empty())
        .ok_or(RefreshRejection::MissingHeader)?;
    if !valid_nonce_format(nonce)
        || !signature.starts_with(SIGNATURE_PREFIX)
        || signature.len() != SIGNATURE_PREFIX.len() + SIGNATURE_HEX_LEN
    {
        return Err(RefreshRejection::MalformedHeader);
    }
    // Authenticate before freshness, as internal actions do: a forged
    // request must not interact with time or replay state.
    let expected = sign(
        verifier.key.expose(),
        timestamp,
        nonce,
        request_id,
        secret.as_bytes(),
    );
    if !signatures_match(&expected, signature) {
        return Err(RefreshRejection::BadSignature);
    }
    // Evaluate the freshness clock against the high-water mark before
    // touching time or replay state, as internal actions do: a forged
    // request must not move the mark, and a regressed reading must never
    // decide freshness. Within-tolerance samples decide at the mark;
    // rollback past the tolerance refuses fail-closed.
    let guarded_ms = {
        let mut clock = match verifier.clock.lock() {
            Ok(clock) => clock,
            // Poisoned clock state fails closed; the guard is unrecoverable
            // and must not travel with the error.
            Err(_) => return Err(RefreshRejection::Unavailable),
        };
        match clock.evaluate(now_ms) {
            Ok(guarded) => guarded,
            Err(_) => return Err(RefreshRejection::Stale),
        }
    };
    if !within_skew(timestamp, SKEW_SECONDS, guarded_ms / 1000) {
        return Err(RefreshRejection::Stale);
    }
    // Burn only authenticated, fresh nonces at the guarded time: a replay can
    // never re-arm an old secret, and a forged nonce cannot deny a legitimate
    // one.
    let fresh = match verifier.nonces.lock() {
        Ok(mut cache) => cache.offer(nonce, guarded_ms),
        // Poisoned replay state fails closed; the guard is unrecoverable and
        // must not travel with the error.
        Err(_) => return Err(RefreshRejection::Unavailable),
    };
    if !fresh {
        return Err(RefreshRejection::Replay);
    }
    let next = match WebhookSecret::new(secret.to_owned()) {
        Ok(next) => next,
        Err(_) => return Err(RefreshRejection::InvalidSecret),
    };
    match current.write() {
        Ok(mut guard) => {
            *guard = next;
            Ok(())
        }
        Err(_) => Err(RefreshRejection::Unavailable),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const NOW_MS: u64 = 1_760_000_000_000;
    const REQUEST_ID: &str = "synthetic-request-0001";

    /// Synthetic keys and secrets come from fresh random bytes. Fixed material
    /// belongs only to the shared, public known-answer fixture below.
    fn fresh_material() -> String {
        hex::encode(rand::random::<[u8; 32]>())
    }

    fn verifier() -> (RefreshVerifier, Vec<u8>) {
        let key = fresh_material().into_bytes();
        assert!(key.len() >= 32, "fixture reload key must clear the floor");
        (RefreshVerifier::new(Secret::new(key.clone())), key)
    }

    fn lock_for(secret: &str) -> RwLock<WebhookSecret> {
        RwLock::new(WebhookSecret::new(secret.to_owned()).expect("fixture secret must parse"))
    }

    fn fresh_nonce() -> String {
        hex::encode(rand::random::<[u8; 16]>())
    }

    /// Validly signed refresh headers for `secret` under `key`.
    fn signed_headers(
        key: &[u8],
        secret: &str,
        timestamp_secs: u64,
        nonce: &str,
    ) -> HashMap<String, String> {
        let timestamp = timestamp_secs.to_string();
        let signature = sign(key, &timestamp, nonce, REQUEST_ID, secret.as_bytes());
        HashMap::from([
            (HEADER_SECRET.to_owned(), secret.to_owned()),
            (HEADER_TIMESTAMP.to_owned(), timestamp),
            (HEADER_NONCE.to_owned(), nonce.to_owned()),
            (HEADER_SIGNATURE.to_owned(), signature),
        ])
    }

    fn lookup<'headers>(
        headers: &'headers HashMap<String, String>,
    ) -> impl Fn(&str) -> Option<&'headers str> {
        move |name| headers.get(name).map(String::as_str)
    }

    fn now_secs() -> u64 {
        NOW_MS / 1000
    }

    #[test]
    fn canonical_string_pins_the_worker_contract() {
        // Byte-exact mirror of `signEventsubRefresh` in
        // `wrangler/src/eventsub.ts`: method, fixed path, timestamp, nonce,
        // request id, secret digest. Any drift fails closed on both sides.
        let nonce = fresh_nonce();
        assert_eq!(
            canonical_string("1720000000", &nonce, REQUEST_ID, "ab12"),
            format!("POST\n/hooks/twitch\n1720000000\n{nonce}\n{REQUEST_ID}\nab12")
        );
        assert_eq!(EVENTSUB_CALLBACK_PATH, "/hooks/twitch");
        assert_eq!(secret_sha_hex(b"abc").len(), 64);
    }

    #[test]
    fn signature_matches_the_worker_known_answer_vector() {
        // Both languages load the same public synthetic vector. Its expected
        // signature is fixed test data, never computed by the implementation.
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct KnownAnswer {
            reload_key: String,
            timestamp: String,
            nonce: String,
            request_id: String,
            secret_parts: Vec<String>,
            signature: String,
        }
        let fixture = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/eventsub_refresh.json"
        ))
        .expect("shared EventSub refresh vector must be readable");
        let vector: KnownAnswer =
            serde_json::from_str(&fixture).expect("shared EventSub refresh vector must parse");
        let signature = sign(
            vector.reload_key.as_bytes(),
            &vector.timestamp,
            &vector.nonce,
            &vector.request_id,
            vector.secret_parts.join("-").as_bytes(),
        );
        assert_eq!(signature, vector.signature);
    }

    #[test]
    fn valid_refresh_atomically_replaces_the_snapshot() {
        let (verifier, key) = verifier();
        let old = fresh_material();
        let new = fresh_material();
        let current = lock_for(&old);
        let headers = signed_headers(&key, &new, now_secs(), &fresh_nonce());

        assert_eq!(
            refresh_before_verify(&verifier, &current, lookup(&headers), REQUEST_ID, NOW_MS),
            Ok(())
        );
        assert_eq!(
            *current.read().expect("snapshot lock must be readable"),
            WebhookSecret::new(new).expect("rotated secret must parse")
        );
    }

    #[test]
    fn each_missing_or_empty_header_leaves_the_snapshot() {
        let (verifier, key) = verifier();
        let old = fresh_material();
        let new = fresh_material();
        let base = signed_headers(&key, &new, now_secs(), &fresh_nonce());
        for name in [
            HEADER_SECRET,
            HEADER_TIMESTAMP,
            HEADER_NONCE,
            HEADER_SIGNATURE,
        ] {
            for headers in [
                {
                    let mut dropped = base.clone();
                    dropped.remove(name);
                    dropped
                },
                {
                    let mut emptied = base.clone();
                    emptied.insert(name.to_owned(), String::new());
                    emptied
                },
            ] {
                let current = lock_for(&old);
                assert_eq!(
                    refresh_before_verify(
                        &verifier,
                        &current,
                        lookup(&headers),
                        REQUEST_ID,
                        NOW_MS
                    ),
                    Err(RefreshRejection::MissingHeader),
                    "{name}"
                );
                assert_eq!(
                    *current.read().expect("snapshot lock must be readable"),
                    WebhookSecret::new(old.clone()).expect("fixture secret must parse"),
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn malformed_nonce_or_signature_shape_is_rejected() {
        let (verifier, key) = verifier();
        let old = fresh_material();
        let new = fresh_material();
        let base = signed_headers(&key, &new, now_secs(), &fresh_nonce());
        let mut bad_nonce = base.clone();
        bad_nonce.insert(HEADER_NONCE.to_owned(), "not-hex".to_owned());
        let mut short_nonce = base.clone();
        short_nonce.insert(HEADER_NONCE.to_owned(), "ab12".to_owned());
        let mut bad_prefix = base.clone();
        bad_prefix.insert(
            HEADER_SIGNATURE.to_owned(),
            format!("md5={}", "a".repeat(SIGNATURE_HEX_LEN)),
        );
        let mut short_sig = base.clone();
        short_sig.insert(HEADER_SIGNATURE.to_owned(), "sha256=ab12".to_owned());
        for (label, headers) in [
            ("bad-nonce", bad_nonce),
            ("short-nonce", short_nonce),
            ("bad-prefix", bad_prefix),
            ("short-signature", short_sig),
        ] {
            let current = lock_for(&old);
            assert_eq!(
                refresh_before_verify(&verifier, &current, lookup(&headers), REQUEST_ID, NOW_MS),
                Err(RefreshRejection::MalformedHeader),
                "{label}"
            );
        }
    }

    #[test]
    fn tampering_any_signed_field_breaks_the_hmac() {
        let (other_verifier, _) = verifier();
        let (verifier, key) = verifier();
        let old = fresh_material();
        let new = fresh_material();
        let base = signed_headers(&key, &new, now_secs(), &fresh_nonce());
        let other = fresh_material();
        let retimestamped = signed_headers(&key, &new, now_secs() - 30, &fresh_nonce());
        let cases: Vec<(&str, HashMap<String, String>)> = vec![
            ("secret", {
                let mut tampered = base.clone();
                tampered.insert(HEADER_SECRET.to_owned(), other);
                tampered
            }),
            ("timestamp", {
                let mut tampered = base.clone();
                tampered.insert(HEADER_TIMESTAMP.to_owned(), (now_secs() - 30).to_string());
                tampered
            }),
            ("nonce", {
                let mut tampered = base.clone();
                tampered.insert(HEADER_NONCE.to_owned(), fresh_nonce());
                tampered
            }),
            ("signature-bit", {
                let mut tampered = base.clone();
                let mut signature = base[HEADER_SIGNATURE].clone();
                let last = signature.pop().expect("signature is non-empty");
                signature.push(if last == 'a' { 'b' } else { 'a' });
                tampered.insert(HEADER_SIGNATURE.to_owned(), signature);
                tampered
            }),
        ];
        for (label, headers) in cases {
            let current = lock_for(&old);
            assert_eq!(
                refresh_before_verify(&verifier, &current, lookup(&headers), REQUEST_ID, NOW_MS),
                Err(RefreshRejection::BadSignature),
                "{label}"
            );
        }
        // A signature for another request id does not verify here.
        let current = lock_for(&old);
        assert_eq!(
            refresh_before_verify(
                &verifier,
                &current,
                lookup(&retimestamped),
                "synthetic-request-0002",
                NOW_MS
            ),
            Err(RefreshRejection::BadSignature)
        );
        // The original request id and correct key still verify.
        let current = lock_for(&old);
        assert_eq!(
            refresh_before_verify(
                &verifier,
                &current,
                lookup(&retimestamped),
                REQUEST_ID,
                NOW_MS
            ),
            Ok(())
        );
        // A fresh signature does not verify under another key.
        let signed_with_original_key = signed_headers(&key, &new, now_secs(), &fresh_nonce());
        let current = lock_for(&old);
        assert_eq!(
            refresh_before_verify(
                &other_verifier,
                &current,
                lookup(&signed_with_original_key),
                REQUEST_ID,
                NOW_MS
            ),
            Err(RefreshRejection::BadSignature)
        );
    }

    #[test]
    fn freshness_window_matches_internal_actions() {
        let (verifier, key) = verifier();
        let old = fresh_material();
        let new = fresh_material();
        // Whole-second skew, both directions: ±120 s verifies, ±121 s is stale.
        for (label, sent, fresh) in [
            ("now", now_secs(), true),
            ("past-edge", now_secs() - SKEW_SECONDS, true),
            ("past-outside", now_secs() - SKEW_SECONDS - 1, false),
            ("future-edge", now_secs() + SKEW_SECONDS, true),
            ("future-outside", now_secs() + SKEW_SECONDS + 1, false),
        ] {
            // A non-numeric timestamp is not fresh (same rule as internal actions).
            let headers = signed_headers(&key, &new, sent, &fresh_nonce());
            let current = lock_for(&old);
            let outcome =
                refresh_before_verify(&verifier, &current, lookup(&headers), REQUEST_ID, NOW_MS);
            assert_eq!(outcome.is_ok(), fresh, "{label}");
            if !fresh {
                assert_eq!(outcome, Err(RefreshRejection::Stale), "{label}");
            }
        }
        let nonce = fresh_nonce();
        let mut garbage = signed_headers(&key, &new, now_secs(), &nonce);
        // Re-sign over the garbage timestamp using the same nonce so the
        // failure is freshness, not HMAC.
        let signature = sign(&key, "not-a-time", &nonce, REQUEST_ID, new.as_bytes());
        garbage.insert(HEADER_TIMESTAMP.to_owned(), "not-a-time".to_owned());
        garbage.insert(HEADER_SIGNATURE.to_owned(), signature);
        let current = lock_for(&old);
        assert_eq!(
            refresh_before_verify(&verifier, &current, lookup(&garbage), REQUEST_ID, NOW_MS),
            Err(RefreshRejection::Stale)
        );
    }

    #[test]
    fn replayed_nonce_cannot_rearm_an_old_secret() {
        let (verifier, key) = verifier();
        let old = fresh_material();
        let new = fresh_material();
        let current = lock_for(&old);
        let first = signed_headers(&key, &new, now_secs(), &fresh_nonce());
        assert_eq!(
            refresh_before_verify(&verifier, &current, lookup(&first), REQUEST_ID, NOW_MS),
            Ok(())
        );
        // Same signed refresh replayed: refused, snapshot stays on the new secret.
        assert_eq!(
            refresh_before_verify(&verifier, &current, lookup(&first), REQUEST_ID, NOW_MS),
            Err(RefreshRejection::Replay)
        );
        assert_eq!(
            *current.read().expect("snapshot lock must be readable"),
            WebhookSecret::new(new.clone()).expect("rotated secret must parse")
        );
        // A replay of the *old* secret under a fresh nonce still swaps back:
        // nonces prove freshness, not direction — rotation safety comes from
        // the Worker sending only its current secret, and the DO fence admits
        // only the owning version. Pin that the mechanism is direction-blind.
        let rollback = signed_headers(&key, &old, now_secs(), &fresh_nonce());
        assert_eq!(
            refresh_before_verify(&verifier, &current, lookup(&rollback), REQUEST_ID, NOW_MS),
            Ok(())
        );
    }

    #[test]
    fn clock_rollback_after_nonce_sweep_cannot_rearm_an_old_secret() {
        // Threat-model F8: nonce memory drops entries past NONCE_TTL_SECONDS,
        // so expiry followed by wall-clock rollback could reopen a signed
        // window whose nonce is already forgotten. The ClockGuard high-water
        // mark closes that gap: both freshness and the burn decide at the
        // guarded time, and rollback past the tolerance refuses fail-closed.
        let (verifier, key) = verifier();
        let old = fresh_material();
        let new = fresh_material();
        let current = lock_for(&old);
        let first = signed_headers(&key, &new, now_secs(), &fresh_nonce());
        assert_eq!(
            refresh_before_verify(&verifier, &current, lookup(&first), REQUEST_ID, NOW_MS),
            Ok(())
        );
        // A later refresh advances the high-water mark and sweeps the first
        // nonce from memory (300 s gap over the 241 s TTL).
        let later_ms = NOW_MS + 300_000;
        let later = signed_headers(&key, &new, later_ms / 1000, &fresh_nonce());
        assert_eq!(
            refresh_before_verify(&verifier, &current, lookup(&later), REQUEST_ID, later_ms),
            Ok(())
        );
        // Replaying the first header set at the rolled-back clock refuses:
        // the regressed reading is 300 s below the mark (past the 5 s
        // tolerance), so the guard fails closed instead of re-arming.
        assert_eq!(
            refresh_before_verify(&verifier, &current, lookup(&first), REQUEST_ID, NOW_MS),
            Err(RefreshRejection::Stale)
        );
        assert_eq!(
            *current.read().expect("snapshot lock must be readable"),
            WebhookSecret::new(new).expect("rotated secret must parse")
        );
    }

    #[test]
    fn freshness_within_tolerance_decides_at_guarded_time() {
        // Pins `within_skew(.., guarded_ms / 1000)` against a raw-clock
        // regression. The rollback test above only exercises the guard's Err
        // branch (300 s below the mark); this test stays 5 s below the mark
        // (within tolerance) where the guarded and raw clocks disagree by
        // exactly the tolerance.
        use crate::clock_guard::CLOCK_SKEW_TOLERANCE_MS;
        let (verifier, key) = verifier();
        let old = fresh_material();
        let capture_secret = fresh_material();
        let sweep_secret = fresh_material();
        let current = lock_for(&old);
        // Future-edge capture: signed 120 s ahead, accepted at B.
        let capture = signed_headers(
            &key,
            &capture_secret,
            now_secs() + SKEW_SECONDS,
            &fresh_nonce(),
        );
        assert_eq!(
            refresh_before_verify(&verifier, &current, lookup(&capture), REQUEST_ID, NOW_MS),
            Ok(())
        );
        // Sweep at B + 242 s advances the mark and drops the capture nonce
        // (242 s over the 241 s TTL).
        let sweep_ms = NOW_MS + (NONCE_TTL_SECONDS + 1) * 1000;
        let sweep = signed_headers(&key, &sweep_secret, sweep_ms / 1000, &fresh_nonce());
        assert_eq!(
            refresh_before_verify(&verifier, &current, lookup(&sweep), REQUEST_ID, sweep_ms),
            Ok(())
        );
        // Replay 5 s below the mark: fresh at the raw clock (117 s) but stale
        // at the guarded clock (122 s), so it must refuse without re-arming.
        let replay_ms = sweep_ms - CLOCK_SKEW_TOLERANCE_MS;
        assert_eq!(
            refresh_before_verify(&verifier, &current, lookup(&capture), REQUEST_ID, replay_ms),
            Err(RefreshRejection::Stale)
        );
        assert_eq!(
            *current.read().expect("snapshot lock must be readable"),
            WebhookSecret::new(sweep_secret).expect("sweep secret must parse")
        );
    }

    #[test]
    fn nonce_burn_within_tolerance_decides_at_guarded_time() {
        // Pins `cache.offer(nonce, guarded_ms)` against a raw-clock
        // regression. A capture burned while the raw sample sits 5 s below
        // the mark must be stored at the mark: stored 5 s early it would
        // expire at +242 s raw while still guarded-fresh, reopening the F8
        // replay. The capture timestamp also requires guarded freshness to
        // accept (125 s raw-stale, 120 s guarded-fresh).
        use crate::clock_guard::CLOCK_SKEW_TOLERANCE_MS;
        let tolerance_secs = CLOCK_SKEW_TOLERANCE_MS / 1000;
        let (verifier, key) = verifier();
        let old = fresh_material();
        let primer_secret = fresh_material();
        let capture_secret = fresh_material();
        let current = lock_for(&old);
        // Primer advances the mark to B + 5 s.
        let primer_ms = NOW_MS + CLOCK_SKEW_TOLERANCE_MS;
        let primer = signed_headers(&key, &primer_secret, primer_ms / 1000, &fresh_nonce());
        assert_eq!(
            refresh_before_verify(&verifier, &current, lookup(&primer), REQUEST_ID, primer_ms),
            Ok(())
        );
        // Capture accepted at raw B with the mark at B + 5 s.
        let capture = signed_headers(
            &key,
            &capture_secret,
            now_secs() + SKEW_SECONDS + tolerance_secs,
            &fresh_nonce(),
        );
        assert_eq!(
            refresh_before_verify(&verifier, &current, lookup(&capture), REQUEST_ID, NOW_MS),
            Ok(())
        );
        // Replay at B + 242 s is guarded-fresh (117 s) with the nonce still
        // live (237 s at the guarded burn): it must be a Replay, not an
        // accept. A raw-clock burn would have expired (242 s) and re-armed.
        let replay_ms = NOW_MS + (NONCE_TTL_SECONDS + 1) * 1000;
        assert_eq!(
            refresh_before_verify(&verifier, &current, lookup(&capture), REQUEST_ID, replay_ms),
            Err(RefreshRejection::Replay)
        );
        assert_eq!(
            *current.read().expect("snapshot lock must be readable"),
            WebhookSecret::new(capture_secret).expect("capture secret must parse")
        );
    }

    #[test]
    fn invalid_candidate_is_rejected_without_swapping() {
        let (verifier, key) = verifier();
        let old = fresh_material();
        // Validly signed but too short: authentication passes, validation refuses.
        let short = "x".repeat(9);
        let headers = signed_headers(&key, &short, now_secs(), &fresh_nonce());
        let current = lock_for(&old);
        assert_eq!(
            refresh_before_verify(&verifier, &current, lookup(&headers), REQUEST_ID, NOW_MS),
            Err(RefreshRejection::InvalidSecret)
        );
        assert_eq!(
            *current.read().expect("snapshot lock must be readable"),
            WebhookSecret::new(old).expect("fixture secret must parse")
        );
    }

    #[test]
    fn debug_and_rejection_carry_no_secret_material() {
        let (verifier, key) = verifier();
        let secret = fresh_material();
        let key_text = String::from_utf8_lossy(&key);
        for rendered in [format!("{verifier:?}"), format!("{verifier:#?}")] {
            assert!(!rendered.contains(key_text.as_ref()));
            assert!(rendered.contains("key_len"));
        }
        for rejection in [
            RefreshRejection::MissingHeader,
            RefreshRejection::MalformedHeader,
            RefreshRejection::BadSignature,
            RefreshRejection::Stale,
            RefreshRejection::Replay,
            RefreshRejection::InvalidSecret,
            RefreshRejection::Unavailable,
        ] {
            let text = format!("{rejection:?}");
            assert!(!text.contains(key_text.as_ref()), "{rejection:?}");
            assert!(!text.contains(secret.as_str()), "{rejection:?}");
        }
        // The digest helper is stable and hex-shaped; digests never format
        // alongside secrets anywhere in this module.
        assert_eq!(secret_sha_hex(secret.as_bytes()).len(), 64);
    }
}
