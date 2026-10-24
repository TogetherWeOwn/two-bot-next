//! Twitch EventSub webhook transport core (pure: no I/O, no clock, no secrets
//! beyond the opaque [`WebhookSecret`]).
//!
//! Transport decision (see `docs/stream-funnel.md`): **webhook**. A webhook
//! subscription authenticates with an app access token (client-credentials
//! flow), so there is no user token to store or refresh, and delivery is an
//! inbound HTTPS push that wakes the sleeping container through the Worker
//! instead of needing a long-lived outbound socket. WebSocket needs a user
//! access token and a connection that must outlive container idle; Conduit
//! adds shard management for no benefit at one broadcaster.
//!
//! This module is the reviewable decision core for that transport:
//!
//! - [`verify_delivery`]: HMAC-SHA256 signature over
//!   `message-id + timestamp + raw body`, compared in constant time, plus a
//!   10-minute freshness window (Twitch's replay guard).
//! - [`classify`]: the three message types (`webhook_callback_verification`,
//!   `notification`, `revocation`), strict about subscription type/version.
//! - [`plan_live_post`] with [`RecentKeys`]: broadcaster allowlist plus
//!   claim-before-post dedup so EventSub retries and replays collapse to one
//!   announcements-channel post.
//! - [`CallbackUrl`], [`create_subscription_body`], [`parse_subscriptions`] and
//!   [`reconcile`]: subscription management as pure request bodies and a
//!   create/delete plan. No Helix polling and no timers here: the bot runtime
//!   runs reconcile at startup and on a bounded interval.
//!
//! The HTTP receiver, Helix client, credential loading and durable dedup
//! ledger are separate reviewed slices; every caller of `WebhookSecret::expose()`
//! stays at the signing boundary and never logs the value.

use std::collections::{HashSet, VecDeque};

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use thiserror::Error;

use crate::secret::Secret;
use crate::stream_funnel::{
    live_announcement, live_dedup_key, parse_stream_online, valid_broadcaster_id,
    StreamFunnelError, StreamOnline, STREAM_ONLINE_TYPE, STREAM_ONLINE_VERSION,
};

/// Lowercase header names (HTTP/2 and well-behaved HTTP/1 stacks lowercase).
pub const HEADER_MESSAGE_ID: &str = "twitch-eventsub-message-id";
pub const HEADER_TIMESTAMP: &str = "twitch-eventsub-message-timestamp";
pub const HEADER_SIGNATURE: &str = "twitch-eventsub-message-signature";
pub const HEADER_MESSAGE_TYPE: &str = "twitch-eventsub-message-type";

/// Largest request body the receiver may buffer. `stream.online` bodies are
/// well under 2 KiB; the cap bounds memory before the HMAC runs.
pub const MAX_BODY_BYTES: usize = 64 * 1024;
/// Twitch: reject messages whose timestamp is more than 10 minutes off.
pub const MAX_TIMESTAMP_SKEW_MS: i64 = 10 * 60 * 1000;
/// Helix collection endpoint for EventSub subscriptions (GET/POST/DELETE).
pub const HELIX_SUBSCRIPTIONS_URL: &str = "https://api.twitch.tv/helix/eventsub/subscriptions";
/// Most broadcasters one reconcile may manage.
pub const MAX_BROADCASTERS: usize = 100;

const MESSAGE_ID_MAX: usize = 128;
const SIGNATURE_PREFIX: &str = "sha256=";
const SECRET_MIN: usize = 10;
const SECRET_MAX: usize = 100;
const CHALLENGE_MAX: usize = 256;
const SUBSCRIPTION_ID_MAX: usize = 64;
const CALLBACK_MAX: usize = 200;
/// How many distinct go-lives the dedup window remembers.
const RECENT_CAPACITY: usize = 1024;
/// A claimed go-live is remembered this long (retries land within minutes).
const RECENT_TTL_MS: i64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EventSubError {
    #[error("Webhook secret must be 10-100 printable ASCII characters.")]
    InvalidSecret,
    #[error("A required EventSub header is missing or malformed.")]
    InvalidHeader,
    #[error("Request body is too large.")]
    BodyTooLarge,
    #[error("Signature does not match.")]
    BadSignature,
    #[error("Message timestamp is invalid.")]
    InvalidTimestamp,
    #[error("Message timestamp is outside the freshness window.")]
    StaleTimestamp,
    #[error("Body is not a valid EventSub message.")]
    InvalidBody,
    #[error("Body contains a duplicate JSON object key.")]
    DuplicateKeys,
    #[error("Message is not for the stream.online v1 subscription.")]
    WrongSubscription,
    #[error("Verification challenge is invalid.")]
    InvalidChallenge,
    #[error("Callback URL is invalid.")]
    InvalidCallback,
    #[error("Broadcaster id is invalid.")]
    InvalidBroadcasterId,
    #[error("Too many broadcasters.")]
    TooManyBroadcasters,
}

/// Bounded metric/log label for an [`EventSubError`] variant. Total over the
/// enum: a future variant fails compilation here until the
/// vocabulary grows with it, so labels can never silently collapse through
/// `bounded_index`. Every label is a member of
/// [`crate::metrics::EVENTSUB_DELIVERY_KINDS`]; unknown strings still
/// collapse to the trailing `other`.
pub fn eventsub_error_label(error: &EventSubError) -> &'static str {
    match error {
        EventSubError::InvalidSecret => "invalid_secret",
        EventSubError::InvalidHeader => "invalid_header",
        EventSubError::BodyTooLarge => "body_too_large",
        EventSubError::BadSignature => "bad_signature",
        EventSubError::InvalidTimestamp => "invalid_timestamp",
        EventSubError::StaleTimestamp => "stale_timestamp",
        EventSubError::InvalidBody => "invalid_body",
        EventSubError::DuplicateKeys => "duplicate_keys",
        EventSubError::WrongSubscription => "wrong_subscription",
        EventSubError::InvalidChallenge => "invalid_challenge",
        EventSubError::InvalidCallback => "invalid_callback",
        EventSubError::InvalidBroadcasterId => "invalid_broadcaster_id",
        EventSubError::TooManyBroadcasters => "too_many_broadcasters",
    }
}

