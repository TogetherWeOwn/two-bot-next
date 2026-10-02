//! Bounded, scalar rejection telemetry for `POST /internal/actions`
//! (threat model F4). Pure core: the receiver feeds it one [`Rejection`] per
//! refused request and logs whatever [`RejectionRecord`]s come back. Wiring
//! into the route lives with the receiver slice (TOG-10603).
//!
//! Two properties are load-bearing:
//!
//! - **Scalar records.** A record carries a closed class, a key label, an
//!   action label and counts. The key label is a configured key id or a fixed
//!   word; the action label is a `&'static str` taken from
//!   [`IMPLEMENTED_ACTIONS`], never the caller's bytes. Records hold no body,
//!   header value other than a configured key id, error message, `log_reason`,
//!   SQL or path: the labels have nowhere to put them.
//! - **Bounded output and memory.** Suppression runs per
//!   `(class, key, action)` bucket inside a tumbling window. At most
//!   `max_tracked` buckets are tracked; overflow folds into one `other` bucket
//!   per class. Each bucket emits at most `samples_per_window` samples plus
//!   one summary when the window closes, so a window emits at most
//!   [`RejectionTelemetry::max_records_per_window`] records however large the
//!   flood. See `docs/rejection-telemetry.md` for the bound across windows.

use std::collections::BTreeMap;
use std::fmt;

use serde_json::Value;

use crate::internal_actions::{ErrorCode, IMPLEMENTED_ACTIONS, MAX_BODY_BYTES};

/// Default window: one minute.
pub const DEFAULT_WINDOW_MS: u64 = 60_000;
/// Default samples per bucket per window: the first rejection logs, the rest
/// are counted into the summary.
pub const DEFAULT_SAMPLES_PER_WINDOW: u32 = 1;
/// Default cap on tracked `(class, key, action)` buckets per window.
pub const DEFAULT_MAX_TRACKED: usize = 32;
/// Hard ceiling on `max_tracked`, whatever the configuration says.
pub const MAX_TRACKED_CEILING: usize = 1024;
/// Hard ceiling on `samples_per_window`.
pub const MAX_SAMPLES_CEILING: u32 = 100;
/// Longest key id that can appear in a record.
pub const MAX_KEY_ID_LEN: usize = 64;

/// Why a request was refused. Closed: [`RejectionClass::classify`] matches
/// every [`ErrorCode`] without a wildcard, so a new refusal code does not
/// compile until it has a class here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RejectionClass {
    /// MAC or auth failure for a configured key id: bad signature, missing
    /// auth header, malformed nonce.
    AuthFailure,
    /// Auth failure for a key id the ring does not hold. The caller sees the
    /// same refusal as [`Self::AuthFailure`]; only the log tells them apart.
    UnknownKey,
    /// Timestamp outside the skew window.
    ClockSkew,
    /// Nonce already used.
    NonceReplay,
    /// Per-key or per-action token bucket empty.
    RateLimit,
    /// Action name outside the 19-name catalog (or missing).
    UnknownAction,
    /// Catalog action that is disabled or lacks its store on this bot.
    ActionDisabled,
    /// Body too large, not JSON, not an object, missing `action`, or a field
    /// validator refused it.
    MalformedBody,
    /// Settings version conflict or an in-progress idempotent retry.
    Conflict,
    /// Discord refused, was unavailable, or timed out.
    Upstream,
    /// Server-side failure (misconfiguration, storage unavailable).
    Internal,
}

impl RejectionClass {
    /// Every class, in index order.
    pub const ALL: [Self; 11] = [
        Self::AuthFailure,
        Self::UnknownKey,
        Self::ClockSkew,
        Self::NonceReplay,
        Self::RateLimit,
        Self::UnknownAction,
        Self::ActionDisabled,
        Self::MalformedBody,
        Self::Conflict,
        Self::Upstream,
        Self::Internal,
    ];
    /// Number of classes; also the number of `other` overflow buckets.
    pub const COUNT: usize = Self::ALL.len();

