//! Website-to-bot internal actions: pure-domain half of `POST /internal/actions`.
//!
//! Ports `src/internal/*` from legacy two-bot (frozen `main`, card acceptance
//! criteria) as framework-free data plus pure functions. The axum route, the
//! sqlx `InternalActionStore` tables (`internal_nonces`, `internal_idempotency`,
//! `internal_action_log`, `internal_discord_events`), and the twilight Discord
//! calls land in later slices behind the `db` feature and the bot crate; this
//! module owns the order of the checks and every refusal the caller can see, so
//! the whole pipeline below is unit-testable without Discord, Postgres, or a
//! socket.
//!
//! Legacy map (`src/internal/*.ts`):
//! - `signing.ts` — HMAC-SHA256 `sha256=` over
//!   `POST\n/internal/actions\n{ts}\n{nonce}\n{sha256_hex(body)}`, the
//!   rotation-aware `KeyRing` with its constant-time unknown-key decoy, and
//!   `parseKeys` for `TWO_INTERNAL_KEYS`.
//! - `nonce.ts` — in-process replay guard (240 s TTL, twice the ±120 s skew)
//!   plus the skew check itself.
//! - `rateLimit.ts` — per-key token buckets applied AFTER verify: default 20
//!   burst / 1 per s, `guild.add_member` 10 burst / 0.5 per s, `Retry-After`
//!   never 0.
//! - `errors.ts` — the typed `{code, status, retryable}` envelope and the one
//!   message every auth failure returns.
//! - `actions.ts` — the 19-verb allowlist (the card's 18 plus `event.read`,
//!   which landed later under TOG-5510), the idempotency-key set, the
//!   settings-store set, the role/channel key maps, and the per-field
//!   validators (snowflakes, reason, event input, moderation bounds, settings
//!   guard).
//! - `config.ts` — env-only flags (never settings): which verbs are live.
//! - `bind.ts` — private-interface guard: no wildcard or public bind, and
//!   deliberately no override flag. DNS resolution stays with the route; this
//!   module validates literal addresses.
//! - `server.ts` — the load-bearing check order, ported as [`authorize`]:
//!   headers → signature → skew → replay → key bucket → body → allowlist →
//!   action bucket. The idempotency claim and the Discord run stay with the
//!   store/bot slices.
//!
//! Security invariants carried over verbatim: buckets are keyed only after the
//! signature verifies (else anyone could lock out a real caller by spamming its
//! key id); the nonce burns before the body is read; unknown key id and wrong
//! signature are one indistinguishable refusal; nothing from the request body
//! (one body carries a live member OAuth token) ever reaches a log or audit
//! field — [`AuthDecision`] carries the parsed body to the caller and the
//! `Seen`-style log scalars stay with the route.

use std::collections::{HashMap, HashSet};

use hmac::{Hmac, KeyInit, Mac as _};
use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;

use super::moderation::{require_moderation_reason, ModerationAction, ReasonError};

/// Route path the canonical string signs (legacy `ACTIONS_PATH`).
pub const ACTIONS_PATH: &str = "/internal/actions";
/// Timestamp freshness window each way, seconds (legacy default `skewSeconds`).
pub const SKEW_SECONDS: u64 = 120;
/// Nonce memory: twice the skew window (legacy `NONCE_TTL_SECONDS`).
pub const NONCE_TTL_SECONDS: u64 = 240;
/// Crash-recovery valve for `in_flight` idempotency claims (legacy
/// `CLAIM_STALE_SECONDS`). The claim state machine itself lives with the db
/// slice; the constant lives here so both sides agree.
pub const CLAIM_STALE_SECONDS: u64 = 60;
/// Anything larger is a caller bug, not a request (legacy `MAX_BODY_BYTES`).
/// A full MEE6 export may hold hundreds of 2,000-character templates.
pub const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
/// Discord's own message ceiling; rejecting here beats a bare 400 from Discord.
pub const MAX_MESSAGE_CHARS: usize = 2000;
/// Discord's own event-name ceiling.
pub const MAX_EVENT_NAME_CHARS: usize = 100;
/// Discord's own event-description ceiling.
pub const MAX_EVENT_DESCRIPTION_CHARS: usize = 1000;
/// Ceiling on one stored setting: a dashboard field needing more is not a
/// setting, and the cap keeps one signed request from filling the table.
pub const MAX_SETTING_VALUE_BYTES: usize = 8192;
/// Settings keys look like environment variable names (legacy
/// `SETTINGS_KEY_PATTERN`): shape only, NOT the security guard — the guard is
/// [`is_storable_key`].
pub const MAX_SETTING_KEY_LEN: usize = 128;
/// Minimum `TWO_INTERNAL_KEYS` secret length, characters (legacy `parseKeys`).
pub const MIN_KEY_SECRET_LEN: usize = 32;
/// The one message every auth failure returns: a bad signature and an unknown
/// key id must be indistinguishable (legacy `AUTH_FAILURE_MESSAGE`).
pub const AUTH_FAILURE_MESSAGE: &str = "Signature verification failed";
/// ULID-ish request ids are 26 Crockford characters (legacy `newRequestId`).
pub const REQUEST_ID_LEN: usize = 26;

// ---------------------------------------------------------------------------
// Signing (signing.ts)
// ---------------------------------------------------------------------------

/// `sha256` of the raw body bytes, hex. The canonical string signs this hash
/// rather than the body, so it stays short and there is no argument about
/// encoding or key order. Verification runs over the raw bytes received — the
/// body is never re-serialised between here and the signature check.
#[must_use]
pub fn body_hash(raw: &[u8]) -> String {
    hex::encode(Sha256::digest(raw))
}

/// `POST\n/internal/actions\n{timestamp}\n{nonce}\n{sha256_hex(body)}`.
#[must_use]
pub fn canonical_string(timestamp: &str, nonce: &str, raw: &[u8]) -> String {
    format!(
        "POST\n{ACTIONS_PATH}\n{timestamp}\n{nonce}\n{}",
        body_hash(raw)
    )
}

/// `sha256=` + hex(HMAC-SHA256(secret, canonical)).
#[must_use]
pub fn sign(secret: &[u8], timestamp: &str, nonce: &str, raw: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts keys of any length");
    mac.update(canonical_string(timestamp, nonce, raw).as_bytes());
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// Constant-time compare of two `sha256=<hex>` strings.
///
/// `timingSafeEqual` throws on a length mismatch, which would itself be a
/// timing signal, so unequal lengths burn a same-length dummy compare first.
#[must_use]
pub fn signatures_match(a: &str, b: &str) -> bool {
    let (ab, bb) = (a.as_bytes(), b.as_bytes());
    if ab.len() != bb.len() {
        let _ = ab.ct_eq(ab);
        return false;
    }
    bool::from(ab.ct_eq(bb))
}

/// One caller: the value of `X-TWO-Key-Id`, with its secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SigningKey {
    pub id: String,
    pub secret: Vec<u8>,
}

/// `TWO_INTERNAL_KEYS` entries are `id:secret,id:secret`. Secrets may contain
/// anything except a comma, so entries split on the first colon only. An entry
/// without a colon is a config mistake and is rejected loudly — silently
/// dropping a key id would present as intermittent 401s later.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeySpecError {
    #[error("TWO_INTERNAL_KEYS entries must be \"key-id:secret\"")]
    MalformedEntry,
    #[error("TWO_INTERNAL_KEYS: secret for \"{0}\" is shorter than 32 characters")]
    SecretTooShort(String),
    #[error("TWO_INTERNAL_ACTIONS=1 but no signing keys are configured")]
    NoKeys,
}

/// Parse a `TWO_INTERNAL_KEYS` spec. Error messages name the key id, never the
/// secret.
pub fn parse_keys(spec: &str) -> Result<Vec<SigningKey>, KeySpecError> {
    let mut out = Vec::new();
    for entry in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (id, secret) = match entry.split_once(':') {
            Some((id, secret)) => (id.trim(), secret.trim()),
            None => return Err(KeySpecError::MalformedEntry),
        };
        if id.is_empty() || secret.is_empty() {
            return Err(KeySpecError::MalformedEntry);
        }
        if secret.len() < MIN_KEY_SECRET_LEN {
            return Err(KeySpecError::SecretTooShort(id.to_owned()));
        }
        out.push(SigningKey {
            id: id.to_owned(),
            secret: secret.as_bytes().to_vec(),
        });
    }
    if out.is_empty() {
        return Err(KeySpecError::NoKeys);
    }
    Ok(out)
}

/// Rotation-aware verifier. True only for a known key id whose signature
/// verifies; the caller gets one boolean and no way to tell an unknown id
/// from a wrong signature apart — unknown ids verify against a decoy secret so
/// both failures cost the same work.
#[derive(Debug, Clone)]
pub struct KeyRing {
    keys: HashMap<String, Vec<u8>>,
}

/// A secret used when the key id is unknown (legacy `DECOY_SECRET`).
pub const DECOY_SECRET: &[u8] = b"unknown-key-id-decoy";

impl KeyRing {
    #[must_use]
    pub fn new(keys: Vec<SigningKey>) -> Self {
        Self {
            keys: keys.into_iter().map(|k| (k.id, k.secret)).collect(),
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    #[must_use]
    pub fn contains(&self, id: &str) -> bool {
        self.keys.contains_key(id)
    }

    #[must_use]
    pub fn verify(
        &self,
        key_id: &str,
        signature: &str,
        timestamp: &str,
        nonce: &str,
        raw: &[u8],
    ) -> bool {
        let secret = self.keys.get(key_id);
        let expected = sign(
            secret.map_or(DECOY_SECRET, Vec::as_slice),
            timestamp,
            nonce,
            raw,
        );
        secret.is_some() && signatures_match(&expected, signature)
    }
}

// ---------------------------------------------------------------------------
// Freshness and replay (nonce.ts, server.ts header checks)
// ---------------------------------------------------------------------------

/// Is this timestamp inside the skew window? `timestamp` is unix *seconds* as
/// sent; a non-numeric value is not fresh.
#[must_use]
pub fn within_skew(timestamp: &str, skew_seconds: u64, now_unix_secs: u64) -> bool {
    if timestamp.is_empty() || timestamp.len() > 15 {
        return false;
    }
    if !timestamp.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let sent: u64 = match timestamp.parse() {
        Ok(t) => t,
        Err(_) => return false,
    };
    now_unix_secs.abs_diff(sent) <= skew_seconds
}

/// Header nonces are 32 lowercase-or-uppercase hex characters (legacy
/// `/^[0-9a-f]{32}$/i`).
#[must_use]
pub fn valid_nonce_format(nonce: &str) -> bool {
    nonce.len() == 32 && nonce.bytes().all(|b| b.is_ascii_hexdigit())
}

/// In-process replay guard: a nonce is remembered for the TTL so a request
/// still fresh enough to accept is still recent enough to recognise as a
/// repeat. Times are explicit milliseconds (the injectable clock, made a
/// parameter so tests age entries without sleeping). The durable table version
/// lands with the db slice; this guard covers auth-only operation and unit
/// tests exactly the way the legacy `NonceCache` did.
#[derive(Debug, Clone)]
pub struct NonceCache {
    seen: HashMap<String, u64>,
    ttl_ms: u64,
}

impl NonceCache {
    #[must_use]
    pub fn new(ttl_seconds: u64) -> Self {
        Self {
            seen: HashMap::new(),
            ttl_ms: ttl_seconds * 1000,
        }
    }

    /// Record a nonce. Returns false if it was already present and still live,
    /// which is a replay. Check-and-insert is one call on purpose: two callers
    /// doing "check then insert" would race.
    pub fn offer(&mut self, nonce: &str, now_ms: u64) -> bool {
        self.sweep(now_ms);
        if let Some(&prev) = self.seen.get(nonce) {
            // Saturating: a backwards clock yields 0, which is inside the TTL
            // and therefore a denial — matching legacy `t - prev < ttlMs`,
            // where a negative difference is also a replay refusal.
            if now_ms.saturating_sub(prev) < self.ttl_ms {
                return false;
            }
        }
        self.seen.insert(nonce.to_owned(), now_ms);
        true
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }

    /// Drop expired entries. Called on every offer, so the map stays bounded
    /// by the request rate over one TTL rather than growing forever.
    pub fn sweep(&mut self, now_ms: u64) {
        self.seen
            .retain(|_, &mut at| now_ms.saturating_sub(at) < self.ttl_ms);
    }
}

// ---------------------------------------------------------------------------
// Rate limits (rateLimit.ts)
// ---------------------------------------------------------------------------

/// Token-bucket shape: `capacity` is the burst, `refill_per_second` the
/// sustained rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BucketSpec {
    pub capacity: f64,
    pub refill_per_second: f64,
}

/// 60 requests/minute sustained with a burst of 20 (legacy `DEFAULT_BUCKET`).
pub const DEFAULT_BUCKET: BucketSpec = BucketSpec {
    capacity: 20.0,
    refill_per_second: 1.0,
};

/// 30/minute with a burst of 10, so a handful of simultaneous signups pass
/// (legacy `ADD_MEMBER_BUCKET`).
pub const ADD_MEMBER_BUCKET: BucketSpec = BucketSpec {
    capacity: 10.0,
    refill_per_second: 0.5,
};

/// Bucket verdict. `retry_after_secs` is only meaningful when denied — and is
/// never 0, because a `Retry-After: 0` invites a hot loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketDecision {
    pub allowed: bool,
    pub retry_after_secs: u64,
}