/// The EventSub signing secret handed to Twitch at subscription creation.
/// Twitch requires 10-100 ASCII characters. The value only leaves through
/// `WebhookSecret::expose()` at the HMAC and subscription-body boundaries.
#[derive(Clone, PartialEq, Eq)]
pub struct WebhookSecret(Secret<String>);

impl WebhookSecret {
    /// Validate Twitch's secret constraints.
    pub fn new(value: String) -> Result<Self, EventSubError> {
        if !(SECRET_MIN..=SECRET_MAX).contains(&value.len())
            || !value.bytes().all(|b| b.is_ascii_graphic())
        {
            return Err(EventSubError::InvalidSecret);
        }
        Ok(Self(Secret::new(value)))
    }

    fn expose(&self) -> &str {
        self.0.expose()
    }
}

impl std::fmt::Debug for WebhookSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// The four EventSub delivery headers, borrowed from the request.
#[derive(Debug, Clone, Copy)]
pub struct DeliveryHeaders<'a> {
    message_id: &'a str,
    timestamp: &'a str,
    signature: &'a str,
    message_type: &'a str,
}

impl<'a> DeliveryHeaders<'a> {
    /// Build from a case-normalized lookup (`name` is already lowercase).
    /// The id is bounded and restricted to a safe alphabet because it feeds
    /// the HMAC and may be logged by callers.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<&'a str>) -> Result<Self, EventSubError> {
        let get = |name: &str| lookup(name).ok_or(EventSubError::InvalidHeader);
        let message_id = get(HEADER_MESSAGE_ID)?;
        if message_id.is_empty()
            || message_id.len() > MESSAGE_ID_MAX
            || !message_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(EventSubError::InvalidHeader);
        }
        Ok(Self {
            message_id,
            timestamp: get(HEADER_TIMESTAMP)?,
            signature: get(HEADER_SIGNATURE)?,
            message_type: get(HEADER_MESSAGE_TYPE)?,
        })
    }

    /// The `Twitch-Eventsub-Message-Type` value.
    #[must_use]
    pub fn message_type(&self) -> &str {
        self.message_type
    }
}

/// Verify a delivery before any JSON is parsed. Order: size cap, signature
/// (constant-time), then freshness, so an unauthenticated sender learns
/// nothing about the clock.
pub fn verify_delivery(
    secret: &WebhookSecret,
    headers: &DeliveryHeaders<'_>,
    body: &[u8],
    now_ms: i64,
) -> Result<(), EventSubError> {
    if body.len() > MAX_BODY_BYTES {
        return Err(EventSubError::BodyTooLarge);
    }
    let provided = headers
        .signature
        .strip_prefix(SIGNATURE_PREFIX)
        .and_then(|hex_part| hex::decode(hex_part).ok())
        .ok_or(EventSubError::BadSignature)?;
    let mut mac = <Hmac<Sha256>>::new_from_slice(secret.expose().as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(headers.message_id.as_bytes());
    mac.update(headers.timestamp.as_bytes());
    mac.update(body);
    // `verify_slice` compares in constant time.
    mac.verify_slice(&provided)
        .map_err(|_| EventSubError::BadSignature)?;
    let sent = crate::funnel::parse_iso_millis(headers.timestamp)
        .ok_or(EventSubError::InvalidTimestamp)?;
    if now_ms.saturating_sub(sent).abs() > MAX_TIMESTAMP_SKEW_MS {
        return Err(EventSubError::StaleTimestamp);
    }
    Ok(())
}

/// Why Twitch revoked a subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevocationReason {
    UserRemoved,
    AuthorizationRevoked,
    NotificationFailuresExceeded,
    VersionRemoved,
    Other,
}

impl RevocationReason {
    /// Bounded log scalar for the revocation ack line: Twitch's revocation
    /// `status` vocabulary, so revocation counts by reason are answerable
    /// from logs alone. No id, header, body or secret travels with it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserRemoved => "user_removed",
            Self::AuthorizationRevoked => "authorization_revoked",
            Self::NotificationFailuresExceeded => "notification_failures_exceeded",
            Self::VersionRemoved => "version_removed",
            Self::Other => "other",
        }
    }
}

/// A verified delivery, classified by message type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// Echo `challenge` back as the `text/plain` 200 body.
    Challenge(String),
    /// A genuine go-live to announce.
    Notification(StreamOnline),
    /// Twitch dropped the subscription; reconcile to restore it.
    Revocation {
        subscription_id: String,
        reason: RevocationReason,
    },
    /// An unknown message type: acknowledge with 2xx and do nothing.
    Ignored,
}

fn subscription_is_stream_online(subscription: &serde_json::Value) -> bool {
    subscription.get("type").and_then(serde_json::Value::as_str) == Some(STREAM_ONLINE_TYPE)
        && subscription
            .get("version")
            .and_then(serde_json::Value::as_str)
            == Some(STREAM_ONLINE_VERSION)
}