    /// Classify a refusal from its wire code and the already-bounded labels.
    /// The key and action labels only split a code that covers two paths.
    #[must_use]
    pub fn classify(code: ErrorCode, key: &KeyLabel, action: ActionLabel) -> Self {
        match code {
            ErrorCode::Unauthorized => match key {
                KeyLabel::Configured(_) => Self::AuthFailure,
                KeyLabel::Unknown | KeyLabel::Invalid | KeyLabel::Other => Self::UnknownKey,
            },
            ErrorCode::StaleRequest => Self::ClockSkew,
            ErrorCode::Replayed => Self::NonceReplay,
            ErrorCode::RateLimited => Self::RateLimit,
            ErrorCode::ActionNotAllowed => match action {
                ActionLabel::Known(_) => Self::ActionDisabled,
                ActionLabel::Unknown | ActionLabel::Other => Self::UnknownAction,
            },
            ErrorCode::Malformed => Self::MalformedBody,
            ErrorCode::VersionConflict | ErrorCode::InProgress => Self::Conflict,
            ErrorCode::DiscordRejected
            | ErrorCode::DiscordUnavailable
            | ErrorCode::UpstreamTimeout => Self::Upstream,
            ErrorCode::Internal => Self::Internal,
        }
    }

    /// Stable log label.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AuthFailure => "auth_failure",
            Self::UnknownKey => "unknown_key",
            Self::ClockSkew => "clock_skew",
            Self::NonceReplay => "nonce_replay",
            Self::RateLimit => "rate_limit",
            Self::UnknownAction => "unknown_action",
            Self::ActionDisabled => "action_disabled",
            Self::MalformedBody => "malformed_body",
            Self::Conflict => "conflict",
            Self::Upstream => "upstream",
            Self::Internal => "internal",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// Is this key id safe to print? 1–64 ASCII letters, digits, `.`, `_`, `-`.
#[must_use]
pub fn valid_key_id_shape(key_id: &str) -> bool {
    !key_id.is_empty()
        && key_id.len() <= MAX_KEY_ID_LEN
        && key_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The key id as it may appear in a record.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum KeyLabel {
    /// A well-shaped id the key ring holds. Configured ids are operator
    /// config, not caller input.
    Configured(String),
    /// A well-shaped id the ring does not hold. The text is caller-chosen, so
    /// it is not printed.
    Unknown,
    /// Empty, too long, or outside the allowed characters.
    Invalid,
    /// Folded by the tracking cap.
    Other,
}

impl KeyLabel {
    /// `configured` is the receiver's `KeyRing::contains(key_id)`.
    #[must_use]
    pub fn new(key_id: &str, configured: bool) -> Self {
        if !valid_key_id_shape(key_id) {
            Self::Invalid
        } else if configured {
            Self::Configured(key_id.to_owned())
        } else {
            Self::Unknown
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Configured(id) => id,
            Self::Unknown => "unknown",
            Self::Invalid => "invalid",
            Self::Other => "other",
        }
    }
}

/// The action as it may appear in a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ActionLabel {
    /// One of [`IMPLEMENTED_ACTIONS`]; the string is the catalog constant.
    Known(&'static str),
    /// Missing, unparsed, or outside the catalog.
    Unknown,
    /// Folded by the tracking cap.
    Other,
}

impl ActionLabel {
    /// `None` when the request never got as far as an action.
    #[must_use]
    pub fn new(action: Option<&str>) -> Self {
        action
            .and_then(|a| IMPLEMENTED_ACTIONS.iter().find(|known| **known == a))
            .map_or(Self::Unknown, |known| Self::Known(known))
    }

    /// Label the `action` field of a raw body, for refusals the pipeline
    /// returns before handing back the parsed action (`action_not_allowed`,
    /// the add-member bucket). Call it only after the signature verified:
    /// parsing unauthenticated bodies would hand callers free JSON work.
    /// Bodies over [`MAX_BODY_BYTES`] are not parsed.
    #[must_use]
    pub fn from_body(raw: &[u8]) -> Self {
        if raw.len() > MAX_BODY_BYTES {
            return Self::Unknown;
        }
        match serde_json::from_slice::<Value>(raw) {
            Ok(body) => Self::new(body.get("action").and_then(Value::as_str)),
            Err(_) => Self::Unknown,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Known(name) => name,
            Self::Unknown => "unknown",
            Self::Other => "other",
        }
    }
}

/// One refused request, already reduced to labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    class: RejectionClass,
    key: KeyLabel,
    action: ActionLabel,
}

impl Rejection {
    /// `code` is the refusal's `ActionError::code`. Build `key` with
    /// [`KeyLabel::new`] from the raw `X-TWO-Key-Id` header and
    /// `KeyRing::contains`; build `action` from `AuthDecision::action` when the
    /// request got that far, else [`ActionLabel::from_body`] or `new(None)`.
    #[must_use]
    pub fn new(code: ErrorCode, key: KeyLabel, action: ActionLabel) -> Self {
        Self {
            class: RejectionClass::classify(code, &key, action),
            key,
            action,
        }
    }