/// Per-key token buckets, keyed AFTER the signature verifies, never before.
/// Times are explicit milliseconds (legacy injectable `now`, made a
/// parameter).
#[derive(Debug, Clone, Default)]
pub struct TokenBuckets {
    buckets: HashMap<String, BucketState>,
}

#[derive(Debug, Clone, Copy)]
struct BucketState {
    tokens: f64,
    updated_ms: u64,
}

impl TokenBuckets {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Take one token from `key`'s bucket, creating it full on first use.
    pub fn take(&mut self, key: &str, spec: BucketSpec, now_ms: u64) -> BucketDecision {
        let bucket = self.buckets.entry(key.to_owned()).or_insert(BucketState {
            tokens: spec.capacity,
            updated_ms: now_ms,
        });
        let elapsed = now_ms.saturating_sub(bucket.updated_ms) as f64 / 1000.0;
        bucket.tokens = spec
            .capacity
            .min(bucket.tokens + elapsed * spec.refill_per_second);
        bucket.updated_ms = now_ms;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            return BucketDecision {
                allowed: true,
                retry_after_secs: 0,
            };
        }
        let wait = ((1.0 - bucket.tokens) / spec.refill_per_second).ceil() as u64;
        BucketDecision {
            allowed: false,
            retry_after_secs: wait.max(1),
        }
    }
}

// ---------------------------------------------------------------------------
// Error envelope (errors.ts)
// ---------------------------------------------------------------------------

/// The website branches on `code` and on `retryable`, never on the English in
/// the message — so the mapping from code to HTTP status and to retryable
/// lives here, once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    Malformed,
    Unauthorized,
    StaleRequest,
    ActionNotAllowed,
    Replayed,
    /// The one 409 that IS retryable: an earlier attempt at this same operation
    /// has not finished yet. Retrying with the same key is exactly right.
    InProgress,
    DiscordRejected,
    RateLimited,
    Internal,
    DiscordUnavailable,
    UpstreamTimeout,
}

impl ErrorCode {
    /// Wire name (legacy string codes).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::Unauthorized => "unauthorized",
            Self::StaleRequest => "stale_request",
            Self::ActionNotAllowed => "action_not_allowed",
            Self::Replayed => "replayed",
            Self::InProgress => "in_progress",
            Self::DiscordRejected => "discord_rejected",
            Self::RateLimited => "rate_limited",
            Self::Internal => "internal",
            Self::DiscordUnavailable => "discord_unavailable",
            Self::UpstreamTimeout => "upstream_timeout",
        }
    }

    /// HTTP status for the code (legacy `statusFor`).
    #[must_use]
    pub fn status(self) -> u16 {
        match self {
            Self::Malformed => 400,
            Self::Unauthorized | Self::StaleRequest => 401,
            Self::ActionNotAllowed => 403,
            Self::Replayed | Self::InProgress => 409,
            Self::DiscordRejected => 422,
            Self::RateLimited => 429,
            Self::Internal => 500,
            Self::DiscordUnavailable => 502,
            Self::UpstreamTimeout => 504,
        }
    }

    /// Whether the website should retry (legacy `retryableFor`).
    #[must_use]
    pub fn retryable(self) -> bool {
        match self {
            Self::InProgress
            | Self::RateLimited
            | Self::Internal
            | Self::DiscordUnavailable
            | Self::UpstreamTimeout => true,
            Self::Malformed
            | Self::Unauthorized
            | Self::StaleRequest
            | Self::ActionNotAllowed
            | Self::Replayed
            | Self::DiscordRejected => false,
        }
    }
}

/// Thrown anywhere in the request pipeline; caught once at the top and turned
/// into the envelope. `log_reason` is the detail kept for the structured log —
/// never the response body. `retry_after_secs` is set for `rate_limited` only.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ActionError {
    pub code: ErrorCode,
    pub message: String,
    pub log_reason: String,
    pub retry_after_secs: Option<u64>,
}

impl ActionError {
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>, log_reason: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            log_reason: log_reason.into(),
            retry_after_secs: None,
        }
    }

    #[must_use]
    pub fn with_retry_after(mut self, secs: u64) -> Self {
        self.retry_after_secs = Some(secs);
        self
    }

    /// HTTP status for the response.
    #[must_use]
    pub fn status(&self) -> u16 {
        self.code.status()
    }
}

/// Every auth failure is this one error: same code, same message, detail only
/// in `log_reason`.
#[must_use]
pub fn auth_failure(log_reason: impl Into<String>) -> ActionError {
    ActionError::new(ErrorCode::Unauthorized, AUTH_FAILURE_MESSAGE, log_reason)
}

// ---------------------------------------------------------------------------
// Allowlist (actions.ts: IMPLEMENTED_ACTIONS, NEEDS_* sets, assertAllowed)
// ---------------------------------------------------------------------------

/// Every verb the endpoint will run, in legacy publish order: the card's 18
/// plus `event.read`, the narrow mapped-event verifier that landed later
/// (TOG-5510). Adding a verb here without deciding which side of
/// [`NEEDS_IDEMPOTENCY_KEY`] it falls on is the mistake that set exists to
/// prevent.
pub const IMPLEMENTED_ACTIONS: [&str; 19] = [
    "role.assign",
    "guild.add_member",
    "announcement.post",
    "event.upsert",
    "event.cancel",
    "event.read",
    "automations.import",
    "automations.export",
    "settings.get",
    "settings.set",
    "moderation.ban",
    "moderation.tempban",
    "moderation.kick",
    "moderation.timeout",
    "moderation.warn",
    "moderation.purge",
    "moderation.slowmode",
    "moderation.lockdown",
    "moderation.unlock",
];

/// Actions where a repeat is not harmless, so the caller must send an
/// `Idempotency-Key` and the bot must remember the result. A settings write
/// sits here because two deliveries of the same save are two audit rows and
/// two version bumps — and a concurrent save landing in between would be
/// silently reverted by the second delivery.
pub const NEEDS_IDEMPOTENCY_KEY: [&str; 14] = [
    "announcement.post",
    "event.upsert",
    "event.cancel",
    "automations.import",
    "settings.set",
    "moderation.ban",
    "moderation.tempban",
    "moderation.kick",
    "moderation.timeout",
    "moderation.warn",
    "moderation.purge",
    "moderation.slowmode",
    "moderation.lockdown",
    "moderation.unlock",
];

/// Actions that cannot run without the config store wired in. Configured-on
/// with nothing behind it is a typed refusal, never a 500.
pub const NEEDS_SETTINGS_STORE: [&str; 2] = ["settings.get", "settings.set"];

/// The nine moderation verbs, in legacy publish order (mirrors
/// `MODERATION_ACTIONS`; [`ModerationAction::action_name`] is the same list).
pub const MODERATION_ACTIONS: [&str; 9] = [
    "moderation.ban",
    "moderation.tempban",
    "moderation.kick",
    "moderation.timeout",
    "moderation.warn",
    "moderation.purge",
    "moderation.slowmode",
    "moderation.lockdown",
    "moderation.unlock",
];

#[must_use]
pub fn is_implemented(action: &str) -> bool {
    IMPLEMENTED_ACTIONS.contains(&action)
}

#[must_use]
pub fn needs_idempotency_key(action: &str) -> bool {
    NEEDS_IDEMPOTENCY_KEY.contains(&action)
}

#[must_use]
pub fn needs_settings_store(action: &str) -> bool {
    NEEDS_SETTINGS_STORE.contains(&action)
}