fn valid_subscription_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= SUBSCRIPTION_ID_MAX
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Classify a **verified** delivery. Call [`verify_delivery`] first. The
/// receiver acknowledges known deterministic notification errors with 2xx rather
/// than inviting Twitch retries. Unknown/future classification errors remain
/// retryable; transient send or durable-ledger failures are also non-2xx.
pub fn classify(message_type: &str, body: &[u8]) -> Result<Delivery, EventSubError> {
    if !matches!(
        message_type,
        "webhook_callback_verification" | "notification" | "revocation"
    ) {
        return Ok(Delivery::Ignored);
    }
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| EventSubError::InvalidBody)?;
    // Second pass over bytes that already parsed: syntax and nesting depth
    // are proven, so the only refusal left is a repeated key.
    crate::duplicate_keys::reject_duplicate_keys(body).map_err(|_| EventSubError::DuplicateKeys)?;
    let subscription = value
        .get("subscription")
        .filter(|subscription| subscription.is_object())
        .ok_or(EventSubError::InvalidBody)?;
    if !subscription_is_stream_online(subscription) {
        return Err(EventSubError::WrongSubscription);
    }
    match message_type {
        "webhook_callback_verification" => {
            let challenge = value
                .get("challenge")
                .and_then(serde_json::Value::as_str)
                .filter(|c| {
                    !c.is_empty()
                        && c.len() <= CHALLENGE_MAX
                        && c.bytes().all(|b| b.is_ascii_graphic())
                })
                .ok_or(EventSubError::InvalidChallenge)?;
            Ok(Delivery::Challenge(challenge.to_owned()))
        }
        "revocation" => {
            let subscription_id = subscription
                .get("id")
                .and_then(serde_json::Value::as_str)
                .filter(|id| valid_subscription_id(id))
                .ok_or(EventSubError::InvalidBody)?
                .to_owned();
            let reason = match subscription
                .get("status")
                .and_then(serde_json::Value::as_str)
            {
                Some("user_removed") => RevocationReason::UserRemoved,
                Some("authorization_revoked") => RevocationReason::AuthorizationRevoked,
                Some("notification_failures_exceeded") => {
                    RevocationReason::NotificationFailuresExceeded
                }
                Some("version_removed") => RevocationReason::VersionRemoved,
                _ => RevocationReason::Other,
            };
            Ok(Delivery::Revocation {
                subscription_id,
                reason,
            })
        }
        _ => parse_stream_online(&value)
            .map(Delivery::Notification)
            .map_err(|_| EventSubError::InvalidBody),
    }
}

/// Bounded, time-limited set of claimed go-live keys. Claim before posting;
/// [`RecentKeys::release`] a key when the post failed and a Twitch retry
/// should be allowed to try again. In-memory only: a durable ledger is a
/// separate slice, so a crash between post and acknowledgement can still
/// double-post once.
#[derive(Debug, Default)]
pub struct RecentKeys {
    order: VecDeque<(String, i64)>,
    keys: HashSet<String>,
}

impl RecentKeys {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn evict(&mut self, now_ms: i64) {
        while let Some((key, claimed_at)) = self.order.front() {
            if self.order.len() > RECENT_CAPACITY
                || now_ms.saturating_sub(*claimed_at) > RECENT_TTL_MS
            {
                self.keys.remove(key);
                self.order.pop_front();
            } else {
                break;
            }
        }
    }

    /// Returns `true` when `key` was not already claimed (the caller owns the
    /// post), `false` for a retry or replay.
    pub fn claim(&mut self, key: &str, now_ms: i64) -> bool {
        self.evict(now_ms);
        if self.keys.contains(key) {
            return false;
        }
        self.keys.insert(key.to_owned());
        self.order.push_back((key.to_owned(), now_ms));
        self.evict(now_ms);
        true
    }

    /// Forget a claim whose post failed so a retry can try again.
    pub fn release(&mut self, key: &str) {
        if self.keys.remove(key) {
            self.order.retain(|(k, _)| k != key);
        }
    }
}

/// Why a notification produced no post.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// The broadcaster is not in the configured allowlist.
    BroadcasterNotAllowed,
    /// This go-live was already claimed (retry or replay).
    Duplicate,
}

/// A rendered post. Send with mentions suppressed (empty `allowed_mentions`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LivePost {
    /// Claim key; pass to [`RecentKeys::release`] if the send fails.
    pub dedup_key: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationPlan {
    Post(LivePost),
    Skip(SkipReason),
}

/// Decide what one verified go-live does. The allowlist is checked first (a
/// valid signature proves Twitch sent it, not that we asked for it), then the
/// post is rendered from the validated invite, and only then is the claim taken
/// so an invalid invite never burns it.
pub fn plan_live_post(
    stream: &StreamOnline,
    discord_invite_url: &str,
    allowed_broadcasters: &[String],
    recent: &mut RecentKeys,
    now_ms: i64,
) -> Result<NotificationPlan, StreamFunnelError> {
    if !allowed_broadcasters.contains(&stream.broadcaster_user_id) {
        return Ok(NotificationPlan::Skip(SkipReason::BroadcasterNotAllowed));
    }
    // Render first so an invalid invite never burns the claim.
    let content = live_announcement(stream, None, discord_invite_url)?;
    let dedup_key = live_dedup_key(stream);
    if !recent.claim(&dedup_key, now_ms) {
        return Ok(NotificationPlan::Skip(SkipReason::Duplicate));
    }
    Ok(NotificationPlan::Post(LivePost { dedup_key, content }))
}

/// The public HTTPS callback Twitch posts to. Twitch requires HTTPS on port
/// 443. The input must be canonical: the parsed form must equal the trimmed
/// input, so explicit `:443` and other parser-normalized forms are rejected
/// along with userinfo, odd ports, query and fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallbackUrl(String);