    #[must_use]
    pub fn class(&self) -> RejectionClass {
        self.class
    }

    #[must_use]
    pub fn key(&self) -> &KeyLabel {
        &self.key
    }

    #[must_use]
    pub fn action(&self) -> ActionLabel {
        self.action
    }
}

/// Sample: an individual rejection inside the bucket's sample budget.
/// Summary: one per bucket that suppressed anything, when the window closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordKind {
    Sample,
    Summary,
}

impl RecordKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sample => "sample",
            Self::Summary => "summary",
        }
    }
}

/// One structured log record. Every field is a closed label or a count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectionRecord {
    pub kind: RecordKind,
    pub class: RejectionClass,
    pub key: KeyLabel,
    pub action: ActionLabel,
    /// Rejections in this bucket this window: so far (sample) or in total
    /// (summary).
    pub count: u64,
    /// Rejections in this bucket this window that were not sampled; 0 on a
    /// sample.
    pub suppressed: u64,
}

impl fmt::Display for RejectionRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "kind={} class={} key={} action={} count={} suppressed={}",
            self.kind.as_str(),
            self.class.as_str(),
            self.key.as_str(),
            self.action.as_str(),
            self.count,
            self.suppressed
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TelemetryConfigError {
    #[error("rejection telemetry window must be at least 1 ms")]
    ZeroWindow,
    #[error("rejection telemetry samples_per_window exceeds {MAX_SAMPLES_CEILING}")]
    TooManySamples,
    #[error("rejection telemetry max_tracked exceeds {MAX_TRACKED_CEILING}")]
    TooManyTracked,
}

/// Window length, per-bucket sample budget and tracked-bucket cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TelemetryConfig {
    window_ms: u64,
    samples_per_window: u32,
    max_tracked: usize,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            window_ms: DEFAULT_WINDOW_MS,
            samples_per_window: DEFAULT_SAMPLES_PER_WINDOW,
            max_tracked: DEFAULT_MAX_TRACKED,
        }
    }
}

impl TelemetryConfig {
    pub fn new(
        window_ms: u64,
        samples_per_window: u32,
        max_tracked: usize,
    ) -> Result<Self, TelemetryConfigError> {
        if window_ms == 0 {
            return Err(TelemetryConfigError::ZeroWindow);
        }
        if samples_per_window > MAX_SAMPLES_CEILING {
            return Err(TelemetryConfigError::TooManySamples);
        }
        if max_tracked > MAX_TRACKED_CEILING {
            return Err(TelemetryConfigError::TooManyTracked);
        }
        Ok(Self {
            window_ms,
            samples_per_window,
            max_tracked,
        })
    }

    #[must_use]
    pub fn window_ms(&self) -> u64 {
        self.window_ms
    }

    #[must_use]
    pub fn samples_per_window(&self) -> u32 {
        self.samples_per_window
    }