/// Check the action is one the endpoint will run, before anything looks at the
/// rest of the body. Unknown, disabled, store-less, and settings-less are all
/// `action_not_allowed`, which is never retryable.
pub fn assert_allowed(
    action: &str,
    flags: &InternalFlags,
    has_store: bool,
    has_settings: bool,
) -> Result<(), ActionError> {
    if !is_implemented(action) {
        return Err(ActionError::new(
            ErrorCode::ActionNotAllowed,
            format!(r#""{action}" is not an allowlisted action"#),
            "action_unknown",
        ));
    }
    if !flags.enabled.contains(action) {
        return Err(ActionError::new(
            ErrorCode::ActionNotAllowed,
            format!(r#""{action}" is not enabled on this bot"#),
            "action_disabled",
        ));
    }
    // Configured on but with no database behind it. Only reachable if the
    // endpoint starts without a store, which boot does not do; a typed refusal
    // beats a 500 if that ever changes.
    if needs_idempotency_key(action) && !has_store {
        return Err(ActionError::new(
            ErrorCode::ActionNotAllowed,
            format!(r#""{action}" needs the durable store"#),
            "action_needs_store",
        ));
    }
    if needs_settings_store(action) && !has_settings {
        return Err(ActionError::new(
            ErrorCode::ActionNotAllowed,
            format!(r#""{action}" needs the config store"#),
            "action_needs_settings",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Env-only flags (config.ts: loadInternalActionsConfig)
// ---------------------------------------------------------------------------

/// Which verbs are live, read straight from the environment — never from the
/// settings store. These switches decide what the *website* may make the bot
/// do, so a website that could change them could grant itself the rest of the
/// allowlist. `settings.set` refuses the whole `TWO_INTERNAL_*` namespace (see
/// [`is_storable_key`]), and `TWO_MODERATION` co-gates the nine moderation
/// verbs while carrying no `TWO_INTERNAL_` prefix, so the prefix refusal never
/// covered it — the co-gate below is the backstop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InternalFlags {
    /// Live verbs. The three Phase 1 actions (`role.assign`,
    /// `announcement.post`, `event.upsert`) are approved and unconditional;
    /// everything else joins on its own approval flag.
    pub enabled: HashSet<String>,
    /// Destructive automations imports need this stronger, separately
    /// configured capability on top of the base automations flag.
    pub allow_automation_overwrite: bool,
}

impl InternalFlags {
    /// Read flags from the process environment.
    pub fn from_env() -> Self {
        Self::from_map(&std::env::vars().collect())
    }

    /// Read flags from an explicit map (tests, staged config).
    #[must_use]
    pub fn from_map(vars: &HashMap<String, String>) -> Self {
        let flag = |key: &str| vars.get(key).is_some_and(|v| v == "1");
        let mut enabled: HashSet<String> = ["role.assign", "announcement.post", "event.upsert"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        // `guild.add_member` stays dark until the CEO signs off on the
        // allowlist entry (TOG-44); the flag is the record of that decision.
        if flag("TWO_INTERNAL_ALLOW_ADD_MEMBER") {
            enabled.insert("guild.add_member".to_owned());
        }
        if flag("TWO_INTERNAL_ALLOW_EVENT_CANCEL") {
            enabled.insert("event.cancel".to_owned());
        }
        if flag("TWO_INTERNAL_ALLOW_EVENT_READ") {
            enabled.insert("event.read".to_owned());
        }
        // Merely shipping the implementation must not widen the allowlist.
        if flag("TWO_INTERNAL_ALLOW_AUTOMATIONS") {
            enabled.insert("automations.import".to_owned());
            enabled.insert("automations.export".to_owned());
        }
        if flag("TWO_INTERNAL_ALLOW_SETTINGS") {
            enabled.insert("settings.get".to_owned());
            enabled.insert("settings.set".to_owned());
        }
        if flag("TWO_INTERNAL_ALLOW_MODERATION") && flag("TWO_MODERATION") {
            enabled.extend(MODERATION_ACTIONS.iter().map(|s| (*s).to_owned()));
        }
        Self {
            enabled,
            allow_automation_overwrite: flag("TWO_INTERNAL_ALLOW_AUTOMATIONS")
                && flag("TWO_INTERNAL_ALLOW_AUTOMATIONS_OVERWRITE"),
        }
    }

    #[must_use]
    pub fn is_enabled(&self, action: &str) -> bool {
        self.enabled.contains(action)
    }
}

// ---------------------------------------------------------------------------
// Role / channel key maps (actions.ts: buildRoleKeys, buildChannelKeys)
// ---------------------------------------------------------------------------

/// Bad `TWO_INTERNAL_ROLE_KEYS` / `TWO_INTERNAL_CHANNEL_KEYS` spec. Boot reads
/// these maps, so a bad entry is a crash at startup, not a quietly-exposed
/// remote control.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyMapError {
    #[error("{env_name} entries must be \"{what}-key:<{what} snowflake>\"")]
    MalformedEntry { env_name: String, what: String },
}

/// Parse `key:snowflake,key:snowflake` pairs. The website never names a
/// Discord snowflake: roles resolve through this map, channels through the
/// channel map (which starts EMPTY — with no `TWO_INTERNAL_CHANNEL_KEYS` there
/// is no channel `announcement.post` may address, so an unconfigured bot
/// refuses every post by key lookup, which is the correct answer, not a gap).
pub fn build_key_map(
    spec: &str,
    env_name: &str,
    what: &str,
) -> Result<HashMap<String, String>, KeyMapError> {
    let mut map = HashMap::new();
    for entry in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let mut parts = entry.split(':').map(str::trim);
        let key = parts.next().unwrap_or_default();
        let id = parts.next().unwrap_or_default();
        if key.is_empty() || !is_snowflake(id) {
            return Err(KeyMapError::MalformedEntry {
                env_name: env_name.to_owned(),
                what: what.to_owned(),
            });
        }
        map.insert(key.to_owned(), id.to_owned());
    }
    Ok(map)
}

/// Build the role-key map from `TWO_INTERNAL_ROLE_KEYS`. Legacy additionally
/// seeds every role a member can already self-assign in Discord (the
/// conservative default: it hands the website no privilege a member lacks by
/// clicking a menu); that seed arrives with the onboarding-catalog slice, so
/// until then only explicit pairs exist.
pub fn build_role_keys(extra_spec: &str) -> Result<HashMap<String, String>, KeyMapError> {
    build_key_map(extra_spec, "TWO_INTERNAL_ROLE_KEYS", "role")
}

/// Build the channel-key map from `TWO_INTERNAL_CHANNEL_KEYS`. Unlike roles
/// there is no safe starting set to inherit, so this starts empty.
pub fn build_channel_keys(spec: &str) -> Result<HashMap<String, String>, KeyMapError> {
    build_key_map(spec, "TWO_INTERNAL_CHANNEL_KEYS", "channel")
}

// ---------------------------------------------------------------------------
// Settings-key guard (actions.ts: requireSettingsKey, settingsCatalog.ts)
// ---------------------------------------------------------------------------

/// Catalog class for one settings key (legacy `SettingClass`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingClass {
    /// Dashboard-writable.
    Hot,
    /// Read once at boot.
    Cold,
    /// Secrets, boot inputs, network binds, capability gates: never reachable
    /// from the dashboard.
    EnvOnly,
}

/// Prefixes that are unconditionally environment-only. `TWO_INTERNAL_KEYS`
/// reaches code through a secret fallback rather than an env read, so the
/// census never sees it — but it is the signing secret, and the prefix arm
/// declares it env-only positively rather than by omission.
pub const ENV_ONLY_KEY_PREFIXES: [&str; 1] = ["TWO_INTERNAL_"];

/// The whole census: every name `src/` reads. `hot` is the residual class (a
/// key is hot when nothing reads it at boot); `cold` and `env_only` carry the
/// legacy citation in the module doc below. Counts are asserted in tests
/// (hot 41 / cold 28 / env_only 47) so drift is caught, not silent.
///
/// Legacy source: `src/core/settingsCatalog.ts` `SETTING_CLASSES`.
fn classify_key(key: &str) -> Option<SettingClass> {
    use SettingClass::{Cold, EnvOnly, Hot};
    match key {
        // ------------------------------------------------------- secrets
        "DISCORD_BOT_TOKEN"
        | "DISCORD_TOKEN"
        | "TWO_MODERATION_AUDIT_SECRET"
        | "TWO_DATABASE_URL"
        | "TWO_STAGING_DATABASE_URL"
        | "DISCORD_STAGING_BOT_TOKEN"
        | "TWO_BACKUP_S3_ACCESS_KEY_ID"
        | "TWO_BACKUP_S3_SECRET_ACCESS_KEY" => Some(EnvOnly),
        // A web-settable bucket or endpoint turns the backup job into an
        // exfiltration channel — worse than the credentials leaking alone.
        "TWO_BACKUP_S3_BUCKET"
        | "TWO_BACKUP_S3_ENDPOINT"
        | "TWO_BACKUP_S3_PREFIX"
        | "TWO_BACKUP_S3_REGION" => Some(EnvOnly),
        // ---------------------------------------------------------- boot
        "CREDENTIALS_DIRECTORY"
        | "TWO_DB_POOL_MAX"
        | "DISCORD_GUILD_ID"
        | "DISCORD_STAGING_GUILD_ID"
        | "TWO_STAGING_RESTART_CONTAINMENT"
        | "TWO_STAGING_RESTART_SYNTHETIC_ACTORS" => Some(EnvOnly),
        // ------------------------------------------------------- network
        // `DISCORD_API_BASE` repoints every token-bearing request; settable
        // from a web UI it redirects them all.
        "TWO_HEALTH_BIND_HOST"
        | "TWO_HEALTH_PORT"
        | "TWO_REDIRECT_BIND_HOST"
        | "TWO_REDIRECT_PORT"
        | "DISCORD_API_BASE" => Some(EnvOnly),
        // ----------------------------- capability gates and their bounds
        "TWO_INTERNAL_ACTIONS"
        | "TWO_INTERNAL_ALLOW_ADD_MEMBER"
        | "TWO_INTERNAL_ALLOW_AUTOMATIONS"
        | "TWO_INTERNAL_ALLOW_AUTOMATIONS_OVERWRITE"
        | "TWO_INTERNAL_ALLOW_EVENT_CANCEL"
        | "TWO_INTERNAL_ALLOW_EVENT_READ"
        | "TWO_INTERNAL_ALLOW_MODERATION"
        | "TWO_INTERNAL_ALLOW_SETTINGS"
        | "TWO_INTERNAL_BIND_HOST"
        | "TWO_INTERNAL_CHANNEL_KEYS"
        | "TWO_INTERNAL_PORT"
        | "TWO_INTERNAL_ROLE_KEYS" => Some(EnvOnly),
        // Outside the namespace, inside the blast radius: TWO_MODERATION
        // co-gates nine moderation verbs; TWO_ONBOARDING_MODE switches the
        // join funnel.
        "TWO_MODERATION" | "TWO_ONBOARDING_MODE" => Some(EnvOnly),
        // Staging-only collection and notice capabilities, never
        // dashboard-settable.
        "TWO_ONBOARDING_ROTA_MEASUREMENT"
        | "TWO_ONBOARDING_ROTA_NOTICE"
        | "TWO_ONBOARDING_ROTA_PSEUDONYM_KEY"
        | "TWO_ONBOARDING_ROTA_PRIMARY_ACTOR_ID"
        | "TWO_ONBOARDING_ROTA_READER_IDS" => Some(EnvOnly),
        // These decide who an already-enabled verb may reach; widening them
        // from the web is the same escalation one step later.
        "TWO_MODERATION_PROTECTED_ROLE_IDS"
        | "TWO_OWEN_USER_ID"
        | "TWO_ANTI_NUKE_PROTECTED_USER_IDS"
        | "TWO_ANTI_NUKE_TRUSTED_USER_IDS" => Some(EnvOnly),
        // A filesystem path chosen by a web form is a write primitive.
        "TWO_ANTI_NUKE_SNAPSHOT_PATH" => Some(EnvOnly),
        // ---------------------------------------------------------- cold
        // Read once at boot: feature master switches gate construction or
        // command registration, and none can flip on a live client.
        "TWO_AUTOMOD"
        | "TWO_ANNOUNCEMENTS"
        | "TWO_AUTOMATIONS"
        | "TWO_TEXT_COMMANDS"
        | "TEMP_VOICE_ENABLED"
        | "TWO_TEMP_VOICE"
        | "TWO_TEMP_VOICE_GENERATOR_CHANNEL_ID"
        | "TWO_TEMP_VOICE_CATEGORY_ID"
        | "TWO_TEMP_VOICE_PROTECTED_CHANNEL_IDS"
        | "TWO_TEMP_VOICE_EMPTY_GRACE_SECONDS"
        | "TWO_TEMP_VOICE_SWEEP_SECONDS"
        | "TWO_TEMP_VOICE_MAX_PER_USER"
        | "TWO_TEMP_VOICE_MAX_PER_GUILD"
        | "TWO_TEMP_VOICE_CREATE_COOLDOWN_SECONDS"
        | "TWO_TEMP_VOICE_NAME_TEMPLATE"
        | "TWO_TEMP_VOICE_PANEL_CHANNEL_ID"
        | "TWO_TEMP_VOICE_DISABLED_CONTROLS"
        | "TWO_ANTI_NUKE"
        | "TWO_ANTI_NUKE_DRY_RUN"
        | "TWO_COMMUNITY_SCORECARD"
        | "TWO_COMMUNITY_RECOMMENDATIONS"
        | "TWO_COMMUNITY_CORRECTION_CYCLES"
        | "TWO_SELF_ROLE_PANELS"
        | "TWO_PRESENCE_PROBE"
        | "TWO_FEED_POLL_SECONDS"
        | "TWO_INACTIVITY_DAYS"
        | "TWO_TICKET_COOLDOWN_SECONDS"
        | "LOG_LEVEL" => Some(Cold),
        // ----------------------------------------------------------- hot
        // Channel, role and category ids: read per event, so a saved value
        // applies on the next poll.
        "DISCORD_ANCHOR_WELCOME_CHANNEL_ID"
        | "DISCORD_AUDIT_LOG_CHANNEL_ID"
        | "DISCORD_GOODBYE_CHANNEL_IDS"
        | "DISCORD_LANDING_CHANNEL_IDS"
        | "DISCORD_MODERATION_LOG_CHANNEL_ID"
        | "DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID"
        | "DISCORD_SESSION_LOOKING_TO_PLAY_CHANNEL_ID"
        | "DISCORD_STAFF_ALERT_CHANNEL_ID"
        | "DISCORD_TICKET_CATEGORY_ID"
        | "DISCORD_TICKET_PANEL_CHANNEL_ID"
        | "DISCORD_VOICE_LOG_CHANNEL_ID"
        | "DISCORD_TICKET_STAFF_ROLE_ID" => Some(Hot),
        // Raid and join-risk thresholds.
        "TWO_RAID_JOIN_THRESHOLD"
        | "TWO_RAID_WINDOW_SECONDS"
        | "TWO_JOIN_RISK_THRESHOLD"
        | "TWO_JOIN_RISK_WINDOW_SECONDS" => Some(Hot),
        // Anti-nuke tuning. The master switch and exempt lists are env_only
        // above; what is left is genuinely just tuning.
        "TWO_ANTI_NUKE_EVENT_MAX_AGE_SECONDS"
        | "TWO_ANTI_NUKE_HEAT_THRESHOLD"
        | "TWO_ANTI_NUKE_WINDOW_SECONDS"
        | "TWO_BULK_JOIN_WINDOW_UNTIL" => Some(Hot),
        // Automod tuning. The master switch is cold, the tuning is hot — that
        // split is the whole point of automod being dashboard-usable at all.
        "TWO_AUTOMOD_ALLOWED_DOMAINS"
        | "TWO_AUTOMOD_BAD_WORDS"
        | "TWO_AUTOMOD_BLOCKED_ATTACHMENT_EXTENSIONS"
        | "TWO_AUTOMOD_BYPASS_ROLE_IDS"
        | "TWO_AUTOMOD_ENFORCE"
        | "TWO_AUTOMOD_EXEMPT_CHANNEL_IDS"
        | "TWO_AUTOMOD_MENTION_LIMIT"
        | "TWO_AUTOMOD_REPEAT_COUNT"
        | "TWO_AUTOMOD_REPEAT_WINDOW_SECONDS"
        | "TWO_AUTOMOD_SANCTIONS" => Some(Hot),
        // Community classifier inputs: read per scorecard run.
        "TWO_COMMUNITY_AUTOMATION_ACTOR_IDS"
        | "TWO_COMMUNITY_CLASSIFIER_VERSION"
        | "TWO_COMMUNITY_HUMAN_CHANNEL_IDS"
        | "TWO_COMMUNITY_RAID_ACTOR_IDS"
        | "TWO_COMMUNITY_STAGING_ACTOR_IDS"
        | "TWO_COMMUNITY_STAGING_GUILD_IDS"
        | "TWO_COMMUNITY_TEST_ACTOR_IDS"
        | "TWO_COMMUNITY_WELCOME_CHANNEL_IDS" => Some(Hot),
        // Dry-run flags, read at the point of the write they suppress.
        "TWO_ONBOARDING_DRY_RUN" | "TWO_SELF_ROLE_DRY_RUN" | "TWO_REDIRECT_FALLBACK_CODE" => {
            Some(Hot)
        }
        _ => None,
    }
}

/// True for a key that must stay in the environment. Unknown keys are
/// env-only: fail-closed is the only safe census miss.
#[must_use]
pub fn is_env_only_key(key: &str) -> bool {
    if ENV_ONLY_KEY_PREFIXES.iter().any(|p| key.starts_with(p)) {
        return true;
    }
    !matches!(
        classify_key(key),
        Some(SettingClass::Hot | SettingClass::Cold)
    )
}

/// True when the catalog *declares* the key environment-only, as opposed to
/// refusing it for never having heard of it. The distinction is for the human
/// reading the refusal: "environment-only" and "no such setting" send an admin
/// to two different places.
#[must_use]
pub fn is_declared_env_only(key: &str) -> bool {
    if ENV_ONLY_KEY_PREFIXES.iter().any(|p| key.starts_with(p)) {
        return true;
    }
    matches!(classify_key(key), Some(SettingClass::EnvOnly))
}

/// True when `settings.set` may store this key. This is the copy that matters
/// most: it runs before an attacker-supplied key reaches any code that writes.
#[must_use]
pub fn is_storable_key(key: &str) -> bool {
    !is_env_only_key(key)
}

/// Shape check for a settings key: the shape of an environment variable, which
/// is what every reader was written against. A typo lands as a typed
/// `malformed` instead of a row nothing will ever read.
#[must_use]
pub fn valid_settings_key_shape(key: &str) -> bool {
    let bytes = key.as_bytes();
    if bytes.len() < 2 || bytes.len() > MAX_SETTING_KEY_LEN {
        return false;
    }
    if !bytes[0].is_ascii_uppercase() {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_')
}

/// The key guard for both settings actions. Since TOG-3100 the rule is catalog
/// membership, not a prefix: writable iff the catalog classes it `hot` or
/// `cold`, refused otherwise. Both refusals are absolute but not the same
/// answer — "environment-only" is a policy decision about a classified key,
/// while an unclassified key is almost always a typo.
pub fn require_settings_key<'a>(
    body: &'a Map<String, Value>,
    action: &str,
) -> Result<&'a str, ActionError> {
    let key = require_field_str(body, "key")?;
    if !valid_settings_key_shape(key) {
        return Err(ActionError::new(
            ErrorCode::Malformed,
            "\"key\" must look like an environment variable name",
            "settings_key_malformed",
        ));
    }
    if !is_storable_key(key) {
        if is_declared_env_only(key) {
            return Err(ActionError::new(
                ErrorCode::ActionNotAllowed,
                format!(r#""{key}" is environment-only and cannot be reached by "{action}""#),
                "settings_key_env_only",
            ));
        }
        return Err(ActionError::new(
            ErrorCode::ActionNotAllowed,
            format!(r#""{key}" is not a setting this bot reads, so "{action}" will not reach it"#),
            "settings_key_unknown",
        ));
    }
    Ok(key)
}

// ---------------------------------------------------------------------------
// Field validators (actions.ts require*, moderation bounds, event input)
// ---------------------------------------------------------------------------

/// A Discord snowflake: 17–20 ASCII digits.
#[must_use]
pub fn is_snowflake(value: &str) -> bool {
    let bytes = value.as_bytes();
    (17..=20).contains(&bytes.len()) && bytes.iter().all(|b| b.is_ascii_digit())
}

/// Non-empty string field, else `malformed`.
pub fn require_field_str<'a>(
    body: &'a Map<String, Value>,
    field: &str,
) -> Result<&'a str, ActionError> {
    match body.get(field).and_then(Value::as_str) {
        Some(v) if !v.is_empty() => Ok(v),
        _ => Err(ActionError::new(
            ErrorCode::Malformed,
            format!(r#""{field}" must be a non-empty string"#),
            format!("missing_{field}"),
        )),
    }
}

/// Snowflake field, else `malformed`.
pub fn require_snowflake<'a>(
    body: &'a Map<String, Value>,
    field: &str,
) -> Result<&'a str, ActionError> {
    let value = require_field_str(body, field)?;
    if !is_snowflake(value) {
        return Err(ActionError::new(
            ErrorCode::Malformed,
            format!(r#""{field}" must be a Discord id"#),
            format!("bad_{field}"),
        ));
    }
    Ok(value)
}

/// The mandatory audit reason: trimmed, non-empty, at most 512 characters
/// (legacy `requireModerationReason`, shared with the slash-command path via
/// [`require_moderation_reason`]).
pub fn require_reason(value: &Value) -> Result<String, ActionError> {
    match value.as_str() {
        Some(s) => require_moderation_reason(s).map_err(|e| match e {
            ReasonError::Empty => ActionError::new(
                ErrorCode::Malformed,
                "\"reason\" must be a non-empty string",
                "missing_reason",
            ),
            ReasonError::TooLong => ActionError::new(
                ErrorCode::Malformed,
                "\"reason\" is longer than 512 characters",
                "reason_too_long",
            ),
        }),
        None => Err(ActionError::new(
            ErrorCode::Malformed,
            "\"reason\" must be a non-empty string",
            "missing_reason",
        )),
    }
}

/// An ISO-8601 instant, normalised to RFC 3339 — what Discord wants.
/// Deliberate tightening vs legacy `Date.parse`: legacy accepted any string
/// the JS engine could date-parse; the Rust port requires RFC 3339, which is
/// what the website already sends (`toISOString()`).
pub fn require_timestamp(body: &Map<String, Value>, field: &str) -> Result<String, ActionError> {
    use time::format_description::well_known::Rfc3339;
    let value = require_field_str(body, field)?;
    let parsed = time::OffsetDateTime::parse(value, &Rfc3339).map_err(|_| {
        ActionError::new(
            ErrorCode::Malformed,
            format!(r#""{field}" must be an ISO-8601 timestamp"#),
            format!("bad_{field}"),
        )
    })?;
    parsed.format(&Rfc3339).map_err(|_| {
        ActionError::new(
            ErrorCode::Internal,
            "failed to format timestamp",
            "timestamp_format",
        )
    })
}

/// What `validate_idempotency_key` accepts. A UUID is what the doc asks for,
/// but anything opaque and bounded is safe — it is only ever compared, never
/// interpreted. The bound matters because it is a primary-key column and it
/// lands in the audit trail.
#[must_use]
pub fn valid_idempotency_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    (8..=200).contains(&bytes.len())
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

/// Header validation from the retry-safe path: the claim itself lives with the
/// db slice, but the shape refusal is pure and belongs here.
pub fn validate_idempotency_key<'a>(
    key: Option<&'a str>,
    action: &str,
) -> Result<&'a str, ActionError> {
    match key {
        None | Some("") => Err(ActionError::new(
            ErrorCode::Malformed,
            format!(r#""{action}" requires an Idempotency-Key header"#),
            "missing_idempotency_key",
        )),
        Some(k) if !valid_idempotency_key(k) => Err(ActionError::new(
            ErrorCode::Malformed,
            "Idempotency-Key must be 8-200 characters of [A-Za-z0-9._:-]",
            "bad_idempotency_key",
        )),
        Some(k) => Ok(k),
    }
}

/// Per-action numeric bounds (legacy `validateRequest` in
/// `src/moderation/service.ts`): tempban 60–1y, timeout 60–28d (Discord's own
/// ceiling), purge count 1–100, slowmode seconds 0–6h. The field is required
/// for these four verbs — `undefined` is `malformed`, not a default. All other
/// verbs carry no numeric check at this layer.
pub fn validate_moderation_numbers(
    action: ModerationAction,
    duration_seconds: Option<&Value>,
    count: Option<&Value>,
    seconds: Option<&Value>,
) -> Result<(), ActionError> {
    match action {
        ModerationAction::TempBan => {
            integer_between(duration_seconds, 60, 365 * 24 * 60 * 60, "duration_seconds")?;
        }
        ModerationAction::Timeout => {
            integer_between(duration_seconds, 60, 28 * 24 * 60 * 60, "duration_seconds")?;
        }
        ModerationAction::Purge => {
            integer_between(count, 1, 100, "count")?;
        }
        ModerationAction::Slowmode => {
            integer_between(seconds, 0, 6 * 60 * 60, "seconds")?;
        }
        _ => {}
    }
    Ok(())
}

fn integer_between(
    value: Option<&Value>,
    min: i64,
    max: i64,
    field: &str,
) -> Result<i64, ActionError> {
    match value.and_then(Value::as_i64) {
        Some(n) if (min..=max).contains(&n) => Ok(n),
        _ => Err(ActionError::new(
            ErrorCode::Malformed,
            format!(r#""{field}" must be an integer between {min} and {max}"#),
            format!("bad_{field}"),
        )),
    }
}

/// A validated scheduled-event input: Discord takes an event in a voice
/// channel OR an external one with a place written on it, never both and never
/// neither. Deciding here means the caller gets a typed error naming the field
/// instead of Discord's bare 400.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventInput {
    pub name: String,
    pub starts_at: String,
    pub ends_at: String,
    pub description: Option<String>,
    pub place: EventPlace,
}

/// Exactly one of the two Discord event placements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventPlace {
    /// Voice-channel event: resolved channel snowflake (never a raw id from
    /// the caller — always through the channel-key map).
    Channel(String),
    /// External event with a written location.
    Location(String),
}

pub fn validate_event_input(
    body: &Map<String, Value>,
    channel_keys: &HashMap<String, String>,
) -> Result<EventInput, ActionError> {
    use time::format_description::well_known::Rfc3339;
    let name = require_field_str(body, "name")?;
    if name.len() > MAX_EVENT_NAME_CHARS {
        return Err(ActionError::new(
            ErrorCode::Malformed,
            format!(r#""name" is longer than {MAX_EVENT_NAME_CHARS} characters"#),
            "name_too_long",
        ));
    }
    let starts_at = require_timestamp(body, "starts_at")?;
    let ends_at = require_timestamp(body, "ends_at")?;
    let starts = time::OffsetDateTime::parse(&starts_at, &Rfc3339)
        .expect("require_timestamp just normalised this");
    let ends = time::OffsetDateTime::parse(&ends_at, &Rfc3339)
        .expect("require_timestamp just normalised this");
    if ends <= starts {
        return Err(ActionError::new(
            ErrorCode::Malformed,
            "\"ends_at\" must be after \"starts_at\"",
            "ends_before_starts",
        ));
    }
    let description = match body.get("description") {
        None => None,
        Some(_) => {
            let s = require_field_str(body, "description")?;
            if s.len() > MAX_EVENT_DESCRIPTION_CHARS {
                return Err(ActionError::new(
                    ErrorCode::Malformed,
                    format!(
                        r#""description" is longer than {MAX_EVENT_DESCRIPTION_CHARS} characters"#
                    ),
                    "description_too_long",
                ));
            }
            Some(s.to_owned())
        }
    };
    // Exactly one placement: the caller gets a typed error naming the field.
    let has_channel = body.get("channel_key").is_some();
    let has_location = body.get("location").is_some();
    if has_channel == has_location {
        return Err(ActionError::new(
            ErrorCode::Malformed,
            "Send exactly one of \"channel_key\" or \"location\"",
            "event_place_ambiguous",
        ));
    }
    let place = if has_channel {
        let key = require_field_str(body, "channel_key")?;
        match channel_keys.get(key) {
            Some(id) => EventPlace::Channel(id.clone()),
            None => {
                return Err(ActionError::new(
                    ErrorCode::ActionNotAllowed,
                    format!(r#""{key}" is not a postable channel key"#),
                    "channel_key_unknown",
                ));
            }
        }
    } else {
        EventPlace::Location(require_field_str(body, "location")?.to_owned())
    };
    Ok(EventInput {
        name: name.to_owned(),
        starts_at,
        ends_at,
        description,
        place,
    })
}

/// `role.assign` field validation: caller names a role *key*, never a
/// snowflake. Returns the resolved role id. Reading the member first (to tell
/// "assigned" from "already held") is a Discord call and stays with the bot
/// slice.
pub fn validate_role_assign<'a>(
    body: &Map<String, Value>,
    role_keys: &'a HashMap<String, String>,
) -> Result<&'a str, ActionError> {
    require_snowflake(body, "discord_id")?;
    let role_key = require_field_str(body, "role_key")?;
    role_keys.get(role_key).map(String::as_str).ok_or_else(|| {
        ActionError::new(
            ErrorCode::ActionNotAllowed,
            format!(r#""{role_key}" is not an assignable role key"#),
            "role_key_unknown",
        )
    })
}

/// `guild.add_member` field presence: `discord_id` is a snowflake,
/// `access_token` a non-empty string. The token is validated for presence only
/// and never returned in a struct — it must not reach logs, audit rows, or
/// stored idempotency results.
pub fn validate_guild_add_member(body: &Map<String, Value>) -> Result<(), ActionError> {
    require_snowflake(body, "discord_id")?;
    require_field_str(body, "access_token")?;
    Ok(())
}

/// `announcement.post` field validation: channel through the key map, body
/// within Discord's ceiling. The message id comes back from Discord, so the
/// stored idempotency result can still tell the website *which* message it has.
pub fn validate_announcement<'a>(
    body: &Map<String, Value>,
    channel_keys: &'a HashMap<String, String>,
) -> Result<&'a str, ActionError> {
    let channel_key = require_field_str(body, "channel_key")?;
    let channel_id = channel_keys
        .get(channel_key)
        .map(String::as_str)
        .ok_or_else(|| {
            ActionError::new(
                ErrorCode::ActionNotAllowed,
                format!(r#""{channel_key}" is not a postable channel key"#),
                "channel_key_unknown",
            )
        })?;
    let content = require_field_str(body, "body")?;
    if content.len() > MAX_MESSAGE_CHARS {
        return Err(ActionError::new(
            ErrorCode::Malformed,
            format!(r#""body" is longer than {MAX_MESSAGE_CHARS} characters"#),
            "body_too_long",
        ));
    }
    Ok(channel_id)
}

/// `settings.set` value-size check: cheap ceiling on what one setting may
/// weigh. The value itself is never echoed back — the website already knows
/// what it sent, and the result is what gets stored against the idempotency
/// key and replayed.
pub fn check_setting_value_size(value: &Value) -> Result<(), ActionError> {
    if value.is_null() {
        return Ok(());
    }
    let encoded = serde_json::to_string(value).map_err(|_| {
        ActionError::new(
            ErrorCode::Malformed,
            "\"value\" could not be encoded",
            "settings_value_unencodable",
        )
    })?;
    if encoded.len() > MAX_SETTING_VALUE_BYTES {
        return Err(ActionError::new(
            ErrorCode::Malformed,
            format!(r#""value" is larger than {MAX_SETTING_VALUE_BYTES} bytes"#),
            "settings_value_too_large",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Private bind guard (bind.ts)
// ---------------------------------------------------------------------------

/// Bind-guard refusal: a config mistake must be a crash, not a quietly-exposed
/// remote control. There is deliberately no override flag.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BindError {
    #[error(
        "Refusing to start the internal actions endpoint on the wildcard address \"{0}\". Bind it to a specific private address (127.0.0.1 or the private NIC)."
    )]
    Wildcard(String),
    #[error(
        "Refusing to start the internal actions endpoint on the public address \"{0}\". This endpoint is a remote control for the Discord server and must never be reachable from the internet."
    )]
    Public(String),
}

/// Normalise a bind host: trim, lowercase, strip IPv6 brackets and zone, strip
/// the `::ffff:` prefix runtimes use for mapped v4.
#[must_use]
pub fn normalise_bind_host(host: &str) -> String {
    let mut out = host.trim().to_lowercase();
    if out.starts_with('[') && out.ends_with(']') && out.len() >= 2 {
        out = out[1..out.len() - 1].to_owned();
    }
    if let Some(zone) = out.find('%') {
        out.truncate(zone);
    }
    if let Some(stripped) = out.strip_prefix("::ffff:") {
        out = stripped.to_owned();
    }
    out
}

/// Loopback, RFC 1918, CGNAT, link-local, and IPv6 loopback / unique-local
/// (legacy `isPrivateAddress`).
#[must_use]
pub fn is_private_address(addr: &str) -> bool {
    let host = normalise_bind_host(addr);
    if host == "::1" {
        return true;
    }
    if host.starts_with("fc") || host.starts_with("fd") {
        return true;
    }
    if host.starts_with("fe80:") {
        return true;
    }
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() != 4 {
        return false;
    }
    let mut octets = [0u8; 4];
    for (i, part) in parts.iter().enumerate() {
        if part.len() > 3 || part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        let parsed: u16 = match part.parse() {
            Ok(n) => n,
            Err(_) => return false,
        };
        if parsed > 255 {
            return false;
        }
        octets[i] = parsed as u8;
    }
    let (a, b) = (octets[0], octets[1]);
    a == 127
        || a == 10
        || (a == 192 && b == 168)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
}

/// Throw unless `host` is a specific private address. The wildcards are
/// rejected by name because they are the actual mistake being guarded against:
/// `0.0.0.0` looks local in a config file and is not. Returns the normalised
/// address the listener must bind.
pub fn assert_private_bind(host: &str) -> Result<String, BindError> {
    let normal = normalise_bind_host(host);
    if normal.is_empty() || normal == "0.0.0.0" || normal == "::" || normal == "*" {
        return Err(BindError::Wildcard(host.to_owned()));
    }
    if !is_private_address(&normal) {
        return Err(BindError::Public(host.to_owned()));
    }
    Ok(normal)
}

// ---------------------------------------------------------------------------
// Request ids
// ---------------------------------------------------------------------------

/// Crockford base-32 alphabet (legacy `CROCKFORD`; I/L/O/U excluded).
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A ULID-ish request id: time-ordered, so a grep of the log sorts naturally,
/// and random enough not to collide. Randomness arrives as a parameter (16
/// bytes from the route's RNG) so this stays pure and testable. It is the join
/// key between the website's logs and ours, so it goes in every response
/// including the failures.
#[must_use]
pub fn new_request_id(now_ms: u64, rand: &[u8; 16]) -> String {
    let mut out = String::with_capacity(REQUEST_ID_LEN);
    let mut time_part = [0u8; 10];
    let mut t = now_ms;
    for i in (0..10).rev() {
        time_part[i] = CROCKFORD[(t % 32) as usize];
        t /= 32;
    }
    out.push_str(std::str::from_utf8(&time_part).expect("Crockford alphabet is ASCII"));
    for b in rand {
        out.push(CROCKFORD[(b % 32) as usize] as char);
    }
    out
}

// ---------------------------------------------------------------------------
// The pipeline (server.ts: authoriseAndRun, minus Discord + claim)
// ---------------------------------------------------------------------------

/// The four signed headers, already extracted by the route (header *names* and
/// content-type stay HTTP-layer concerns).
pub struct AuthHeaders<'a> {
    pub key_id: &'a str,
    pub timestamp: &'a str,
    pub nonce: &'a str,
    pub signature: &'a str,
}

/// A request that survived every pre-Discord check, in the exact order legacy
/// applies them. The idempotency claim and the Discord run happen after this
/// returns; the parsed body travels along so the claim can hash the same raw
/// bytes it verified.
#[derive(Debug, Clone)]
pub struct AuthDecision {
    pub key_id: String,
    pub action: String,
    pub body: Map<String, Value>,
}

/// Authorise one request against the load-bearing check order:
///
/// 1. headers present → 2. signature (unknown id and bad signature are one
///    refusal) → 3. freshness → 4. replay (nonce burns before the body is
///    read, so a replay can never reach Discord) → 5. per-key bucket → 6. body
///    parses as a JSON object → 7. action allowlisted and enabled, store and
///    settings present where required → 8. `guild.add_member`'s tighter bucket.
///
/// Buckets 5 and 8 run after verification on purpose: rate-limiting an
/// unverified key id would let anyone lock out a legitimate caller by spamming
/// its id.
#[allow(clippy::too_many_arguments)]
pub fn authorize(
    headers: &AuthHeaders<'_>,
    raw: &[u8],
    keys: &KeyRing,
    flags: &InternalFlags,
    has_store: bool,
    has_settings: bool,
    skew_seconds: u64,
    now_unix_secs: u64,
    now_ms: u64,
    nonces: &mut NonceCache,
    buckets: &mut TokenBuckets,
) -> Result<AuthDecision, ActionError> {
    if headers.key_id.is_empty()
        || headers.timestamp.is_empty()
        || headers.nonce.is_empty()
        || headers.signature.is_empty()
    {
        return Err(auth_failure("missing_auth_headers"));
    }
    if !valid_nonce_format(headers.nonce) {
        return Err(auth_failure("bad_nonce_format"));
    }
    if !keys.verify(
        headers.key_id,
        headers.signature,
        headers.timestamp,
        headers.nonce,
        raw,
    ) {
        return Err(auth_failure("bad_signature"));
    }
    if !within_skew(headers.timestamp, skew_seconds, now_unix_secs) {
        return Err(ActionError::new(
            ErrorCode::StaleRequest,
            "Timestamp is outside the ±120s window",
            "stale_timestamp",
        ));
    }
    if !nonces.offer(headers.nonce, now_ms) {
        return Err(ActionError::new(
            ErrorCode::Replayed,
            "This nonce has already been used",
            "replayed_nonce",
        ));
    }
    let per_key = buckets.take(&format!("key:{}", headers.key_id), DEFAULT_BUCKET, now_ms);
    if !per_key.allowed {
        return Err(ActionError::new(
            ErrorCode::RateLimited,
            "Rate limit exceeded for this key",
            "rate_limited_key",
        )
        .with_retry_after(per_key.retry_after_secs));
    }

    let body = parse_body_object(raw)?;
    let action = match body.get("action").and_then(Value::as_str) {
        Some(a) if !a.is_empty() => a.to_owned(),
        _ => {
            return Err(ActionError::new(
                ErrorCode::Malformed,
                "\"action\" must be a string",
                "missing_action",
            ));
        }
    };
    assert_allowed(&action, flags, has_store, has_settings)?;

    if action == "guild.add_member" {
        let per_action = buckets.take(
            &format!("key:{}:add_member", headers.key_id),
            ADD_MEMBER_BUCKET,
            now_ms,
        );
        if !per_action.allowed {
            return Err(ActionError::new(
                ErrorCode::RateLimited,
                "Rate limit exceeded for guild.add_member",
                "rate_limited_add_member",
            )
            .with_retry_after(per_action.retry_after_secs));
        }
    }

    Ok(AuthDecision {
        key_id: headers.key_id.to_owned(),
        action,
        body,
    })
}

/// Body parses as JSON and must be an object. The content-type refusal lives
/// with the route (it needs the headers); JSON shape lives here so the order
/// — after verify, skew, replay, and the key bucket — is pinned in one place.
fn parse_body_object(raw: &[u8]) -> Result<Map<String, Value>, ActionError> {
    if raw.len() > MAX_BODY_BYTES {
        return Err(ActionError::new(
            ErrorCode::Malformed,
            "Body is too large",
            "body_too_large",
        ));
    }
    let parsed: Value = serde_json::from_slice(raw).map_err(|_| {
        ActionError::new(ErrorCode::Malformed, "Body is not valid JSON", "bad_json")
    })?;
    match parsed {
        Value::Object(map) => Ok(map),
        _ => Err(ActionError::new(
            ErrorCode::Malformed,
            "Body must be a JSON object",
            "body_not_object",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Cross-implementation vectors: produced by legacy `node:crypto`
    // (`signing.ts` `sign` over the same canonical string). If these fail, the
    // Rust port no longer verifies what the website signs.
    const VEC1_BODY: &str =
        r#"{"action":"role.assign","discord_id":"123456789012345678","role_key":"member"}"#;
    const VEC1_BODY_HASH: &str = "0d467b6aead193be79e9af85b251752c07d91e9ca2aaff0ea9b447375b1c4c66";
    const VEC1_SIG: &str =
        "sha256=63286b0771e9601ad29fa5ab36ddac5bb7d06f03369254c9ebc9867f36dfeee4";
    const VEC1_SECRET: &[u8] = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const VEC1_TS: &str = "1720000000";
    const VEC1_NONCE: &str = "0123456789abcdef0123456789abcdef";

    const VEC2_BODY: &str = r#"{"action":"settings.get","key":"TWO_RAID_JOIN_THRESHOLD"}"#;
    const VEC2_BODY_HASH: &str = "cccc068066dd4ce177c75140dc2098ae1b3d2abdf8b4ff6d4304a1dc70963784";
    const VEC2_SIG: &str =
        "sha256=041d450e1dfa097f25c8d6ad019c32d6140fa48e8287b3f9d61b5750973cad2c";
    const VEC2_SECRET: &[u8] = b"second-secret-bbbbbbbbbbbbbbbbbb";
    const VEC2_TS: &str = "1720000060";
    const VEC2_NONCE: &str = "abcdef0123456789abcdef0123456789";

    fn ring() -> KeyRing {
        KeyRing::new(vec![
            SigningKey {
                id: "web".to_owned(),
                secret: VEC1_SECRET.to_vec(),
            },
            SigningKey {
                id: "web2".to_owned(),
                secret: VEC2_SECRET.to_vec(),
            },
        ])
    }

    fn map(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(m) => m,
            _ => panic!("expected object"),
        }
    }

    #[test]
    fn body_hash_matches_node_crypto() {
        assert_eq!(body_hash(VEC1_BODY.as_bytes()), VEC1_BODY_HASH);
        assert_eq!(body_hash(VEC2_BODY.as_bytes()), VEC2_BODY_HASH);
    }

    #[test]
    fn sign_matches_node_crypto() {
        assert_eq!(
            sign(VEC1_SECRET, VEC1_TS, VEC1_NONCE, VEC1_BODY.as_bytes()),
            VEC1_SIG
        );
        assert_eq!(
            sign(VEC2_SECRET, VEC2_TS, VEC2_NONCE, VEC2_BODY.as_bytes()),
            VEC2_SIG
        );
    }

    #[test]
    fn canonical_string_shape() {
        assert_eq!(
            canonical_string(VEC1_TS, VEC1_NONCE, VEC1_BODY.as_bytes()),
            format!("POST\n{ACTIONS_PATH}\n{VEC1_TS}\n{VEC1_NONCE}\n{VEC1_BODY_HASH}")
        );
    }

    #[test]
    fn keyring_verifies_known_key() {
        let keys = ring();
        assert!(keys.verify("web", VEC1_SIG, VEC1_TS, VEC1_NONCE, VEC1_BODY.as_bytes()));
        assert!(keys.verify("web2", VEC2_SIG, VEC2_TS, VEC2_NONCE, VEC2_BODY.as_bytes()));
    }

    #[test]
    fn keyring_refuses_unknown_id_and_wrong_signature() {
        let keys = ring();
        // Unknown id and wrong signature are both plain false — the caller
        // cannot probe which key ids exist.
        assert!(!keys.verify("nope", VEC1_SIG, VEC1_TS, VEC1_NONCE, VEC1_BODY.as_bytes()));
        assert!(!keys.verify("web", VEC2_SIG, VEC1_TS, VEC1_NONCE, VEC1_BODY.as_bytes()));
        assert!(!keys.verify(
            "web",
            "sha256=0000000000000000000000000000000000000000000000000000000000000000",
            VEC1_TS,
            VEC1_NONCE,
            VEC1_BODY.as_bytes()
        ));
    }

    #[test]
    fn signatures_match_rejects_length_mismatch() {
        assert!(signatures_match(VEC1_SIG, VEC1_SIG));
        assert!(!signatures_match(VEC1_SIG, "sha256=short"));
        assert!(!signatures_match("", VEC1_SIG));
    }

    #[test]
    fn parse_keys_accepts_rotation_pairs() {
        let keys = parse_keys(
            "web:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa,web2:second-secret-bbbbbbbbbbbbbbbbbb",
        )
        .expect("valid spec parses");
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].id, "web");
    }

    #[test]
    fn parse_keys_rejects_bad_specs() {
        assert_eq!(
            parse_keys("no-colon-here"),
            Err(KeySpecError::MalformedEntry)
        );
        assert_eq!(
            parse_keys("web:short"),
            Err(KeySpecError::SecretTooShort("web".to_owned()))
        );
        assert_eq!(parse_keys(""), Err(KeySpecError::NoKeys));
        assert_eq!(parse_keys("  "), Err(KeySpecError::NoKeys));
        // The error names the id, never the secret.
        let err = parse_keys("web:short").expect_err("must fail");
        assert!(!format!("{err}").contains("short") || format!("{err}").contains("web"));
    }

    #[test]
    fn skew_window_boundaries() {
        assert!(within_skew("1720000000", 120, 1_720_000_000));
        assert!(within_skew("1720000000", 120, 1_720_000_120));
        assert!(within_skew("1720000000", 120, 1_719_999_880));
        assert!(!within_skew("1720000000", 120, 1_720_000_121));
        assert!(!within_skew("1720000000", 120, 1_719_999_879));
        assert!(!within_skew("not-a-number", 120, 1_720_000_000));
        assert!(!within_skew("", 120, 1_720_000_000));
        assert!(!within_skew("1234567890123456", 120, 1_720_000_000));
    }

    #[test]
    fn nonce_format_is_32_hex() {
        assert!(valid_nonce_format(VEC1_NONCE));
        assert!(valid_nonce_format("ABCDEF0123456789ABCDEF0123456789"));
        assert!(!valid_nonce_format("short"));
        assert!(!valid_nonce_format(&("a".repeat(33))));
        assert!(!valid_nonce_format("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"));
    }

    #[test]
    fn nonce_cache_rejects_replay_until_expiry() {
        let mut cache = NonceCache::new(240);
        assert!(cache.offer("n1", 1_000));
        assert!(!cache.offer("n1", 2_000));
        assert!(cache.offer("n2", 2_000));
        // 240 s later the first nonce is usable again.
        assert!(cache.offer("n1", 1_000 + 240_000));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn buckets_allow_burst_then_deny_with_retry_after() {
        let mut buckets = TokenBuckets::new();
        for _ in 0..20 {
            assert!(buckets.take("k", DEFAULT_BUCKET, 0).allowed);
        }
        let denied = buckets.take("k", DEFAULT_BUCKET, 0);
        assert!(!denied.allowed);
        assert!(denied.retry_after_secs >= 1);
        // Independent keys do not share a bucket.
        assert!(buckets.take("other", DEFAULT_BUCKET, 0).allowed);
    }

    #[test]
    fn add_member_bucket_is_tighter() {
        let mut buckets = TokenBuckets::new();
        for _ in 0..10 {
            assert!(buckets.take("m", ADD_MEMBER_BUCKET, 0).allowed);
        }
        assert!(!buckets.take("m", ADD_MEMBER_BUCKET, 0).allowed);
    }

    #[test]
    fn buckets_refill_over_time() {
        let mut buckets = TokenBuckets::new();
        for _ in 0..20 {
            buckets.take("k", DEFAULT_BUCKET, 0);
        }
        assert!(!buckets.take("k", DEFAULT_BUCKET, 0).allowed);
        // One second refills one token at 1/s.
        assert!(buckets.take("k", DEFAULT_BUCKET, 1_000).allowed);
        assert!(!buckets.take("k", DEFAULT_BUCKET, 1_000).allowed);
    }

    #[test]
    fn error_code_table() {
        let cases = [
            (ErrorCode::Malformed, 400, false),
            (ErrorCode::Unauthorized, 401, false),
            (ErrorCode::StaleRequest, 401, false),
            (ErrorCode::ActionNotAllowed, 403, false),
            (ErrorCode::Replayed, 409, false),
            (ErrorCode::InProgress, 409, true),
            (ErrorCode::DiscordRejected, 422, false),
            (ErrorCode::RateLimited, 429, true),
            (ErrorCode::Internal, 500, true),
            (ErrorCode::DiscordUnavailable, 502, true),
            (ErrorCode::UpstreamTimeout, 504, true),
        ];
        for (code, status, retryable) in cases {
            assert_eq!(code.status(), status, "{:?}", code);
            assert_eq!(code.retryable(), retryable, "{:?}", code);
        }
        assert_eq!(auth_failure("x").message, AUTH_FAILURE_MESSAGE);
        assert_eq!(auth_failure("x").code, ErrorCode::Unauthorized);
    }

    #[test]
    fn allowlist_gates() {
        let flags = InternalFlags::from_map(&HashMap::new());
        // Phase 1 verbs need no store and no settings.
        assert!(assert_allowed("role.assign", &flags, false, false).is_ok());
        // Unknown verbs are refused even though they parse.
        let err = assert_allowed("guild.kick_everyone", &flags, true, true).expect_err("unknown");
        assert_eq!(err.code, ErrorCode::ActionNotAllowed);
        // Gated verbs stay dark by default.
        let err = assert_allowed("guild.add_member", &flags, true, true).expect_err("dark");
        assert_eq!(err.code, ErrorCode::ActionNotAllowed);
        // Key-requiring verbs refuse without a store instead of 500ing later.
        let mut vars = HashMap::new();
        vars.insert("TWO_INTERNAL_ALLOW_EVENT_CANCEL".to_owned(), "1".to_owned());
        let flags = InternalFlags::from_map(&vars);
        let err = assert_allowed("event.cancel", &flags, false, false).expect_err("no store");
        assert_eq!(err.code, ErrorCode::ActionNotAllowed);
        assert!(assert_allowed("event.cancel", &flags, true, false).is_ok());
        // Settings verbs refuse without the config store.
        let mut vars = HashMap::new();
        vars.insert("TWO_INTERNAL_ALLOW_SETTINGS".to_owned(), "1".to_owned());
        let flags = InternalFlags::from_map(&vars);
        let err = assert_allowed("settings.get", &flags, true, false).expect_err("no settings");
        assert_eq!(err.code, ErrorCode::ActionNotAllowed);
    }

    #[test]
    fn flags_enable_each_approval() {
        let on = |k: &str| {
            let mut vars = HashMap::new();
            vars.insert(k.to_owned(), "1".to_owned());
            InternalFlags::from_map(&vars)
        };
        assert!(on("TWO_INTERNAL_ALLOW_ADD_MEMBER").is_enabled("guild.add_member"));
        assert!(on("TWO_INTERNAL_ALLOW_EVENT_CANCEL").is_enabled("event.cancel"));
        assert!(on("TWO_INTERNAL_ALLOW_EVENT_READ").is_enabled("event.read"));
        let auto = on("TWO_INTERNAL_ALLOW_AUTOMATIONS");
        assert!(auto.is_enabled("automations.import"));
        assert!(auto.is_enabled("automations.export"));
        assert!(!auto.allow_automation_overwrite);
        let mut vars = HashMap::new();
        vars.insert("TWO_INTERNAL_ALLOW_AUTOMATIONS".to_owned(), "1".to_owned());
        vars.insert(
            "TWO_INTERNAL_ALLOW_AUTOMATIONS_OVERWRITE".to_owned(),
            "1".to_owned(),
        );
        assert!(InternalFlags::from_map(&vars).allow_automation_overwrite);
        // Overwrite alone (without the base flag) grants nothing.
        let mut vars = HashMap::new();
        vars.insert(
            "TWO_INTERNAL_ALLOW_AUTOMATIONS_OVERWRITE".to_owned(),
            "1".to_owned(),
        );
        let flags = InternalFlags::from_map(&vars);
        assert!(!flags.is_enabled("automations.import"));
        assert!(!flags.allow_automation_overwrite);
        let settings = on("TWO_INTERNAL_ALLOW_SETTINGS");
        assert!(settings.is_enabled("settings.get"));
        assert!(settings.is_enabled("settings.set"));
    }

    #[test]
    fn moderation_verbs_need_both_co_gates() {
        let both = {
            let mut vars = HashMap::new();
            vars.insert("TWO_INTERNAL_ALLOW_MODERATION".to_owned(), "1".to_owned());
            vars.insert("TWO_MODERATION".to_owned(), "1".to_owned());
            InternalFlags::from_map(&vars)
        };
        for action in MODERATION_ACTIONS {
            assert!(both.is_enabled(action), "{action}");
        }
        // Either gate alone leaves all nine dark: TWO_MODERATION carries no
        // TWO_INTERNAL_ prefix, so the prefix refusal never covered it.
        for key in ["TWO_INTERNAL_ALLOW_MODERATION", "TWO_MODERATION"] {
            let mut vars = HashMap::new();
            vars.insert(key.to_owned(), "1".to_owned());
            let flags = InternalFlags::from_map(&vars);
            assert!(!flags.is_enabled("moderation.ban"), "{key}");
        }
    }

    #[test]
    fn key_maps_parse_pairs() {
        let roles = build_role_keys("member:111111111111111111,vip:222222222222222222")
            .expect("valid pairs parse");
        assert_eq!(
            roles.get("member").map(String::as_str),
            Some("111111111111111111")
        );
        assert!(build_role_keys("").expect("empty is empty").is_empty());
        assert!(build_channel_keys("ann:333333333333333333")
            .expect("valid")
            .contains_key("ann"));
        assert!(build_key_map("x:not-an-id", "E", "role").is_err());
        assert!(build_key_map("no-colon", "E", "role").is_err());
        assert!(build_key_map("k:123", "E", "role").is_err());
    }

    #[test]
    fn settings_guard_refuses_env_only_and_unknown() {
        let get = map(json!({"key": "TWO_RAID_JOIN_THRESHOLD"}));
        assert_eq!(
            require_settings_key(&get, "settings.get").expect("hot key"),
            "TWO_RAID_JOIN_THRESHOLD"
        );
        // Every TWO_INTERNAL_* name is refused by the prefix arm — including
        // TWO_INTERNAL_KEYS, which no census row names.
        for key in [
            "TWO_INTERNAL_KEYS",
            "TWO_INTERNAL_ALLOW_MODERATION",
            "TWO_MODERATION",
            "DISCORD_TOKEN",
            "TWO_OWEN_USER_ID",
        ] {
            let body = map(json!({"key": key}));
            let err = require_settings_key(&body, "settings.set").expect_err("env-only");
            assert_eq!(err.code, ErrorCode::ActionNotAllowed, "{key}");
        }
        // Unclassified keys fail closed with the typo message, not the policy one.
        let body = map(json!({"key": "TWO_TYPO_KEYS"}));
        let err = require_settings_key(&body, "settings.get").expect_err("unknown");
        assert!(err.message.contains("not a setting"), "{err}");
        // Shape violations are malformed, not policy refusals.
        for key in ["lowercase", "X", &"Y".repeat(129)] {
            let body = map(json!({"key": key}));
            assert_eq!(
                require_settings_key(&body, "settings.get")
                    .expect_err("shape")
                    .code,
                ErrorCode::Malformed,
                "{key}"
            );
        }
    }

    #[test]
    fn catalog_counts_match_legacy_census() {
        // hot 41 / cold 28 / env_only 47 in settingsCatalog.ts SETTING_CLASSES.
        // Spot-check each class; the full census lives in classify_key.
        assert_eq!(
            classify_key("TWO_RAID_JOIN_THRESHOLD"),
            Some(SettingClass::Hot)
        );
        assert_eq!(classify_key("TWO_AUTOMOD"), Some(SettingClass::Cold));
        assert_eq!(classify_key("DISCORD_TOKEN"), Some(SettingClass::EnvOnly));
        assert_eq!(classify_key("TWO_NEVER_EXISTED"), None);
        assert!(is_storable_key("TWO_RAID_JOIN_THRESHOLD"));
        assert!(!is_storable_key("TWO_INTERNAL_KEYS"));
        assert!(!is_storable_key("TWO_NEVER_EXISTED"));
        assert!(is_declared_env_only("TWO_MODERATION"));
        assert!(!is_declared_env_only("TWO_NEVER_EXISTED"));
    }

    #[test]
    fn snowflake_and_field_validators() {
        assert!(is_snowflake("123456789012345678"));
        assert!(!is_snowflake("123"));
        assert!(!is_snowflake("12345678901234567890a"));
        let body = map(json!({"discord_id": "123456789012345678"}));
        assert_eq!(
            require_snowflake(&body, "discord_id").expect("valid"),
            "123456789012345678"
        );
        let body = map(json!({"discord_id": "nope"}));
        assert_eq!(
            require_snowflake(&body, "discord_id")
                .expect_err("bad")
                .code,
            ErrorCode::Malformed
        );
        let body = map(json!({}));
        assert!(require_field_str(&body, "role_key").is_err());
    }

    #[test]
    fn reason_validation_shares_slash_command_rules() {
        assert_eq!(require_reason(&json!("do it")).expect("valid"), "do it");
        assert_eq!(
            require_reason(&json!("  ")).expect_err("blank").code,
            ErrorCode::Malformed
        );
        assert_eq!(
            require_reason(&json!(42)).expect_err("non-string").code,
            ErrorCode::Malformed
        );
    }

    #[test]
    fn moderation_numeric_bounds() {
        use ModerationAction as A;
        let n = |v: i64| Some(json!(v));
        let none: Option<Value> = None;
        assert!(validate_moderation_numbers(
            A::TempBan,
            n(60).as_ref(),
            none.as_ref(),
            none.as_ref()
        )
        .is_ok());
        assert!(validate_moderation_numbers(
            A::TempBan,
            n(31_536_000).as_ref(),
            none.as_ref(),
            none.as_ref()
        )
        .is_ok());
        assert!(validate_moderation_numbers(
            A::TempBan,
            none.as_ref(),
            none.as_ref(),
            none.as_ref()
        )
        .is_err());
        assert!(validate_moderation_numbers(
            A::TempBan,
            n(59).as_ref(),
            none.as_ref(),
            none.as_ref()
        )
        .is_err());
        assert!(validate_moderation_numbers(
            A::Timeout,
            n(2_419_200).as_ref(),
            none.as_ref(),
            none.as_ref()
        )
        .is_ok());
        assert!(validate_moderation_numbers(
            A::Timeout,
            n(2_419_201).as_ref(),
            none.as_ref(),
            none.as_ref()
        )
        .is_err());
        assert!(
            validate_moderation_numbers(A::Purge, none.as_ref(), n(1).as_ref(), none.as_ref())
                .is_ok()
        );
        assert!(validate_moderation_numbers(
            A::Purge,
            none.as_ref(),
            n(101).as_ref(),
            none.as_ref()
        )
        .is_err());
        assert!(validate_moderation_numbers(
            A::Slowmode,
            none.as_ref(),
            none.as_ref(),
            n(0).as_ref()
        )
        .is_ok());
        assert!(validate_moderation_numbers(
            A::Slowmode,
            none.as_ref(),
            none.as_ref(),
            n(21_601).as_ref()
        )
        .is_err());
        // Verbs without numeric inputs carry no check here.
        assert!(
            validate_moderation_numbers(A::Ban, none.as_ref(), none.as_ref(), none.as_ref())
                .is_ok()
        );
        assert!(validate_moderation_numbers(
            A::Unlock,
            none.as_ref(),
            none.as_ref(),
            none.as_ref()
        )
        .is_ok());
    }

    #[test]
    fn event_input_requires_exactly_one_placement() {
        let keys = build_channel_keys("ann:333333333333333333").expect("valid");
        let base = || {
            map(json!({
                "name": "Party",
                "starts_at": "2026-10-01T18:00:00Z",
                "ends_at": "2026-10-01T20:00:00Z",
            }))
        };
        let mut both = base();
        both.insert("channel_key".to_owned(), json!("ann"));
        both.insert("location".to_owned(), json!("Park"));
        assert!(validate_event_input(&both, &keys).is_err());
        assert!(validate_event_input(&base(), &keys).is_err());
        let mut channel = base();
        channel.insert("channel_key".to_owned(), json!("ann"));
        let input = validate_event_input(&channel, &keys).expect("channel placement");
        assert_eq!(
            input.place,
            EventPlace::Channel("333333333333333333".to_owned())
        );
        let mut unknown = base();
        unknown.insert("channel_key".to_owned(), json!("nope"));
        assert_eq!(
            validate_event_input(&unknown, &keys)
                .expect_err("unknown key")
                .code,
            ErrorCode::ActionNotAllowed
        );
        let mut external = base();
        external.insert("location".to_owned(), json!("Park"));
        let input = validate_event_input(&external, &keys).expect("external placement");
        assert_eq!(input.place, EventPlace::Location("Park".to_owned()));
        let mut backwards = base();
        backwards.insert("location".to_owned(), json!("Park"));
        backwards.insert("ends_at".to_owned(), json!("2026-10-01T17:00:00Z"));
        assert!(validate_event_input(&backwards, &keys).is_err());
    }

    #[test]
    fn idempotency_key_pattern() {
        assert!(valid_idempotency_key(
            "550e8400-e29b-41d4-a716-446655440000"
        ));
        assert!(!valid_idempotency_key("short"));
        assert!(!valid_idempotency_key("has space here!"));
        assert!(validate_idempotency_key(None, "announcement.post").is_err());
        assert!(validate_idempotency_key(Some(""), "announcement.post").is_err());
        assert!(validate_idempotency_key(Some("valid-key_1:A"), "announcement.post").is_ok());
    }

    #[test]
    fn role_assign_resolves_through_key_map() {
        let roles = build_role_keys("member:111111111111111111").expect("valid");
        let body = map(json!({"discord_id": "123456789012345678", "role_key": "member"}));
        assert_eq!(
            validate_role_assign(&body, &roles).expect("known"),
            "111111111111111111"
        );
        let body = map(json!({"discord_id": "123456789012345678", "role_key": "owner"}));
        assert_eq!(
            validate_role_assign(&body, &roles)
                .expect_err("unknown")
                .code,
            ErrorCode::ActionNotAllowed
        );
    }

    #[test]
    fn add_member_and_announcement_fields() {
        let body = map(json!({"discord_id": "123456789012345678", "access_token": "tok"}));
        assert!(validate_guild_add_member(&body).is_ok());
        let channels = build_channel_keys("ann:333333333333333333").expect("valid");
        let body = map(json!({"channel_key": "ann", "body": "hello"}));
        assert_eq!(
            validate_announcement(&body, &channels).expect("valid"),
            "333333333333333333"
        );
        let body = map(json!({"channel_key": "ann", "body": "x".repeat(2001)}));
        assert_eq!(
            validate_announcement(&body, &channels)
                .expect_err("too long")
                .code,
            ErrorCode::Malformed
        );
    }

    #[test]
    fn setting_value_size_cap() {
        assert!(check_setting_value_size(&json!(null)).is_ok());
        assert!(check_setting_value_size(&json!("small")).is_ok());
        assert!(check_setting_value_size(&json!("x".repeat(9000))).is_err());
    }

    #[test]
    fn bind_guard_refuses_wildcard_and_public() {
        assert!(assert_private_bind("0.0.0.0").is_err());
        assert!(assert_private_bind("::").is_err());
        assert!(assert_private_bind("*").is_err());
        assert!(assert_private_bind("8.8.8.8").is_err());
        assert_eq!(
            assert_private_bind("127.0.0.1").expect("loopback"),
            "127.0.0.1"
        );
        assert_eq!(
            assert_private_bind("10.1.2.3").expect("rfc1918"),
            "10.1.2.3"
        );
        assert_eq!(
            assert_private_bind("[::ffff:127.0.0.1]").expect("mapped"),
            "127.0.0.1"
        );
        // Deliberately no override flag: the error is the whole API.
        assert!(matches!(
            assert_private_bind("0.0.0.0"),
            Err(BindError::Wildcard(_))
        ));
    }

    #[test]
    fn request_ids_are_26_crockford_chars() {
        let id = new_request_id(1_720_000_000_000, &[0u8; 16]);
        assert_eq!(id.len(), REQUEST_ID_LEN);
        assert!(id
            .bytes()
            .all(|b| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&b)));
        // Time-ordered: a later timestamp sorts later.
        let later = new_request_id(1_720_000_000_001, &[0u8; 16]);
        assert!(later > id);
    }

    fn signed_headers<'a>(
        key_id: &'a str,
        ts: &'a str,
        nonce: &'a str,
        sig: &'a str,
    ) -> AuthHeaders<'a> {
        AuthHeaders {
            key_id,
            timestamp: ts,
            nonce,
            signature: sig,
        }
    }

    #[test]
    fn pipeline_accepts_signed_role_assign() {
        let keys = ring();
        let flags = InternalFlags::from_map(&HashMap::new());
        let mut nonces = NonceCache::new(NONCE_TTL_SECONDS);
        let mut buckets = TokenBuckets::new();
        let headers = signed_headers("web", VEC1_TS, VEC1_NONCE, VEC1_SIG);
        let decision = authorize(
            &headers,
            VEC1_BODY.as_bytes(),
            &keys,
            &flags,
            false,
            false,
            SKEW_SECONDS,
            1_720_000_000,
            1_720_000_000_000,
            &mut nonces,
            &mut buckets,
        )
        .expect("valid request authorizes");
        assert_eq!(decision.action, "role.assign");
        assert_eq!(decision.key_id, "web");
        // Same nonce again is a replay — burned before the body was read.
        let headers = signed_headers("web", VEC1_TS, VEC1_NONCE, VEC1_SIG);
        let err = authorize(
            &headers,
            VEC1_BODY.as_bytes(),
            &keys,
            &flags,
            false,
            false,
            SKEW_SECONDS,
            1_720_000_000,
            1_720_000_000_001,
            &mut nonces,
            &mut buckets,
        )
        .expect_err("replay");
        assert_eq!(err.code, ErrorCode::Replayed);
    }

    #[test]
    fn pipeline_checks_signature_before_freshness() {
        let keys = ring();
        let flags = InternalFlags::from_map(&HashMap::new());
        let mut nonces = NonceCache::new(NONCE_TTL_SECONDS);
        let mut buckets = TokenBuckets::new();
        // Stale timestamp AND bad signature: the signature refusal wins,
        // because everything after it trusts the key id.
        let headers = signed_headers("web", "1000000000", VEC1_NONCE, "sha256=nope");
        let err = authorize(
            &headers,
            VEC1_BODY.as_bytes(),
            &keys,
            &flags,
            false,
            false,
            SKEW_SECONDS,
            1_720_000_000,
            1_720_000_000_000,
            &mut nonces,
            &mut buckets,
        )
        .expect_err("bad signature");
        assert_eq!(err.code, ErrorCode::Unauthorized);
        assert_eq!(err.message, AUTH_FAILURE_MESSAGE);
    }

    #[test]
    fn pipeline_rejects_stale_then_unknown_action_then_disabled() {
        let keys = ring();
        let flags = InternalFlags::from_map(&HashMap::new());
        let mut nonces = NonceCache::new(NONCE_TTL_SECONDS);
        let mut buckets = TokenBuckets::new();
        // Stale but correctly signed → stale_request.
        let sig = sign(VEC1_SECRET, "1000000000", VEC1_NONCE, VEC1_BODY.as_bytes());
        let headers = signed_headers("web", "1000000000", VEC1_NONCE, &sig);
        let err = authorize(
            &headers,
            VEC1_BODY.as_bytes(),
            &keys,
            &flags,
            false,
            false,
            SKEW_SECONDS,
            1_720_000_000,
            1_720_000_000_000,
            &mut nonces,
            &mut buckets,
        )
        .expect_err("stale");
        assert_eq!(err.code, ErrorCode::StaleRequest);
        // Unknown action → action_not_allowed (never retryable).
        let raw = br#"{"action":"guild.kick_everyone"}"#;
        let sig = sign(
            VEC1_SECRET,
            VEC1_TS,
            "11111111111111111111111111111111",
            raw,
        );
        let headers = signed_headers("web", VEC1_TS, "11111111111111111111111111111111", &sig);
        let err = authorize(
            &headers,
            raw,
            &keys,
            &flags,
            true,
            true,
            SKEW_SECONDS,
            1_720_000_000,
            1_720_000_000_001,
            &mut nonces,
            &mut buckets,
        )
        .expect_err("unknown action");
        assert_eq!(err.code, ErrorCode::ActionNotAllowed);
        assert!(!err.code.retryable());
    }

    #[test]
    fn pipeline_rate_limits_after_verify() {
        let keys = ring();
        let flags = InternalFlags::from_map(&HashMap::new());
        let mut nonces = NonceCache::new(NONCE_TTL_SECONDS);
        let mut buckets = TokenBuckets::new();
        // Exhaust the 20-burst key bucket with distinct valid nonces.
        for i in 0..20 {
            let nonce = format!("{i:032x}");
            let sig = sign(VEC1_SECRET, VEC1_TS, &nonce, VEC1_BODY.as_bytes());
            let headers = signed_headers("web", VEC1_TS, &nonce, &sig);
            authorize(
                &headers,
                VEC1_BODY.as_bytes(),
                &keys,
                &flags,
                false,
                false,
                SKEW_SECONDS,
                1_720_000_000,
                1_720_000_000_000,
                &mut nonces,
                &mut buckets,
            )
            .expect("burst allows 20");
        }
        let nonce = "ffffffffffffffffffffffffffffffff";
        let sig = sign(VEC1_SECRET, VEC1_TS, nonce, VEC1_BODY.as_bytes());
        let headers = signed_headers("web", VEC1_TS, nonce, &sig);
        let err = authorize(
            &headers,
            VEC1_BODY.as_bytes(),
            &keys,
            &flags,
            false,
            false,
            SKEW_SECONDS,
            1_720_000_000,
            1_720_000_000_000,
            &mut nonces,
            &mut buckets,
        )
        .expect_err("21st is limited");
        assert_eq!(err.code, ErrorCode::RateLimited);
        assert!(err.retry_after_secs.is_some_and(|s| s >= 1));
    }

    #[test]
    fn pipeline_malformed_bodies() {
        let keys = ring();
        let flags = InternalFlags::from_map(&HashMap::new());
        let mut nonces = NonceCache::new(NONCE_TTL_SECONDS);
        let mut buckets = TokenBuckets::new();
        let attempt = |raw: &[u8],
                       nonce: &str,
                       nonces: &mut NonceCache,
                       buckets: &mut TokenBuckets|
         -> ActionError {
            let sig = sign(VEC1_SECRET, VEC1_TS, nonce, raw);
            let headers = signed_headers("web", VEC1_TS, nonce, &sig);
            authorize(
                &headers,
                raw,
                &keys,
                &flags,
                true,
                true,
                SKEW_SECONDS,
                1_720_000_000,
                1_720_000_000_000,
                nonces,
                buckets,
            )
            .expect_err("malformed")
        };
        assert_eq!(
            attempt(
                b"not json",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                &mut nonces,
                &mut buckets
            )
            .code,
            ErrorCode::Malformed
        );
        assert_eq!(
            attempt(
                b"[1,2]",
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                &mut nonces,
                &mut buckets
            )
            .code,
            ErrorCode::Malformed
        );
        assert_eq!(
            attempt(
                br#"{"no_action":1}"#,
                "cccccccccccccccccccccccccccccccc",
                &mut nonces,
                &mut buckets
            )
            .code,
            ErrorCode::Malformed
        );
    }
}