impl CallbackUrl {
    pub fn new(value: &str) -> Result<Self, EventSubError> {
        let trimmed = value.trim();
        let parsed = url::Url::parse(trimmed).map_err(|_| EventSubError::InvalidCallback)?;
        let ok = parsed.scheme() == "https"
            && parsed.host().is_some()
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.port().is_none()
            && parsed.query().is_none()
            && parsed.fragment().is_none()
            && parsed.as_str() == trimmed
            && trimmed.len() <= CALLBACK_MAX;
        if ok {
            Ok(Self(trimmed.to_owned()))
        } else {
            Err(EventSubError::InvalidCallback)
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// JSON body for `POST /helix/eventsub/subscriptions`. It embeds the signing
/// secret, so it is returned as a [`Secret`]: expose it only to the HTTP
/// client and never log it.
pub fn create_subscription_body(
    broadcaster_user_id: &str,
    callback: &CallbackUrl,
    secret: &WebhookSecret,
) -> Result<Secret<String>, EventSubError> {
    if !valid_broadcaster_id(broadcaster_user_id) {
        return Err(EventSubError::InvalidBroadcasterId);
    }
    let body = serde_json::json!({
        "type": STREAM_ONLINE_TYPE,
        "version": STREAM_ONLINE_VERSION,
        "condition": {"broadcaster_user_id": broadcaster_user_id},
        "transport": {
            "method": "webhook",
            "callback": callback.as_str(),
            "secret": secret.expose(),
        },
    });
    Ok(Secret::new(body.to_string()))
}

/// `DELETE` URL for one subscription id.
pub fn delete_subscription_url(id: &str) -> Result<String, EventSubError> {
    if !valid_subscription_id(id) {
        return Err(EventSubError::InvalidBody);
    }
    Ok(format!("{HELIX_SUBSCRIPTIONS_URL}?id={id}"))
}

/// One existing `stream.online` v1 webhook subscription from Helix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingSubscription {
    pub id: String,
    pub status: String,
    pub broadcaster_user_id: String,
    pub callback: String,
}

impl ExistingSubscription {
    /// Twitch states that will (or may soon) deliver notifications.
    pub(crate) fn healthy(&self) -> bool {
        matches!(
            self.status.as_str(),
            "enabled" | "webhook_callback_verification_pending"
        )
    }
}

/// Extract `stream.online` v1 webhook subscriptions from a `GET` response
/// (`{"data": [...]}`). Entries of any other type, version or transport, and
/// malformed entries, are skipped rather than failing the whole listing.
#[must_use]
pub fn parse_subscriptions(response: &serde_json::Value) -> Vec<ExistingSubscription> {
    let text = |value: &serde_json::Value, field: &str| {
        value
            .get(field)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    response
        .get("data")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|entry| subscription_is_stream_online(entry))
        .filter_map(|entry| {
            let transport = entry.get("transport")?;
            if transport.get("method").and_then(serde_json::Value::as_str) != Some("webhook") {
                return None;
            }
            let id = text(entry, "id").filter(|id| valid_subscription_id(id))?;
            let broadcaster_user_id = text(entry.get("condition")?, "broadcaster_user_id")
                .filter(|b| valid_broadcaster_id(b))?;
            Some(ExistingSubscription {
                id,
                status: text(entry, "status")?,
                broadcaster_user_id,
                callback: text(transport, "callback")?,
            })
        })
        .collect()
}

/// One Helix call the reconciler wants made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubscriptionAction {
    Delete { id: String },
    Create { broadcaster_user_id: String },
}

/// Diff the desired broadcasters against existing subscriptions. Only
/// subscriptions on **our** callback are ever touched. Deletes come first
/// (Twitch answers 409 to a duplicate create), then creates, each sorted so
/// the plan is deterministic. Failed states are deleted and re-created.
pub fn reconcile(
    desired: &[String],
    callback: &CallbackUrl,
    existing: &[ExistingSubscription],
) -> Result<Vec<SubscriptionAction>, EventSubError> {
    if desired.len() > MAX_BROADCASTERS {
        return Err(EventSubError::TooManyBroadcasters);
    }
    if desired.iter().any(|id| !valid_broadcaster_id(id)) {
        return Err(EventSubError::InvalidBroadcasterId);
    }
    let mut deletes: Vec<String> = Vec::new();
    let mut kept: HashSet<&str> = HashSet::new();
    for sub in existing.iter().filter(|s| s.callback == callback.as_str()) {
        let wanted = desired.contains(&sub.broadcaster_user_id);
        if wanted && sub.healthy() && kept.insert(sub.broadcaster_user_id.as_str()) {
            continue;
        }
        deletes.push(sub.id.clone());
    }
    deletes.sort();
    deletes.dedup();
    let mut creates: Vec<&String> = desired
        .iter()
        .filter(|id| !kept.contains(id.as_str()))
        .collect();
    creates.sort();
    creates.dedup();
    Ok(deletes
        .into_iter()
        .map(|id| SubscriptionAction::Delete { id })
        .chain(creates.into_iter().map(|id| SubscriptionAction::Create {
            broadcaster_user_id: id.clone(),
        }))
        .collect())
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use serde_json::json;

    static NEXT_TEST_SECRET: AtomicU64 = AtomicU64::new(1);

    const NOW_MS: i64 = 1_760_000_000_000;
    const TS: &str = "2025-10-09T08:53:20.123456789Z";
    const ID: &str = "e76c6bd4-55c9-4987-8304-da1588d8988b";
    const INVITE: &str = "https://discord.gg/example";

    #[test]
    fn eventsub_error_labels_are_distinct_vocab_members() {
        let errors = [
            EventSubError::InvalidSecret,
            EventSubError::InvalidHeader,
            EventSubError::BodyTooLarge,
            EventSubError::BadSignature,
            EventSubError::InvalidTimestamp,
            EventSubError::StaleTimestamp,
            EventSubError::InvalidBody,
            EventSubError::DuplicateKeys,
            EventSubError::WrongSubscription,
            EventSubError::InvalidChallenge,
            EventSubError::InvalidCallback,
            EventSubError::InvalidBroadcasterId,
            EventSubError::TooManyBroadcasters,
        ];
        let mut labels: Vec<&str> = errors.iter().map(eventsub_error_label).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), errors.len(), "labels: {labels:?}");
        for label in &labels {
            assert!(
                crate::metrics::EVENTSUB_DELIVERY_KINDS.contains(label),
                "{label} must be a bounded delivery kind"
            );
        }
    }

    fn test_secret() -> WebhookSecret {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time is after the Unix epoch")
            .as_nanos();
        let nonce = NEXT_TEST_SECRET.fetch_add(1, Ordering::Relaxed);
        WebhookSecret::new(format!("{timestamp:032x}{nonce:016x}"))
            .expect("generated test secret is valid")
    }

    fn sign(secret: &WebhookSecret, id: &str, ts: &str, body: &[u8]) -> String {
        let mut mac = <Hmac<Sha256>>::new_from_slice(secret.expose().as_bytes()).unwrap();
        mac.update(id.as_bytes());
        mac.update(ts.as_bytes());
        mac.update(body);
        format!(
            "{SIGNATURE_PREFIX}{}",
            hex::encode(mac.finalize().into_bytes())
        )
    }

    fn headers<'a>(id: &'a str, ts: &'a str, sig: &'a str, kind: &'a str) -> DeliveryHeaders<'a> {
        DeliveryHeaders::from_lookup(|name| match name {
            HEADER_MESSAGE_ID => Some(id),
            HEADER_TIMESTAMP => Some(ts),
            HEADER_SIGNATURE => Some(sig),
            HEADER_MESSAGE_TYPE => Some(kind),
            _ => None,
        })
        .unwrap()
    }

    fn live_body() -> Vec<u8> {
        json!({
            "subscription": {
                "id": "sub-1", "type": "stream.online", "version": "1",
                "status": "enabled",
                "condition": {"broadcaster_user_id": "1337"},
            },
            "event": {
                "id": "9001", "broadcaster_user_id": "1337",
                "broadcaster_user_login": "cool_user",
                "broadcaster_user_name": "Cool_User",
                "type": "live", "started_at": "2025-10-09T08:50:00Z",
            },
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn valid_signature_and_fresh_timestamp_verify() {
        let body = live_body();
        let signing_secret = test_secret();
        let sig = sign(&signing_secret, ID, TS, &body);
        let h = headers(ID, TS, &sig, "notification");
        assert_eq!(verify_delivery(&signing_secret, &h, &body, NOW_MS), Ok(()));
    }

    #[test]
    fn tampered_body_wrong_secret_and_bad_formats_fail() {
        let body = live_body();
        let signing_secret = test_secret();
        let sig = sign(&signing_secret, ID, TS, &body);
        let h = headers(ID, TS, &sig, "notification");
        let mut tampered = body.clone();
        tampered.push(b' ');
        assert_eq!(
            verify_delivery(&signing_secret, &h, &tampered, NOW_MS),
            Err(EventSubError::BadSignature)
        );
        let other_secret = test_secret();
        let other = sign(&other_secret, ID, TS, &body);
        let h = headers(ID, TS, &other, "notification");
        assert_eq!(
            verify_delivery(&signing_secret, &h, &body, NOW_MS),
            Err(EventSubError::BadSignature)
        );
        for bad in [
            "",
            "sha256=",
            "sha256=zz",
            "md5=abcd",
            &sig[SIGNATURE_PREFIX.len()..],
        ] {
            let h = headers(ID, TS, bad, "notification");
            assert_eq!(
                verify_delivery(&signing_secret, &h, &body, NOW_MS),
                Err(EventSubError::BadSignature),
                "{bad}"
            );
        }
        // The id and timestamp are part of the signed message.
        let h = headers("other-id", TS, &sig, "notification");
        assert_eq!(
            verify_delivery(&signing_secret, &h, &body, NOW_MS),
            Err(EventSubError::BadSignature)
        );
    }

    #[test]
    fn stale_future_and_garbage_timestamps_fail_after_signature() {
        let body = live_body();
        let signing_secret = test_secret();
        let sig = sign(&signing_secret, ID, TS, &body);
        let h = headers(ID, TS, &sig, "notification");
        let sent = crate::funnel::parse_iso_millis(TS).unwrap();
        assert_eq!(sent, 1_760_000_000_123);
        let edge = sent + MAX_TIMESTAMP_SKEW_MS;
        assert_eq!(verify_delivery(&signing_secret, &h, &body, edge), Ok(()));
        assert_eq!(
            verify_delivery(&signing_secret, &h, &body, edge + 1),
            Err(EventSubError::StaleTimestamp)
        );
        assert_eq!(
            verify_delivery(&signing_secret, &h, &body, sent - MAX_TIMESTAMP_SKEW_MS - 1),
            Err(EventSubError::StaleTimestamp)
        );
        let sig = sign(&signing_secret, ID, "not-a-time", &body);
        let h = headers(ID, "not-a-time", &sig, "notification");
        assert_eq!(
            verify_delivery(&signing_secret, &h, &body, NOW_MS),
            Err(EventSubError::InvalidTimestamp)
        );
    }

    #[test]
    fn oversize_body_is_refused_before_hashing() {
        let body = vec![b'x'; MAX_BODY_BYTES + 1];
        let signing_secret = test_secret();
        let sig = sign(&signing_secret, ID, TS, &body);
        let h = headers(ID, TS, &sig, "notification");
        assert_eq!(
            verify_delivery(&signing_secret, &h, &body, NOW_MS),
            Err(EventSubError::BodyTooLarge)
        );
    }

    #[test]
    fn headers_require_all_four_and_a_safe_id() {
        let all = |name: &str| match name {
            HEADER_MESSAGE_ID => Some(ID),
            HEADER_TIMESTAMP => Some(TS),
            HEADER_SIGNATURE => Some("sha256=00"),
            HEADER_MESSAGE_TYPE => Some("notification"),
            _ => None,
        };
        assert!(DeliveryHeaders::from_lookup(all).is_ok());
        for missing in [
            HEADER_MESSAGE_ID,
            HEADER_TIMESTAMP,
            HEADER_SIGNATURE,
            HEADER_MESSAGE_TYPE,
        ] {
            let lookup = |name: &str| if name == missing { None } else { all(name) };
            assert_eq!(
                DeliveryHeaders::from_lookup(lookup).unwrap_err(),
                EventSubError::InvalidHeader
            );
        }
        let long = "a".repeat(MESSAGE_ID_MAX + 1);
        for bad in ["", "has space", "new\nline", long.as_str()] {
            let lookup = |name: &str| {
                if name == HEADER_MESSAGE_ID {
                    Some(bad)
                } else {
                    all(name)
                }
            };
            assert_eq!(
                DeliveryHeaders::from_lookup(lookup).unwrap_err(),
                EventSubError::InvalidHeader,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn secret_enforces_twitch_limits_and_never_prints() {
        assert!(WebhookSecret::new("a".repeat(9)).is_err());
        assert!(WebhookSecret::new("a".repeat(101)).is_err());
        assert!(WebhookSecret::new("has space in it".to_owned()).is_err());
        assert!(WebhookSecret::new("naïve-secret-é".to_owned()).is_err());
        assert!(WebhookSecret::new("a".repeat(10)).is_ok());
        assert!(WebhookSecret::new("a".repeat(100)).is_ok());
        let signing_secret = test_secret();
        let shown = format!("{signing_secret:?} {:#?}", Some(signing_secret.clone()));
        assert!(
            !shown.contains(signing_secret.expose()),
            "WebhookSecret Debug must redact the signing secret"
        );
        assert!(shown.contains("[REDACTED]"));
    }

    #[test]
    fn classify_notification_challenge_revocation_and_unknown() {
        let delivery = classify("notification", &live_body()).unwrap();
        assert!(
            matches!(delivery, Delivery::Notification(ref s) if s.broadcaster_user_id == "1337")
        );

        let challenge = json!({
            "challenge": "pogchamp-kappa-360noscope-vohiyo",
            "subscription": {
                "id": "sub-1", "type": "stream.online", "version": "1",
                "status": "webhook_callback_verification_pending",
                "condition": {"broadcaster_user_id": "1337"},
            },
        })
        .to_string();
        assert_eq!(
            classify("webhook_callback_verification", challenge.as_bytes()),
            Ok(Delivery::Challenge(
                "pogchamp-kappa-360noscope-vohiyo".to_owned()
            ))
        );

        let revocation = json!({
            "subscription": {
                "id": "sub-1", "type": "stream.online", "version": "1",
                "status": "authorization_revoked",
                "condition": {"broadcaster_user_id": "1337"},
            },
        })
        .to_string();
        assert_eq!(
            classify("revocation", revocation.as_bytes()),
            Ok(Delivery::Revocation {
                subscription_id: "sub-1".to_owned(),
                reason: RevocationReason::AuthorizationRevoked,
            })
        );

        assert_eq!(classify("something_new", b"{}"), Ok(Delivery::Ignored));
        assert_eq!(
            classify("notification", b"not json"),
            Err(EventSubError::InvalidBody)
        );
    }

    #[test]
    fn revocation_statuses_map_to_bounded_log_scalars() {
        // Twitch's revocation `status` vocabulary plus the unmapped collapse:
        // the receiver logs only `RevocationReason::as_str`, so log counts by
        // reason stay exact without header or body bytes leaving the process.
        for (status, reason, scalar) in [
            (
                "user_removed",
                RevocationReason::UserRemoved,
                "user_removed",
            ),
            (
                "authorization_revoked",
                RevocationReason::AuthorizationRevoked,
                "authorization_revoked",
            ),
            (
                "notification_failures_exceeded",
                RevocationReason::NotificationFailuresExceeded,
                "notification_failures_exceeded",
            ),
            (
                "version_removed",
                RevocationReason::VersionRemoved,
                "version_removed",
            ),
            ("gone_without_notice", RevocationReason::Other, "other"),
        ] {
            let body = json!({
                "subscription": {
                    "id": "sub-9", "type": "stream.online", "version": "1",
                    "status": status,
                    "condition": {"broadcaster_user_id": "1337"},
                },
            })
            .to_string();
            assert_eq!(
                classify("revocation", body.as_bytes()),
                Ok(Delivery::Revocation {
                    subscription_id: "sub-9".to_owned(),
                    reason,
                }),
                "status {status}"
            );
            assert_eq!(reason.as_str(), scalar, "status {status}");
        }
    }

    #[test]
    fn classify_rejects_other_subscriptions_and_bad_challenges() {
        let offline = json!({
            "subscription": {"id": "s", "type": "stream.offline", "version": "1"},
            "event": {},
        })
        .to_string();
        assert_eq!(
            classify("notification", offline.as_bytes()),
            Err(EventSubError::WrongSubscription)
        );
        let v2 = json!({"subscription": {"id": "s", "type": "stream.online", "version": "2"}})
            .to_string();
        assert_eq!(
            classify("revocation", v2.as_bytes()),
            Err(EventSubError::WrongSubscription)
        );
        for challenge in [
            json!(""),
            json!("has space"),
            json!("a".repeat(CHALLENGE_MAX + 1)),
            json!(7),
            json!("<b>\n"),
        ] {
            let body = json!({
                "challenge": challenge,
                "subscription": {"id": "s", "type": "stream.online", "version": "1"},
            })
            .to_string();
            assert_eq!(
                classify("webhook_callback_verification", body.as_bytes()),
                Err(EventSubError::InvalidChallenge),
                "{challenge}"
            );
        }
        // A rerun is not a go-live: the helper rejects it and so does the transport.
        let rerun = String::from_utf8(live_body())
            .unwrap()
            .replace("\"live\"", "\"rerun\"");
        assert_eq!(
            classify("notification", rerun.as_bytes()),
            Err(EventSubError::InvalidBody)
        );
    }

    fn stream(started_at: &str) -> StreamOnline {
        StreamOnline {
            broadcaster_user_id: "1337".to_owned(),
            broadcaster_login: "cool_user".to_owned(),
            started_at: started_at.to_owned(),
        }
    }

    #[test]
    fn one_post_per_go_live_across_retries_and_replays() {
        let allowed = vec!["1337".to_owned()];
        let mut recent = RecentKeys::new();
        let first = plan_live_post(
            &stream("2025-10-09T08:50:00Z"),
            INVITE,
            &allowed,
            &mut recent,
            NOW_MS,
        )
        .unwrap();
        let NotificationPlan::Post(post) = first else {
            panic!("first delivery must post");
        };
        assert!(post.content.contains("https://www.twitch.tv/cool_user"));
        assert!(post.content.contains(INVITE));
        for _ in 0..3 {
            assert_eq!(
                plan_live_post(
                    &stream("2025-10-09T08:50:00Z"),
                    INVITE,
                    &allowed,
                    &mut recent,
                    NOW_MS + 1000
                )
                .unwrap(),
                NotificationPlan::Skip(SkipReason::Duplicate)
            );
        }
        // A new stream start is a new go-live.
        assert!(matches!(
            plan_live_post(
                &stream("2025-10-09T20:00:00Z"),
                INVITE,
                &allowed,
                &mut recent,
                NOW_MS + 2000
            )
            .unwrap(),
            NotificationPlan::Post(_)
        ));
    }

    #[test]
    fn failed_send_releases_the_claim_for_the_retry() {
        let allowed = vec!["1337".to_owned()];
        let mut recent = RecentKeys::new();
        let s = stream("2025-10-09T08:50:00Z");
        let NotificationPlan::Post(post) =
            plan_live_post(&s, INVITE, &allowed, &mut recent, NOW_MS).unwrap()
        else {
            panic!("post");
        };
        recent.release(&post.dedup_key);
        assert!(matches!(
            plan_live_post(&s, INVITE, &allowed, &mut recent, NOW_MS + 1).unwrap(),
            NotificationPlan::Post(_)
        ));
    }

    #[test]
    fn unlisted_broadcaster_and_bad_invite_do_not_claim() {
        let mut recent = RecentKeys::new();
        let s = stream("2025-10-09T08:50:00Z");
        assert_eq!(
            plan_live_post(&s, INVITE, &["42".to_owned()], &mut recent, NOW_MS).unwrap(),
            NotificationPlan::Skip(SkipReason::BroadcasterNotAllowed)
        );
        assert_eq!(
            plan_live_post(&s, INVITE, &[], &mut recent, NOW_MS).unwrap(),
            NotificationPlan::Skip(SkipReason::BroadcasterNotAllowed)
        );
        let allowed = vec!["1337".to_owned()];
        assert_eq!(
            plan_live_post(
                &s,
                "http://not-https.example",
                &allowed,
                &mut recent,
                NOW_MS
            ),
            Err(StreamFunnelError::InvalidInviteUrl)
        );
        // Neither refusal burned the claim.
        assert!(matches!(
            plan_live_post(&s, INVITE, &allowed, &mut recent, NOW_MS).unwrap(),
            NotificationPlan::Post(_)
        ));
    }

    #[test]
    fn recent_keys_expire_and_stay_bounded() {
        let mut recent = RecentKeys::new();
        assert!(recent.claim("a", 0));
        assert!(!recent.claim("a", RECENT_TTL_MS));
        assert!(recent.claim("a", RECENT_TTL_MS + 1));
        let mut recent = RecentKeys::new();
        for i in 0..RECENT_CAPACITY + 10 {
            assert!(recent.claim(&format!("k{i}"), 0));
        }
        assert_eq!(recent.keys.len(), RECENT_CAPACITY);
        assert_eq!(recent.order.len(), RECENT_CAPACITY);
        // The oldest were evicted; the newest are still deduped.
        assert!(recent.claim("k0", 0));
        assert!(!recent.claim(&format!("k{}", RECENT_CAPACITY + 9), 0));
    }

    #[test]
    fn callback_requires_plain_https_without_port_query_or_userinfo() {
        assert!(CallbackUrl::new("https://bot.example.com/twitch/eventsub").is_ok());
        assert!(CallbackUrl::new("  https://bot.example.com/twitch/eventsub ").is_ok());
        for bad in [
            "http://bot.example.com/x",
            "https://bot.example.com:8443/x",
            "https://bot.example.com:443/x",
            "https://user:pw@bot.example.com/x",
            "https://bot.example.com/x?token=1",
            "https://bot.example.com/x#frag",
            "https://BOT.example.com/x",
            "https://bot.example.com/a b",
            "not a url",
            "",
        ] {
            assert_eq!(
                CallbackUrl::new(bad),
                Err(EventSubError::InvalidCallback),
                "{bad}"
            );
        }
        let long = format!("https://bot.example.com/{}", "a".repeat(CALLBACK_MAX));
        assert!(CallbackUrl::new(&long).is_err());
    }

    #[test]
    fn create_body_matches_helix_shape_and_stays_secret() {
        let callback = CallbackUrl::new("https://bot.example.com/twitch/eventsub").unwrap();
        let signing_secret = test_secret();
        let body = create_subscription_body("1337", &callback, &signing_secret).unwrap();
        assert_eq!(format!("{body:?}"), "[REDACTED]");
        let value: serde_json::Value = serde_json::from_str(body.expose()).unwrap();
        assert_eq!(
            value,
            json!({
                "type": "stream.online",
                "version": "1",
                "condition": {"broadcaster_user_id": "1337"},
                "transport": {
                    "method": "webhook",
                    "callback": "https://bot.example.com/twitch/eventsub",
                    "secret": signing_secret.expose(),
                },
            })
        );
        for bad in ["", "abc", "13 37", "../x"] {
            assert_eq!(
                create_subscription_body(bad, &callback, &test_secret()).unwrap_err(),
                EventSubError::InvalidBroadcasterId
            );
        }
    }

    #[test]
    fn delete_url_only_accepts_safe_ids() {
        assert_eq!(
            delete_subscription_url("abc-123_X").unwrap(),
            format!("{HELIX_SUBSCRIPTIONS_URL}?id=abc-123_X")
        );
        for bad in ["", "a&b=1", "a b", "x/y", "é"] {
            assert!(delete_subscription_url(bad).is_err(), "{bad}");
        }
    }

    const CB: &str = "https://bot.example.com/twitch/eventsub";

    fn listing() -> serde_json::Value {
        json!({"data": [
            {"id": "s1", "status": "enabled", "type": "stream.online", "version": "1",
             "condition": {"broadcaster_user_id": "1"},
             "transport": {"method": "webhook", "callback": CB}},
            {"id": "s2", "status": "webhook_callback_verification_failed", "type": "stream.online", "version": "1",
             "condition": {"broadcaster_user_id": "2"},
             "transport": {"method": "webhook", "callback": CB}},
            {"id": "s3", "status": "enabled", "type": "stream.offline", "version": "1",
             "condition": {"broadcaster_user_id": "3"},
             "transport": {"method": "webhook", "callback": CB}},
            {"id": "s4", "status": "enabled", "type": "stream.online", "version": "1",
             "condition": {"broadcaster_user_id": "4"},
             "transport": {"method": "websocket", "session_id": "x"}},
            {"id": "s5", "status": "enabled", "type": "stream.online", "version": "1",
             "condition": {"broadcaster_user_id": "5"},
             "transport": {"method": "webhook", "callback": "https://other.example.com/hook"}},
            {"id": "bad id", "status": "enabled", "type": "stream.online", "version": "1",
             "condition": {"broadcaster_user_id": "6"},
             "transport": {"method": "webhook", "callback": CB}},
            {"garbage": true},
        ]})
    }

    #[test]
    fn parse_keeps_only_stream_online_v1_webhooks() {
        let ids: Vec<String> = parse_subscriptions(&listing())
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(ids, ["s1", "s2", "s5"]);
        assert!(parse_subscriptions(&json!({})).is_empty());
        assert!(parse_subscriptions(&json!({"data": "nope"})).is_empty());
    }

    #[test]
    fn reconcile_creates_missing_replaces_failed_and_ignores_foreign() {
        let callback = CallbackUrl::new(CB).unwrap();
        let existing = parse_subscriptions(&listing());
        let desired = vec!["1".to_owned(), "2".to_owned(), "7".to_owned()];
        assert_eq!(
            reconcile(&desired, &callback, &existing).unwrap(),
            vec![
                SubscriptionAction::Delete {
                    id: "s2".to_owned()
                },
                SubscriptionAction::Create {
                    broadcaster_user_id: "2".to_owned()
                },
                SubscriptionAction::Create {
                    broadcaster_user_id: "7".to_owned()
                },
            ]
        );
        // Steady state: nothing to do. The foreign-callback s5 is never touched.
        let steady = vec!["1".to_owned()];
        let healthy_only: Vec<_> = existing.into_iter().filter(|s| s.id != "s2").collect();
        assert_eq!(
            reconcile(&steady, &callback, &healthy_only).unwrap(),
            vec![]
        );
    }

    #[test]
    fn reconcile_drops_undesired_and_duplicate_subscriptions() {
        let callback = CallbackUrl::new(CB).unwrap();
        let sub = |id: &str, status: &str, b: &str| ExistingSubscription {
            id: id.to_owned(),
            status: status.to_owned(),
            broadcaster_user_id: b.to_owned(),
            callback: CB.to_owned(),
        };
        let existing = vec![
            sub("a", "enabled", "1"),
            sub("b", "enabled", "1"),
            sub("c", "enabled", "9"),
            sub("d", "notification_failures_exceeded", "1"),
        ];
        assert_eq!(
            reconcile(&["1".to_owned(), "1".to_owned()], &callback, &existing).unwrap(),
            vec![
                SubscriptionAction::Delete { id: "b".to_owned() },
                SubscriptionAction::Delete { id: "c".to_owned() },
                SubscriptionAction::Delete { id: "d".to_owned() },
            ]
        );
        assert_eq!(
            reconcile(&["x".to_owned()], &callback, &[]),
            Err(EventSubError::InvalidBroadcasterId)
        );
        let many: Vec<String> = (0..=MAX_BROADCASTERS).map(|i| i.to_string()).collect();
        assert_eq!(
            reconcile(&many, &callback, &[]),
            Err(EventSubError::TooManyBroadcasters)
        );
    }
}