    #[must_use]
    pub fn max_tracked(&self) -> usize {
        self.max_tracked
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Bucket {
    seen: u64,
    suppressed: u64,
}

type BucketKey = (RejectionClass, KeyLabel, ActionLabel);

/// Per-window suppressor. Memory is at most `max_tracked` keyed buckets plus
/// [`RejectionClass::COUNT`] fixed overflow buckets.
#[derive(Debug, Clone)]
pub struct RejectionTelemetry {
    config: TelemetryConfig,
    window_start_ms: Option<u64>,
    tracked: BTreeMap<BucketKey, Bucket>,
    overflow: [Bucket; RejectionClass::COUNT],
}

impl Default for RejectionTelemetry {
    fn default() -> Self {
        Self::new(TelemetryConfig::default())
    }
}

impl RejectionTelemetry {
    #[must_use]
    pub fn new(config: TelemetryConfig) -> Self {
        Self {
            config,
            window_start_ms: None,
            tracked: BTreeMap::new(),
            overflow: [Bucket::default(); RejectionClass::COUNT],
        }
    }

    #[must_use]
    pub fn config(&self) -> TelemetryConfig {
        self.config
    }

    /// Most records one window can emit:
    /// `(max_tracked + RejectionClass::COUNT) * (samples_per_window + 1)`.
    #[must_use]
    pub fn max_records_per_window(&self) -> u64 {
        let buckets = (self.config.max_tracked + RejectionClass::COUNT) as u64;
        buckets * (u64::from(self.config.samples_per_window) + 1)
    }

    /// Keyed buckets held for the current window (overflow buckets excluded).
    #[must_use]
    pub fn tracked_len(&self) -> usize {
        self.tracked.len()
    }

    /// Count one rejection at `now_ms`. Returns the summaries of a window that
    /// has elapsed, then this rejection's sample if its bucket still has
    /// budget. Usually empty during a flood.
    pub fn record(&mut self, rejection: Rejection, now_ms: u64) -> Vec<RejectionRecord> {
        let mut out = self.flush(now_ms);
        if self.window_start_ms.is_none() {
            self.window_start_ms = Some(now_ms);
        }

        let Rejection { class, key, action } = rejection;
        let bucket_key = (class, key, action);
        let (bucket, key, action) = if self.tracked.contains_key(&bucket_key) {
            let bucket = self.tracked.get_mut(&bucket_key).expect("checked above");
            (bucket, bucket_key.1, bucket_key.2)
        } else if self.tracked.len() < self.config.max_tracked {
            let (key, action) = (bucket_key.1.clone(), bucket_key.2);
            (self.tracked.entry(bucket_key).or_default(), key, action)
        } else {
            (
                &mut self.overflow[class.index()],
                KeyLabel::Other,
                ActionLabel::Other,
            )
        };

        bucket.seen = bucket.seen.saturating_add(1);
        if bucket.seen <= u64::from(self.config.samples_per_window) {
            out.push(RejectionRecord {
                kind: RecordKind::Sample,
                class,
                key,
                action,
                count: bucket.seen,
                suppressed: 0,
            });
        } else {
            bucket.suppressed = bucket.suppressed.saturating_add(1);
        }
        out
    }

    /// Close the window if it has elapsed at `now_ms`; call from a periodic
    /// tick so a flood that stops still reports its summary.
    pub fn flush(&mut self, now_ms: u64) -> Vec<RejectionRecord> {
        match self.window_start_ms {
            Some(start) if now_ms.saturating_sub(start) >= self.config.window_ms => {
                self.close_window()
            }
            _ => Vec::new(),
        }
    }

    /// Close the current window now (shutdown): one summary per bucket that
    /// suppressed anything, then reset all state.
    pub fn close_window(&mut self) -> Vec<RejectionRecord> {
        let mut out = Vec::new();
        for ((class, key, action), bucket) in std::mem::take(&mut self.tracked) {
            push_summary(&mut out, class, key, action, bucket);
        }
        for class in RejectionClass::ALL {
            let bucket = std::mem::take(&mut self.overflow[class.index()]);
            push_summary(&mut out, class, KeyLabel::Other, ActionLabel::Other, bucket);
        }
        self.window_start_ms = None;
        out
    }
}

fn push_summary(
    out: &mut Vec<RejectionRecord>,
    class: RejectionClass,
    key: KeyLabel,
    action: ActionLabel,
    bucket: Bucket,
) {
    if bucket.suppressed > 0 {
        out.push(RejectionRecord {
            kind: RecordKind::Summary,
            class,
            key,
            action,
            count: bucket.seen,
            suppressed: bucket.suppressed,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_indexes_match_all_order_and_labels_are_unique() {
        for (i, class) in RejectionClass::ALL.into_iter().enumerate() {
            assert_eq!(class.index(), i);
        }
        let mut labels: Vec<_> = RejectionClass::ALL.iter().map(|c| c.as_str()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), RejectionClass::COUNT);
    }

    #[test]
    fn config_rejects_unbounded_values() {
        assert_eq!(
            TelemetryConfig::new(0, 1, 1),
            Err(TelemetryConfigError::ZeroWindow)
        );
        assert_eq!(
            TelemetryConfig::new(1, MAX_SAMPLES_CEILING + 1, 1),
            Err(TelemetryConfigError::TooManySamples)
        );
        assert_eq!(
            TelemetryConfig::new(1, 1, MAX_TRACKED_CEILING + 1),
            Err(TelemetryConfigError::TooManyTracked)
        );
        assert!(TelemetryConfig::new(1, MAX_SAMPLES_CEILING, MAX_TRACKED_CEILING).is_ok());
    }
}
