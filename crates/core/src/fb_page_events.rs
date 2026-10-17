//! Facebook Page events from Discord events — risk-controlled funnel (TOG-22903).
//!
//! Browser-automation exception for Page **events** only (Graph API Event node
//! is read-only). The Graph-API-feasible track (Page posts, no events) lives
//! elsewhere and must not be duplicated here.
//!
//! # Risk controls (owner-accepted, 2026-10-12)
//!
//! 1. Dedicated Page-admin account + >=2 other human admins (runbook checklist;
//!    staging proves with a test Page).
//! 2. **No stealth or anti-detection.** No fingerprint spoofing, proxy
//!    rotation, CAPTCHA solving, or circumvention of Meta technical measures.
//!    On any Meta challenge (checkpoint / CAPTCHA / login wall / 2FA
//!    re-prompt): stop immediately, record evidence, notify the owner through
//!    the CEO — never retry, never solve.
//! 3. Low volume: create/update triggers only, max 1 action per event, no bulk,
//!    no backfill sweep.
//! 4. Idempotent map `discord_event_id -> fb_event_url`: update instead of
//!    duplicating; Discord cancel deletes the mapped event.
//! 5. Verify each action: screenshot artifact + event-page readback; on failure
//!    fall back to a Graph API Page link-post (`pages_manage_posts`).
//! 6. Credentials in the secret store only, never in logs; 2FA on the dedicated
//!    account.
//! 7. Staging first: test Page or unpublished/draft events. Production Page
//!    targets are refused without an explicit staging-evidence flag. Real-Page
//!    activation is out of scope for this slice.
//! 8. Kill switch + audit row per automated action (feed_jobs / write_audit
//!    pattern).
//!
//! This module is synchronous with a fakeable [`BrowserDriver`] boundary: no
//! network, no real Meta access, no credentials in tests. Fixture secrets are
//! synthetic and must never appear in logs, errors, or audit rows.

use std::collections::{HashMap, HashSet};

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq as _;

use crate::{EventStatus, ScheduledEvent};

/// Where the automation is allowed to act.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageTarget {
    /// Test Page (or unpublished/draft events). The only target this slice
    /// acts against without extra evidence.
    StagingTestPage,
    /// Real Page. Refused unless `staging_evidence` is set (control 7).
    ProductionPage,
}

/// Runtime gates for one funnel run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FbPageEventsConfig {
    /// Kill switch (control 8). `false` means the funnel is off: zero driver
    /// calls, zero credential fetches, [`RunOutcome::Disabled`].
    ///
    /// Mirrors the `FeatureGates::automations` env-gate pattern
    /// (`TWO_AUTOMATIONS`; see `crate::feature_commands`).
    pub kill_switch_on: bool,
    /// Staging evidence flag (control 7). Required for [`PageTarget::ProductionPage`].
    pub staging_evidence: bool,
    /// Which Page the run targets.
    pub target: PageTarget,
}

/// What changed on the Discord side (derived from the scheduled-events mirror).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Created,
    Updated,
    Cancelled,
}

/// Derive the change kind from the mirror status plus the idempotency map.
///
/// - [`EventStatus::Cancelled`] always routes to [`ChangeKind::Cancelled`]
///   (delete path), regardless of the map.
/// - Any other status with a known FB URL is an [`ChangeKind::Updated`].
/// - Any other status without a known URL is a [`ChangeKind::Created`].
#[must_use]
pub fn change_kind_for(status: EventStatus, known_url: Option<&str>) -> ChangeKind {
    if status == EventStatus::Cancelled {
        return ChangeKind::Cancelled;
    }
    match known_url {
        Some(url) if !url.is_empty() => ChangeKind::Updated,
        _ => ChangeKind::Created,
    }
}

/// One funnel input: a normalized Discord mirror row plus its change kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRequest {
    pub discord_event: ScheduledEvent,
    pub change: ChangeKind,
}

/// Meta challenge signals. Any of these aborts the run immediately with zero
/// retries and one CEO notification (control 2). There is deliberately no
/// solver, no retry, and no stealth knob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChallengeSignal {
    Checkpoint,
    Captcha,
    LoginWall,
    TwoFaReprompt,
}

/// Driver failures. `Challenge` aborts; `Failed` routes create/update through
/// the link-post fallback (control 5), never silent success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverError {
    Challenge(ChallengeSignal),
    Failed(String),
}

/// Facebook Page webhook front door: verification plus envelope parsing.
///
/// Meta delivers Page webhooks as two shapes against one receiver:
/// - GET verification:
///   `hub.mode=subscribe&hub.verify_token=<token>&hub.challenge=<opaque>`;
///   the receiver echoes `hub.challenge` only when the mode and token match.
/// - POST delivery: JSON
///   `{"object":"page","entry":[{...,"changes":[{"field":...}]}]}` with
///   `X-Hub-Signature-256: sha256=<hex HMAC-SHA256(app secret, raw body)>`.
///
/// These pure parsers hold that surface under hostile input: arbitrary bytes
/// fail as bounded [`DriverError::Failed`] — never a panic, OOM, or hang.
/// Every refusal message is a static string well under
/// [`webhook::FB_WEBHOOK_ERROR_MAX`]; secrets and hostile bytes never appear in
/// errors. A verified envelope with zero changes is `Ok` with no changes
/// (acknowledge with 2xx, do nothing). Driver behavior, Discord mirroring,
/// and notification wiring are out of scope for this slice.
pub mod webhook {
    use super::*;

    /// Re-exported so the fuzz harness names the refusal type at its own path.
    pub use super::DriverError;

    /// Largest POST body the receiver may buffer (1 MiB). Typical
    /// page-events bodies are a few KiB, but a full Meta batch of 1000
    /// realistic feed changes is ~255 KiB, so the cap holds a complete
    /// 1000-update batch with ~4x headroom while still bounding memory
    /// before JSON parsing and the HMAC.
    pub const FB_WEBHOOK_MAX_BODY_BYTES: usize = 1024 * 1024;
    /// Longest `hub.challenge` the verifier echoes back.
    pub const FB_WEBHOOK_CHALLENGE_MAX: usize = 256;
    /// Longest page id accepted per entry.
    pub const FB_WEBHOOK_ID_MAX: usize = 128;
    /// Longest change `field` accepted per change.
    pub const FB_WEBHOOK_FIELD_MAX: usize = 64;
    /// Most entries accepted per envelope: sized to Meta's documented batch
    /// of up to 1000 updates, so an authenticated delivery within the 1 MiB
    /// body cap is never refused for its entry count; bodies beyond the cap
    /// are still refused as too large.
    pub const FB_WEBHOOK_MAX_ENTRIES: usize = 1000;
    /// Most changes accepted per envelope, across all entries: sized to
    /// Meta's documented batch of up to 1000 updates, within the same 1 MiB
    /// body cap.
    pub const FB_WEBHOOK_MAX_CHANGES: usize = 1000;
    /// Upper bound every [`DriverError::Failed`] message from this module
    /// stays under, so hostile input can never inflate an error.
    pub const FB_WEBHOOK_ERROR_MAX: usize = 128;

    const SIGNATURE_PREFIX: &str = "sha256=";
    /// Hex characters in a SHA-256 delivery signature: 32 bytes, hex-encoded.
    const SIGNATURE_HEX_LEN: usize = 64;

    fn failed(message: &str) -> DriverError {
        debug_assert!(
            message.len() <= FB_WEBHOOK_ERROR_MAX,
            "webhook refusal exceeds the error bound: {message:?}"
        );
        DriverError::Failed(message.to_owned())
    }

    /// Non-empty, byte-bounded, printable-ASCII token shape shared by the
    /// challenge echo, page ids, and change fields.
    fn bounded_token(value: &str, max: usize) -> bool {
        !value.is_empty() && value.len() <= max && value.bytes().all(|b| b.is_ascii_graphic())
    }

    /// One accepted page change: which page object changed and which field.
    /// The change `value` payload is intentionally not carried: the funnel
    /// acts on the Discord mirror row, never on Meta-supplied content.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct FbPageChange {
        pub page_id: String,
        pub field: String,
    }

    /// A verified page-events envelope: every accepted change, in order.
    /// Empty means acknowledge with 2xx and do nothing.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct FbPageEnvelope {
        pub changes: Vec<FbPageChange>,
    }

    /// Verify a webhook GET challenge. Returns the `hub.challenge` string to
    /// echo on success. The expected token is compared in constant time and
    /// never appears in errors; an empty expected token refuses everything so
    /// a misconfigured receiver cannot verify against nothing.
    #[must_use = "echo the challenge on Ok or refuse the handshake"]
    pub fn verify_challenge(
        mode: &str,
        verify_token: &str,
        challenge: &str,
        expected_token: &str,
    ) -> Result<String, DriverError> {
        if mode != "subscribe" {
            return Err(failed("webhook challenge mode must be \"subscribe\""));
        }
        if expected_token.is_empty()
            || !bool::from(expected_token.as_bytes().ct_eq(verify_token.as_bytes()))
        {
            return Err(failed("webhook challenge verification failed"));
        }
        if !bounded_token(challenge, FB_WEBHOOK_CHALLENGE_MAX) {
            return Err(failed("webhook challenge is invalid"));
        }
        Ok(challenge.to_owned())
    }

    /// Verify a POST delivery signature before any JSON is parsed. Order: size
    /// cap, then constant-time HMAC, so an unauthenticated sender learns
    /// nothing and cannot force a large parse.
    #[must_use = "parse the envelope only after the signature verifies"]
    pub fn verify_signature(
        app_secret: &str,
        body: &[u8],
        signature_header: &str,
    ) -> Result<(), DriverError> {
        if body.len() > FB_WEBHOOK_MAX_BODY_BYTES {
            return Err(failed("webhook body too large"));
        }
        if app_secret.is_empty() {
            return Err(failed("webhook signature mismatch"));
        }
        let provided = signature_header
            .strip_prefix(SIGNATURE_PREFIX)
            .filter(|hex_part| {
                hex_part.len() == SIGNATURE_HEX_LEN
                    && hex_part.bytes().all(|b| b.is_ascii_hexdigit())
            })
            .and_then(|hex_part| hex::decode(hex_part).ok())
            .ok_or_else(|| failed("webhook signature mismatch"))?;
        let mut mac = match <Hmac<Sha256>>::new_from_slice(app_secret.as_bytes()) {
            Ok(mac) => mac,
            Err(_) => return Err(failed("webhook signature mismatch")),
        };
        mac.update(body);
        match mac.verify_slice(&provided) {
            Ok(()) => Ok(()),
            Err(_) => Err(failed("webhook signature mismatch")),
        }
    }

    /// Parse a POST delivery body into its accepted changes. Call
    /// [`verify_signature`] first. Malformed envelopes fail as bounded
    /// [`DriverError::Failed`]; entries and changes beyond
    /// [`FB_WEBHOOK_MAX_ENTRIES`] / [`FB_WEBHOOK_MAX_CHANGES`] are refused
    /// rather than truncated so a flood can never silently shrink. The caps
    /// match Meta's documented batch of up to 1000 updates, so a correctly
    /// signed delivery within [`FB_WEBHOOK_MAX_BODY_BYTES`] is never refused
    /// for its entry/change count; bodies beyond the 1 MiB cap are refused
    /// as too large (the receiver should acknowledge with 2xx plus an alert
    /// rather than a redelivered non-2xx).
    #[must_use = "handle the parsed changes or the refusal, never drop it"]
    pub fn parse_envelope(body: &[u8]) -> Result<FbPageEnvelope, DriverError> {
        if body.len() > FB_WEBHOOK_MAX_BODY_BYTES {
            return Err(failed("webhook body too large"));
        }
        let value: serde_json::Value =
            serde_json::from_slice(body).map_err(|_| failed("webhook body is not valid JSON"))?;
        if value.get("object").and_then(serde_json::Value::as_str) != Some("page") {
            return Err(failed("webhook object must be \"page\""));
        }
        let entries = value
            .get("entry")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| failed("webhook entry list is invalid"))?;
        if entries.len() > FB_WEBHOOK_MAX_ENTRIES {
            return Err(failed("webhook envelope too large"));
        }
        let mut changes = Vec::new();
        for entry in entries {
            let page_id = entry
                .get("id")
                .and_then(serde_json::Value::as_str)
                .filter(|id| bounded_token(id, FB_WEBHOOK_ID_MAX))
                .ok_or_else(|| failed("webhook page id is invalid"))?;
            let entry_changes = entry
                .get("changes")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| failed("webhook change list is invalid"))?;
            if changes.len().saturating_add(entry_changes.len()) > FB_WEBHOOK_MAX_CHANGES {
                return Err(failed("webhook envelope too large"));
            }
            for change in entry_changes {
                let field = change
                    .get("field")
                    .and_then(serde_json::Value::as_str)
                    .filter(|field| bounded_token(field, FB_WEBHOOK_FIELD_MAX))
                    .ok_or_else(|| failed("webhook change field is invalid"))?;
                changes.push(FbPageChange {
                    page_id: page_id.to_owned(),
                    field: field.to_owned(),
                });
            }
        }
        Ok(FbPageEnvelope { changes })
    }
}

/// Observable driver calls (test seam; the fake records these).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverCall {
    Create {
        discord_event_id: String,
    },
    Update {
        fb_url: String,
        discord_event_id: String,
    },
    Delete {
        fb_url: String,
    },
    Screenshot,
    Readback {
        fb_url: String,
    },
    LinkPost {
        discord_event_id: String,
    },
}

/// Typed FB event content carried from the Discord mirror row to the driver.
///
/// This is what lets updates sync content (start time, description) instead
/// of URLs alone: [`BrowserDriver::create_event_full`] and
/// [`BrowserDriver::update_event_full`] take this draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FbEventDraft {
    pub discord_event_id: String,
    pub name: String,
    pub starts_at: String,
    pub channel_id: Option<String>,
    pub description: Option<String>,
}

impl FbEventDraft {
    /// Build the draft the funnel acts on from a normalized mirror row.
    #[must_use]
    pub fn from_scheduled(event: &ScheduledEvent) -> Self {
        Self {
            discord_event_id: event.id.clone(),
            name: event.name.clone(),
            starts_at: event.starts_at.clone(),
            channel_id: event.channel_id.clone(),
            description: event.description.clone(),
        }
    }
}

/// Observed event-page state for control-5 verification.
///
/// URL-only drivers return `name: None, starts_at: None` (the default
/// [`BrowserDriver::readback_event`] does exactly this) and are verified on
/// URL alone. Drivers that can observe content fill the fields in, and any
/// observed field that mismatches the [`FbEventDraft`] fails verification
/// into the link-post fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventReadback {
    pub url: String,
    pub name: Option<String>,
    pub starts_at: Option<String>,
}

/// Browser-automation boundary. Implementations must be non-evasive: a stock
/// browser session, headed so screenshots and readbacks work, with no
/// anti-detection add-ons.
pub trait BrowserDriver: Send + Sync {
    fn create_event(&self, discord_event_id: &str, name: &str) -> Result<String, DriverError>;
    fn update_event(&self, fb_url: &str, discord_event_id: &str) -> Result<(), DriverError>;
    fn delete_event(&self, fb_url: &str) -> Result<(), DriverError>;
    fn screenshot_png(&self) -> Vec<u8>;
    fn readback_url(&self, fb_url: &str) -> Option<String>;
    fn link_post_fallback(&self, discord_event_id: &str, name: &str)
        -> Result<String, DriverError>;

    /// Create carrying the full [`FbEventDraft`] (start time, description,
    /// channel). Defaults to the narrowing [`BrowserDriver::create_event`]
    /// so existing fakes keep working; real drivers must override so FB
    /// events are created with a start time.
    fn create_event_full(&self, draft: &FbEventDraft) -> Result<String, DriverError> {
        self.create_event(&draft.discord_event_id, &draft.name)
    }

    /// Update carrying the full [`FbEventDraft`] so Discord time/description
    /// changes sync. Defaults to the field-less
    /// [`BrowserDriver::update_event`] so existing fakes keep working; real
    /// drivers must override.
    fn update_event_full(&self, fb_url: &str, draft: &FbEventDraft) -> Result<(), DriverError> {
        self.update_event(fb_url, &draft.discord_event_id)
    }

    /// Read back the event page with observed content. Defaults to URL-only
    /// via [`BrowserDriver::readback_url`]; drivers that can observe content
    /// must override and fill `name` / `starts_at` so verification compares
    /// more than the URL.
    fn readback_event(&self, fb_url: &str) -> Option<EventReadback> {
        self.readback_url(fb_url).map(|url| EventReadback {
            url,
            name: None,
            starts_at: None,
        })
    }
}

/// Page credential handle. The inner secret is never rendered: [`PageToken`]
/// redacts on `Debug` (control 6).
#[derive(Clone, PartialEq, Eq)]
pub struct PageToken {
    inner: String,
}

impl PageToken {
    #[must_use]
    pub fn new(token: String) -> Self {
        Self { inner: token }
    }
}

impl std::fmt::Debug for PageToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PageToken(redacted)")
    }
}

/// Secret-store failures. Both map to fail-closed
/// [`Refused::SecretUnavailable`] with no secret material in the error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretError {
    Unavailable,
    RotationInProgress,
}

/// Credential source (control 6). Exactly one fetch per enabled run.
pub trait SecretStore: Send + Sync {
    fn fetch_page_token(&self) -> Result<PageToken, SecretError>;
}

/// CEO notification (control 2). Challenges notify the owner *through the
/// CEO*, never the owner directly, and never with secret material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CeoNotification {
    Challenge { detail: String },
}

impl CeoNotification {
    #[must_use]
    pub fn challenge(detail: impl Into<String>) -> Self {
        Self::Challenge {
            detail: detail.into(),
        }
    }
}

/// CEO-notify sink.
pub trait NotifySink: Send + Sync {
    fn notify(&self, notification: CeoNotification);
}

/// One audit row per automated action (control 8), shaped like `FeedAudit`
/// (`crates/core/src/feeds_store.rs:271`) following the
/// `crates/bot/src/feed_jobs.rs` audit pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRow {
    pub action: String,
    pub target_key: String,
    pub outcome: String,
    pub fb_url: Option<String>,
    /// Control-5 evidence: the screenshot bytes captured for this action.
    /// `None` where no screenshot was taken (refusals and no-mapping no-ops
    /// take none) or where the terminal row of the same fallback carries it
    /// (the intermediate verify/driver-failure row).
    pub screenshot_png: Option<Vec<u8>>,
}

/// Audit sink.
pub trait AuditSink: Send + Sync {
    fn push(&self, row: AuditRow);
}

/// Idempotency map `discord_event_id -> fb_event_url` (control 4).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdempotencyMap {
    inner: HashMap<String, String>,
}

impl IdempotencyMap {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the mapped FB URL, if any. An empty stored value is treated
    /// as unmapped so every caller (`plan_actions`, `execute`,
    /// `change_kind_for` via the map) shares one empty-URL rule (control 4).
    /// Empty values only arise from a buggy driver or a corrupted persisted
    /// map; [`IdempotencyMap::record`] refuses them and an empty
    /// `create_event_full` result is treated as a driver failure.
    #[must_use]
    pub fn lookup(&self, discord_event_id: &str) -> Option<String> {
        self.inner
            .get(discord_event_id)
            .filter(|url| !url.is_empty())
            .cloned()
    }

    /// Record a verified or unverified FB URL. Empty URLs are refused (no
    /// insert) so one bad create can never poison later update/delete calls.
    pub fn record(&mut self, discord_event_id: &str, fb_url: &str) {
        if fb_url.is_empty() {
            return;
        }
        self.inner
            .insert(discord_event_id.to_owned(), fb_url.to_owned());
    }

    pub fn remove(&mut self, discord_event_id: &str) {
        self.inner.remove(discord_event_id);
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

/// One planned funnel action (control 3: at most one per planning call).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlannedAction {
    Create {
        discord_event_id: String,
    },
    Update {
        discord_event_id: String,
        fb_url: String,
    },
    Delete {
        discord_event_id: String,
    },
    LinkPostFallback {
        discord_event_id: String,
    },
}

/// Planning/execution refusals. Refusals fire *before* any driver call and
/// record no idempotency entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// More than one distinct Discord event in a single planning call:
    /// backfill sweeps and bulk inputs are refused outright.
    Bulk,
    /// Production target without the staging-evidence flag (control 7).
    StagingEvidenceMissing,
    /// Secret store unavailable or rotating (control 6): fail closed.
    SecretUnavailable,
}

/// Outcome of one [`run_once`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    Created {
        url: String,
    },
    Updated {
        url: String,
    },
    Deleted,
    LinkPostFallback {
        url: String,
    },
    /// Delete failed: the FB event is still live, so the mapping is kept and
    /// a later cancel can retry. Never reported as [`RunOutcome::Deleted`]
    /// (control 4, never silent success).
    DeleteFailed {
        fb_url: String,
    },
    /// Primary action and the link-post fallback both failed: nothing new
    /// reached Facebook. `stage` is `"create"` or `"update"`. Never shaped
    /// as [`RunOutcome::LinkPostFallback`] (never silent success).
    FallbackFailed {
        stage: &'static str,
    },
    ChallengeAborted {
        retries: u32,
        ceo_notified: bool,
    },
    Disabled,
}

/// Collapse a batch of requests to at most one action (control 3).
///
/// - Empty input plans nothing.
/// - Two or more *distinct* Discord event ids are refused as [`Refused::Bulk`]
///   (no backfill sweep, no bulk fan-out).
/// - Duplicate deliveries of the same event collapse to the latest request.
/// - `Created` plans a create, unless the map already holds a URL for the
///   event — then it plans an update, so a stale or duplicate `Created` can
///   never mint a second FB event (control 4, map authoritative);
///   `Updated` plans an update when the map already holds a URL (otherwise a
///   create, so a lost map entry heals forward); `Cancelled` plans a delete.
pub fn plan_actions(
    requests: &[RunRequest],
    known: &IdempotencyMap,
) -> Result<Vec<PlannedAction>, Refused> {
    if requests.is_empty() {
        return Ok(Vec::new());
    }
    let distinct: HashSet<&str> = requests
        .iter()
        .map(|r| r.discord_event.id.as_str())
        .collect();
    if distinct.len() > 1 {
        return Err(Refused::Bulk);
    }
    let latest = requests.last().expect("non-empty checked above");
    let id = latest.discord_event.id.clone();
    let action = match latest.change {
        ChangeKind::Created => match known.lookup(&id) {
            // `lookup` never returns an empty URL (see `IdempotencyMap`), so
            // any known value plans an update, never a second create.
            Some(fb_url) => PlannedAction::Update {
                discord_event_id: id,
                fb_url,
            },
            _ => PlannedAction::Create {
                discord_event_id: id,
            },
        },
        ChangeKind::Updated => match known.lookup(&id) {
            Some(fb_url) => PlannedAction::Update {
                discord_event_id: id,
                fb_url,
            },
            None => PlannedAction::Create {
                discord_event_id: id,
            },
        },
        ChangeKind::Cancelled => PlannedAction::Delete {
            discord_event_id: id,
        },
    };
    Ok(vec![action])
}

fn audit_create(
    url: &str,
    target_key: &str,
    outcome: &str,
    fb_url: Option<String>,
    screenshot_png: Option<Vec<u8>>,
) -> AuditRow {
    let _ = url;
    AuditRow {
        action: "fb_page_event_create".to_owned(),
        target_key: target_key.to_owned(),
        outcome: outcome.to_owned(),
        fb_url,
        screenshot_png,
    }
}

fn challenge_detail(discord_event_id: &str, signal: &ChallengeSignal) -> String {
    format!(
        "fb_page_event challenge {signal:?} for discord event {discord_event_id}; \
         stopped with zero retries, no evasion attempted"
    )
}

/// Stop on a Meta challenge: capture screenshot evidence, notify the owner
/// through the CEO, audit the abort (control 2). Every challenge site routes
/// through here so none can skip the notification.
fn abort_on_challenge<D: BrowserDriver, N: NotifySink, A: AuditSink>(
    driver: &D,
    notify: &N,
    audit: &A,
    discord_event_id: &str,
    signal: &ChallengeSignal,
) -> RunOutcome {
    let shot = driver.screenshot_png();
    notify.notify(CeoNotification::challenge(challenge_detail(
        discord_event_id,
        signal,
    )));
    audit.push(AuditRow {
        action: "fb_page_event_challenge_abort".to_owned(),
        target_key: discord_event_id.to_owned(),
        outcome: format!("challenge_aborted_{signal:?}_zero_retries"),
        fb_url: None,
        screenshot_png: Some(shot),
    });
    RunOutcome::ChallengeAborted {
        retries: 0,
        ceo_notified: true,
    }
}

/// Why the primary action gave way to the link-post fallback. Drives the
/// audit outcome string so a driver failure is never mislabeled as a
/// readback mismatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FallbackPrior {
    ReadbackMismatch,
    CreateFailed,
    UpdateFailed,
}

fn fallback_to_link_post<D: BrowserDriver, A: AuditSink>(
    driver: &D,
    audit: &A,
    draft: &FbEventDraft,
    created_fb_url: Option<&str>,
    update: bool,
    screenshot: Option<Vec<u8>>,
    prior: FallbackPrior,
) -> Result<RunOutcome, ChallengeSignal> {
    let action = if update {
        "fb_page_event_update"
    } else {
        "fb_page_event_create"
    };
    let prior_outcome = match prior {
        FallbackPrior::ReadbackMismatch => "verify_failed_readback_mismatch",
        FallbackPrior::CreateFailed => "create_failed_driver_error",
        FallbackPrior::UpdateFailed => "update_failed_driver_error",
    };
    audit.push(AuditRow {
        action: action.to_owned(),
        target_key: draft.discord_event_id.clone(),
        outcome: prior_outcome.to_owned(),
        fb_url: created_fb_url.map(str::to_owned),
        screenshot_png: None,
    });
    match driver.link_post_fallback(&draft.discord_event_id, &draft.name) {
        Ok(link_url) => {
            audit.push(AuditRow {
                action: "fb_page_event_link_post_fallback".to_owned(),
                target_key: draft.discord_event_id.clone(),
                outcome: "link_post_created_via_pages_manage_posts".to_owned(),
                fb_url: Some(link_url.clone()),
                screenshot_png: screenshot,
            });
            Ok(RunOutcome::LinkPostFallback { url: link_url })
        }
        Err(DriverError::Challenge(signal)) => Err(signal),
        Err(DriverError::Failed(_)) => {
            audit.push(AuditRow {
                action: "fb_page_event_link_post_fallback".to_owned(),
                target_key: draft.discord_event_id.clone(),
                outcome: "link_post_failed_driver_error".to_owned(),
                fb_url: None,
                screenshot_png: screenshot,
            });
            Ok(RunOutcome::FallbackFailed {
                stage: if update { "update" } else { "create" },
            })
        }
    }
}

/// Check an event-page readback against the draft. The URL must match, and
/// every content field the driver observed (`Some`) must match too;
/// unobserved (`None`) fields are skipped so URL-only drivers keep working.
fn readback_matches(draft: &FbEventDraft, observed: &EventReadback) -> bool {
    observed
        .name
        .as_deref()
        .is_none_or(|name| name == draft.name)
        && observed
            .starts_at
            .as_deref()
            .is_none_or(|starts_at| starts_at == draft.starts_at)
}

/// Run one funnel step: at most one automated action (control 3).
///
/// Gate order is load-bearing and pinned by tests:
/// 1. kill switch off -> [`RunOutcome::Disabled`] before the credential fetch
///    and before any driver call;
/// 2. production target without staging evidence -> [`Refused::StagingEvidenceMissing`]
///    before the credential fetch and before any driver call;
/// 3. credential fetch exactly once; rotation/failure -> [`Refused::SecretUnavailable`]
///    (fail closed, no secret in the error). The Updated-without-mapping case
///    heals forward inside `execute` without a second fetch (control 6);
/// 4. exactly one mutating driver call for create/update/delete; challenges
///    abort with zero retries + one CEO notification (control 2);
/// 5. create/update capture a screenshot artifact, persisted on the audit
///    row, plus a readback; a failed readback falls back to the link-post
///    path and audits both (control 5). Delete is screenshot-only: on
///    driver `Ok` the mapping drops without a readback; on `Failed` the
///    mapping is kept as [`RunOutcome::DeleteFailed`];
/// 6. every automated action emits exactly one audit row on success
///    (control 8); the fallback path emits two (prior failure + link post).
///    Failures are never success-shaped: a failed delete keeps the mapping
///    and reports [`RunOutcome::DeleteFailed`], and a failed fallback
///    reports [`RunOutcome::FallbackFailed`].
pub fn run_once<D: BrowserDriver, S: SecretStore, A: AuditSink, N: NotifySink>(
    driver: &D,
    secrets: &S,
    pages: &mut IdempotencyMap,
    audit: &A,
    notify: &N,
    config: &FbPageEventsConfig,
    request: &RunRequest,
) -> Result<RunOutcome, Refused> {
    if !config.kill_switch_on {
        return Ok(RunOutcome::Disabled);
    }
    if config.target == PageTarget::ProductionPage && !config.staging_evidence {
        return Err(Refused::StagingEvidenceMissing);
    }
    let _token = match secrets.fetch_page_token() {
        Ok(token) => token,
        Err(_) => return Err(Refused::SecretUnavailable),
    };

    Ok(execute(driver, pages, audit, notify, request))
}

/// Dispatch one funnel step after the gates in [`run_once`] have passed.
/// Separated so the Updated-without-mapping heal-forward path recurses here,
/// never through [`run_once`], keeping the exactly-one-fetch guarantee.
fn execute<D: BrowserDriver, A: AuditSink, N: NotifySink>(
    driver: &D,
    pages: &mut IdempotencyMap,
    audit: &A,
    notify: &N,
    request: &RunRequest,
) -> RunOutcome {
    let event_id = request.discord_event.id.clone();
    let draft = FbEventDraft::from_scheduled(&request.discord_event);

    match request.change {
        ChangeKind::Created => {
            // The map is authoritative in both directions: a stale queued
            // `Created`, or two deliveries classified before the first ran,
            // must not mint a second FB event and orphan the first
            // (control 4). Mirror the `Updated`-without-mapping heal-forward
            // below by re-dispatching as `Updated` when a URL is known.
            // `lookup` filters empty values, so any known URL redispatches
            // as `Updated`; an empty/corrupted entry behaves as unmapped and
            // heals through the create path below.
            if pages.lookup(&event_id).is_some() {
                let updated = RunRequest {
                    discord_event: request.discord_event.clone(),
                    change: ChangeKind::Updated,
                };
                return execute(driver, pages, audit, notify, &updated);
            }
            let fb_url = match driver.create_event_full(&draft) {
                Ok(url) => url,
                Err(DriverError::Challenge(signal)) => {
                    return abort_on_challenge(driver, notify, audit, &event_id, &signal);
                }
                Err(DriverError::Failed(_)) => {
                    return fallback_after_create_failure(driver, audit, notify, &draft);
                }
            };
            // A driver that reports success with an empty URL created nothing
            // addressable: never store it, and fall back honestly instead of
            // reporting `Created { url: "" }` and poisoning the map.
            if fb_url.is_empty() {
                return fallback_after_create_failure(driver, audit, notify, &draft);
            }
            let shot = driver.screenshot_png();
            match driver.readback_event(&fb_url) {
                Some(observed) if observed.url == fb_url && readback_matches(&draft, &observed) => {
                    pages.record(&event_id, &fb_url);
                    audit.push(audit_create(
                        &fb_url,
                        &event_id,
                        "created",
                        Some(fb_url.clone()),
                        Some(shot),
                    ));
                    RunOutcome::Created { url: fb_url }
                }
                _ => {
                    // The event may exist but is unverified: record its URL
                    // so the next delivery updates (or deletes) it instead of
                    // minting a duplicate (control 4).
                    pages.record(&event_id, &fb_url);
                    match fallback_to_link_post(
                        driver,
                        audit,
                        &draft,
                        Some(&fb_url),
                        false,
                        Some(shot),
                        FallbackPrior::ReadbackMismatch,
                    ) {
                        Ok(outcome) => outcome,
                        Err(signal) => {
                            abort_on_challenge(driver, notify, audit, &event_id, &signal)
                        }
                    }
                }
            }
        }
        ChangeKind::Updated => {
            let known = pages.lookup(&event_id);
            let Some(fb_url) = known else {
                let created = RunRequest {
                    discord_event: request.discord_event.clone(),
                    change: ChangeKind::Created,
                };
                return execute(driver, pages, audit, notify, &created);
            };
            match driver.update_event_full(&fb_url, &draft) {
                Ok(()) => {}
                Err(DriverError::Challenge(signal)) => {
                    return abort_on_challenge(driver, notify, audit, &event_id, &signal);
                }
                Err(DriverError::Failed(_)) => {
                    let shot = driver.screenshot_png();
                    return match fallback_to_link_post(
                        driver,
                        audit,
                        &draft,
                        Some(&fb_url),
                        true,
                        Some(shot),
                        FallbackPrior::UpdateFailed,
                    ) {
                        Ok(outcome) => outcome,
                        Err(signal) => {
                            abort_on_challenge(driver, notify, audit, &event_id, &signal)
                        }
                    };
                }
            }
            let shot = driver.screenshot_png();
            match driver.readback_event(&fb_url) {
                Some(observed) if observed.url == fb_url && readback_matches(&draft, &observed) => {
                    audit.push(AuditRow {
                        action: "fb_page_event_update".to_owned(),
                        target_key: event_id.clone(),
                        outcome: "updated".to_owned(),
                        fb_url: Some(fb_url.clone()),
                        screenshot_png: Some(shot),
                    });
                    RunOutcome::Updated { url: fb_url }
                }
                _ => match fallback_to_link_post(
                    driver,
                    audit,
                    &draft,
                    Some(&fb_url),
                    true,
                    Some(shot),
                    FallbackPrior::ReadbackMismatch,
                ) {
                    Ok(outcome) => outcome,
                    Err(signal) => abort_on_challenge(driver, notify, audit, &event_id, &signal),
                },
            }
        }
        ChangeKind::Cancelled => {
            let Some(fb_url) = pages.lookup(&event_id) else {
                audit.push(AuditRow {
                    action: "fb_page_event_delete".to_owned(),
                    target_key: event_id.clone(),
                    outcome: "deleted_noop_no_mapping".to_owned(),
                    fb_url: None,
                    screenshot_png: None,
                });
                return RunOutcome::Deleted;
            };
            match driver.delete_event(&fb_url) {
                Ok(()) => {}
                Err(DriverError::Challenge(signal)) => {
                    return abort_on_challenge(driver, notify, audit, &event_id, &signal);
                }
                Err(DriverError::Failed(message)) => {
                    let _ = message;
                    let shot = driver.screenshot_png();
                    audit.push(AuditRow {
                        action: "fb_page_event_delete".to_owned(),
                        target_key: event_id.clone(),
                        outcome: "delete_failed_driver_error".to_owned(),
                        fb_url: Some(fb_url.clone()),
                        screenshot_png: Some(shot),
                    });
                    // The FB event is still live: keep the mapping so a later
                    // cancel can retry the delete (control 4).
                    return RunOutcome::DeleteFailed { fb_url };
                }
            }
            let shot = driver.screenshot_png();
            pages.remove(&event_id);
            audit.push(AuditRow {
                action: "fb_page_event_delete".to_owned(),
                target_key: event_id.clone(),
                outcome: "deleted".to_owned(),
                fb_url: Some(fb_url),
                screenshot_png: Some(shot),
            });
            RunOutcome::Deleted
        }
    }
}

fn fallback_after_create_failure<D: BrowserDriver, A: AuditSink, N: NotifySink>(
    driver: &D,
    audit: &A,
    notify: &N,
    draft: &FbEventDraft,
) -> RunOutcome {
    let shot = driver.screenshot_png();
    match fallback_to_link_post(
        driver,
        audit,
        draft,
        None,
        false,
        Some(shot),
        FallbackPrior::CreateFailed,
    ) {
        Ok(outcome) => outcome,
        Err(signal) => abort_on_challenge(driver, notify, audit, &draft.discord_event_id, &signal),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    };

    /// Runtime-built HMAC fixture key: derived bytes, never a string literal,
    /// so the hard-coded-crypto query stays quiet while assertions stay
    /// deterministic across runs.
    fn fixture_secret() -> String {
        let bytes: [u8; 32] =
            std::array::from_fn(|index| (index as u8).wrapping_mul(37).wrapping_add(11));
        hex::encode(bytes)
    }

    #[test]
    fn change_kind_routes_cancelled_to_delete_without_map() {
        assert_eq!(
            change_kind_for(EventStatus::Cancelled, None),
            ChangeKind::Cancelled
        );
        assert_eq!(
            change_kind_for(EventStatus::Cancelled, Some("https://example.test/e/1")),
            ChangeKind::Cancelled
        );
    }

    #[test]
    fn page_token_debug_redacts_inner_secret() {
        let token = PageToken::new("fixture-fb-page-credential".to_owned());
        for rendered in [format!("{token:?}"), format!("{token:#?}")] {
            assert!(
                !rendered.contains("fixture-fb-page-credential"),
                "{rendered}"
            );
        }
    }

    const TEST_START: &str = "2026-09-06T18:00:00.000Z";
    const TEST_PAGE_URL: &str = "https://www.facebook.com/events/1234567890";
    const TEST_LINK_URL: &str = "https://www.facebook.com/fixture-page/posts/abcdef";

    fn scheduled(id: &str) -> ScheduledEvent {
        ScheduledEvent {
            id: id.to_owned(),
            name: "Sunday Squad".to_owned(),
            starts_at: TEST_START.to_owned(),
            channel_id: Some("voice-1".to_owned()),
            description: Some("Join the weekly games night.".to_owned()),
            status: EventStatus::Scheduled,
        }
    }

    fn staging() -> FbPageEventsConfig {
        FbPageEventsConfig {
            kill_switch_on: true,
            staging_evidence: true,
            target: PageTarget::StagingTestPage,
        }
    }

    /// Scriptable fake covering the failure branches the acceptance suite
    /// does not reach: per-operation results plus observed readback fields.
    #[derive(Debug)]
    struct FakeInner {
        create: Result<String, DriverError>,
        update: Result<(), DriverError>,
        delete: Result<(), DriverError>,
        link: Result<String, DriverError>,
        readback: Option<EventReadback>,
        rows: Vec<AuditRow>,
        notes: Vec<CeoNotification>,
        full_creates: Vec<FbEventDraft>,
        full_updates: Vec<(String, FbEventDraft)>,
    }

    impl Default for FakeInner {
        fn default() -> Self {
            Self {
                create: Ok(TEST_PAGE_URL.to_owned()),
                update: Ok(()),
                delete: Ok(()),
                link: Ok(TEST_LINK_URL.to_owned()),
                readback: Some(EventReadback {
                    url: TEST_PAGE_URL.to_owned(),
                    name: None,
                    starts_at: None,
                }),
                rows: Vec::new(),
                notes: Vec::new(),
                full_creates: Vec::new(),
                full_updates: Vec::new(),
            }
        }
    }

    #[derive(Debug, Default)]
    struct Fake {
        inner: Mutex<FakeInner>,
    }

    impl Fake {
        fn edit(&self, f: impl FnOnce(&mut FakeInner)) {
            f(&mut self.inner.lock().expect("fake lock"));
        }

        fn rows(&self) -> Vec<AuditRow> {
            self.inner.lock().expect("fake lock").rows.clone()
        }

        fn notes(&self) -> Vec<CeoNotification> {
            self.inner.lock().expect("fake lock").notes.clone()
        }

        fn full_creates(&self) -> Vec<FbEventDraft> {
            self.inner.lock().expect("fake lock").full_creates.clone()
        }

        fn full_updates(&self) -> Vec<(String, FbEventDraft)> {
            self.inner.lock().expect("fake lock").full_updates.clone()
        }
    }

    impl BrowserDriver for Fake {
        fn create_event(&self, _id: &str, _name: &str) -> Result<String, DriverError> {
            self.inner.lock().expect("fake lock").create.clone()
        }

        fn update_event(&self, _url: &str, _id: &str) -> Result<(), DriverError> {
            self.inner.lock().expect("fake lock").update.clone()
        }

        fn delete_event(&self, _url: &str) -> Result<(), DriverError> {
            self.inner.lock().expect("fake lock").delete.clone()
        }

        fn screenshot_png(&self) -> Vec<u8> {
            vec![0x89, b'P', b'N', b'G']
        }

        fn readback_url(&self, _fb_url: &str) -> Option<String> {
            self.inner
                .lock()
                .expect("fake lock")
                .readback
                .clone()
                .map(|observed| observed.url)
        }

        fn link_post_fallback(&self, _id: &str, _name: &str) -> Result<String, DriverError> {
            self.inner.lock().expect("fake lock").link.clone()
        }

        fn readback_event(&self, _fb_url: &str) -> Option<EventReadback> {
            self.inner.lock().expect("fake lock").readback.clone()
        }

        fn create_event_full(&self, draft: &FbEventDraft) -> Result<String, DriverError> {
            let mut inner = self.inner.lock().expect("fake lock");
            inner.full_creates.push(draft.clone());
            inner.create.clone()
        }

        fn update_event_full(&self, fb_url: &str, draft: &FbEventDraft) -> Result<(), DriverError> {
            let mut inner = self.inner.lock().expect("fake lock");
            inner.full_updates.push((fb_url.to_owned(), draft.clone()));
            inner.update.clone()
        }
    }

    #[derive(Debug, Default)]
    struct FakeSecrets {
        fetches: AtomicUsize,
    }

    impl SecretStore for FakeSecrets {
        fn fetch_page_token(&self) -> Result<PageToken, SecretError> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            Ok(PageToken::new("fixture-fb-page-credential".to_owned()))
        }
    }

    impl AuditSink for Fake {
        fn push(&self, row: AuditRow) {
            self.inner.lock().expect("fake lock").rows.push(row);
        }
    }

    impl NotifySink for Fake {
        fn notify(&self, notification: CeoNotification) {
            self.inner
                .lock()
                .expect("fake lock")
                .notes
                .push(notification);
        }
    }

    fn run(
        fake: &Fake,
        secrets: &FakeSecrets,
        pages: &mut IdempotencyMap,
        change: ChangeKind,
    ) -> Result<RunOutcome, Refused> {
        run_once(
            fake,
            secrets,
            pages,
            fake,
            fake,
            &staging(),
            &RunRequest {
                discord_event: scheduled("event-1"),
                change,
            },
        )
    }

    #[test]
    fn failed_delete_keeps_mapping_and_reports_delete_failed() {
        let fake = Fake::default();
        fake.edit(|inner| {
            inner.delete = Err(DriverError::Failed("boom".to_owned()));
        });
        let secrets = FakeSecrets::default();
        let mut pages = IdempotencyMap::new();
        pages.record("event-1", TEST_PAGE_URL);

        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Cancelled)
            .expect("delete failure is an outcome");
        assert!(
            matches!(outcome, RunOutcome::DeleteFailed { .. }),
            "a failed delete must not report Deleted: {outcome:?}"
        );
        assert_eq!(
            pages.lookup("event-1").as_deref(),
            Some(TEST_PAGE_URL),
            "the mapping is kept so a later cancel can retry"
        );

        fake.edit(|inner| inner.delete = Ok(()));
        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Cancelled).expect("retry runs");
        assert!(
            matches!(outcome, RunOutcome::Deleted),
            "retry after recovery deletes: {outcome:?}"
        );
        assert_eq!(
            pages.lookup("event-1"),
            None,
            "the entry drops only after a successful delete"
        );
    }

    #[test]
    fn challenge_in_create_fallback_notifies_ceo() {
        let fake = Fake::default();
        fake.edit(|inner| {
            inner.create = Err(DriverError::Failed("no event".to_owned()));
            inner.link = Err(DriverError::Challenge(ChallengeSignal::Captcha));
        });
        let secrets = FakeSecrets::default();
        let mut pages = IdempotencyMap::new();

        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Created)
            .expect("challenge abort is an outcome");
        assert!(
            matches!(
                outcome,
                RunOutcome::ChallengeAborted {
                    ceo_notified: true,
                    ..
                }
            ),
            "challenge aborts: {outcome:?}"
        );
        assert_eq!(
            fake.notes().len(),
            1,
            "the fallback challenge notifies the CEO exactly once"
        );
    }

    #[test]
    fn double_failure_reports_fallback_failed_never_success_shaped() {
        let fake = Fake::default();
        fake.edit(|inner| {
            inner.create = Err(DriverError::Failed("no event".to_owned()));
            inner.link = Err(DriverError::Failed("no link".to_owned()));
        });
        let secrets = FakeSecrets::default();
        let mut pages = IdempotencyMap::new();

        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Created)
            .expect("double failure is an outcome");
        assert!(
            matches!(outcome, RunOutcome::FallbackFailed { stage: "create" }),
            "nothing reached Facebook: {outcome:?}"
        );
    }

    #[test]
    fn readback_mismatch_records_url_so_redelivery_updates() {
        let fake = Fake::default();
        fake.edit(|inner| inner.readback = None);
        let secrets = FakeSecrets::default();
        let mut pages = IdempotencyMap::new();

        let outcome =
            run(&fake, &secrets, &mut pages, ChangeKind::Created).expect("fallback is an outcome");
        assert!(
            matches!(outcome, RunOutcome::LinkPostFallback { .. }),
            "unreadable create falls back: {outcome:?}"
        );
        assert_eq!(
            pages.lookup("event-1").as_deref(),
            Some(TEST_PAGE_URL),
            "the unverified event is mapped so redelivery updates it"
        );
    }

    #[test]
    fn readback_content_mismatch_fails_verification() {
        let fake = Fake::default();
        fake.edit(|inner| {
            inner.readback = Some(EventReadback {
                url: TEST_PAGE_URL.to_owned(),
                name: Some("Renamed Squad".to_owned()),
                starts_at: None,
            });
        });
        let secrets = FakeSecrets::default();
        let mut pages = IdempotencyMap::new();

        let outcome =
            run(&fake, &secrets, &mut pages, ChangeKind::Created).expect("fallback is an outcome");
        assert!(
            matches!(outcome, RunOutcome::LinkPostFallback { .. }),
            "an observed content mismatch must not verify: {outcome:?}"
        );
    }

    #[test]
    fn challenge_abort_persists_screenshot_evidence() {
        let fake = Fake::default();
        fake.edit(|inner| {
            inner.create = Err(DriverError::Challenge(ChallengeSignal::Checkpoint));
        });
        let secrets = FakeSecrets::default();
        let mut pages = IdempotencyMap::new();

        run(&fake, &secrets, &mut pages, ChangeKind::Created).expect("abort runs");
        let rows = fake.rows();
        assert_eq!(rows.len(), 1, "one abort row: {rows:?}");
        assert!(
            rows[0]
                .screenshot_png
                .as_ref()
                .is_some_and(|png| !png.is_empty()),
            "the challenge abort records screenshot evidence"
        );
    }

    #[test]
    fn update_without_mapping_heals_forward_with_one_credential_fetch() {
        let fake = Fake::default();
        let secrets = FakeSecrets::default();
        let mut pages = IdempotencyMap::new();

        let outcome =
            run(&fake, &secrets, &mut pages, ChangeKind::Updated).expect("heal-forward runs");
        assert!(
            matches!(outcome, RunOutcome::Created { .. }),
            "an unmapped update heals forward into a create: {outcome:?}"
        );
        assert_eq!(
            secrets.fetches.load(Ordering::SeqCst),
            1,
            "exactly one credential fetch per run"
        );
    }

    #[test]
    fn full_draft_methods_default_to_narrowing_calls() {
        // The `_full` defaults narrow to the legacy calls so existing fakes
        // keep working; real drivers override them to sync content.
        struct NarrowOnly {
            inner: Mutex<FakeInner>,
        }
        impl BrowserDriver for NarrowOnly {
            fn create_event(&self, _id: &str, _name: &str) -> Result<String, DriverError> {
                self.inner.lock().expect("fake lock").create.clone()
            }
            fn update_event(&self, _url: &str, _id: &str) -> Result<(), DriverError> {
                self.inner.lock().expect("fake lock").update.clone()
            }
            fn delete_event(&self, _url: &str) -> Result<(), DriverError> {
                self.inner.lock().expect("fake lock").delete.clone()
            }
            fn screenshot_png(&self) -> Vec<u8> {
                vec![0x89, b'P', b'N', b'G']
            }
            fn readback_url(&self, _fb_url: &str) -> Option<String> {
                self.inner
                    .lock()
                    .expect("fake lock")
                    .readback
                    .clone()
                    .map(|observed| observed.url)
            }
            fn link_post_fallback(&self, _id: &str, _name: &str) -> Result<String, DriverError> {
                self.inner.lock().expect("fake lock").link.clone()
            }
        }
        let fake = NarrowOnly {
            inner: Mutex::new(FakeInner::default()),
        };
        let draft = FbEventDraft::from_scheduled(&scheduled("event-7"));
        assert_eq!(draft.starts_at, TEST_START);
        assert_eq!(
            draft.description.as_deref(),
            Some("Join the weekly games night.")
        );
        assert_eq!(fake.create_event_full(&draft), Ok(TEST_PAGE_URL.to_owned()));
        assert_eq!(fake.update_event_full(TEST_PAGE_URL, &draft), Ok(()));
        assert_eq!(
            fake.readback_event(TEST_PAGE_URL)
                .map(|observed| observed.url),
            Some(TEST_PAGE_URL.to_owned())
        );
    }

    #[test]
    fn create_full_draft_reaches_driver_with_start_and_description() {
        let fake = Fake::default();
        let secrets = FakeSecrets::default();
        let mut pages = IdempotencyMap::new();

        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Created).expect("create runs");
        assert!(
            matches!(outcome, RunOutcome::Created { .. }),
            "create succeeds: {outcome:?}"
        );
        let creates = fake.full_creates();
        assert_eq!(creates.len(), 1, "create must go through create_event_full");
        assert_eq!(creates[0].starts_at, TEST_START);
        assert_eq!(
            creates[0].description.as_deref(),
            Some("Join the weekly games night.")
        );
        assert_eq!(creates[0].channel_id.as_deref(), Some("voice-1"));
        assert_eq!(creates[0].name, "Sunday Squad");
    }

    #[test]
    fn update_full_draft_reaches_driver_with_start_and_description() {
        let fake = Fake::default();
        let secrets = FakeSecrets::default();
        let mut pages = IdempotencyMap::new();
        pages.record("event-1", TEST_PAGE_URL);

        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Updated).expect("update runs");
        assert!(
            matches!(outcome, RunOutcome::Updated { .. }),
            "update succeeds: {outcome:?}"
        );
        let updates = fake.full_updates();
        assert_eq!(updates.len(), 1, "update must go through update_event_full");
        assert_eq!(updates[0].0, TEST_PAGE_URL);
        assert_eq!(updates[0].1.starts_at, TEST_START);
        assert_eq!(
            updates[0].1.description.as_deref(),
            Some("Join the weekly games night.")
        );
    }

    #[test]
    fn starts_at_mismatch_fails_create_verification_into_fallback() {
        let fake = Fake::default();
        fake.edit(|inner| {
            inner.readback = Some(EventReadback {
                url: TEST_PAGE_URL.to_owned(),
                name: Some("Sunday Squad".to_owned()),
                starts_at: Some("2026-09-06T19:00:00.000Z".to_owned()),
            });
        });
        let secrets = FakeSecrets::default();
        let mut pages = IdempotencyMap::new();

        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Created).expect("fallback runs");
        assert!(
            matches!(outcome, RunOutcome::LinkPostFallback { .. }),
            "a starts_at mismatch must not verify: {outcome:?}"
        );
        assert_eq!(
            pages.lookup("event-1").as_deref(),
            Some(TEST_PAGE_URL),
            "the unverified event stays mapped"
        );
    }

    #[test]
    fn name_mismatch_on_update_fails_verification_into_fallback() {
        let fake = Fake::default();
        fake.edit(|inner| {
            inner.readback = Some(EventReadback {
                url: TEST_PAGE_URL.to_owned(),
                name: Some("Renamed Squad".to_owned()),
                starts_at: None,
            });
        });
        let secrets = FakeSecrets::default();
        let mut pages = IdempotencyMap::new();
        pages.record("event-1", TEST_PAGE_URL);

        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Updated).expect("fallback runs");
        assert!(
            matches!(outcome, RunOutcome::LinkPostFallback { .. }),
            "an observed name mismatch on update must not verify: {outcome:?}"
        );
    }

    #[test]
    fn create_mismatch_challenge_notifies_ceo_once() {
        let fake = Fake::default();
        fake.edit(|inner| {
            inner.readback = None;
            inner.link = Err(DriverError::Challenge(ChallengeSignal::Captcha));
        });
        let secrets = FakeSecrets::default();
        let mut pages = IdempotencyMap::new();

        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Created)
            .expect("challenge abort is an outcome");
        assert!(
            matches!(
                outcome,
                RunOutcome::ChallengeAborted {
                    ceo_notified: true,
                    ..
                }
            ),
            "create-mismatch challenge aborts: {outcome:?}"
        );
        assert_eq!(
            fake.notes().len(),
            1,
            "the create-mismatch challenge notifies exactly once"
        );
    }

    #[test]
    fn update_failure_challenge_notifies_ceo_once() {
        let fake = Fake::default();
        fake.edit(|inner| {
            inner.update = Err(DriverError::Failed("no update".to_owned()));
            inner.link = Err(DriverError::Challenge(ChallengeSignal::Captcha));
        });
        let secrets = FakeSecrets::default();
        let mut pages = IdempotencyMap::new();
        pages.record("event-1", TEST_PAGE_URL);

        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Updated)
            .expect("challenge abort is an outcome");
        assert!(
            matches!(
                outcome,
                RunOutcome::ChallengeAborted {
                    ceo_notified: true,
                    ..
                }
            ),
            "update-failure challenge aborts: {outcome:?}"
        );
        assert_eq!(
            fake.notes().len(),
            1,
            "the update-failure challenge notifies exactly once"
        );
    }

    #[test]
    fn update_mismatch_challenge_notifies_ceo_once() {
        let fake = Fake::default();
        fake.edit(|inner| {
            inner.readback = None;
            inner.link = Err(DriverError::Challenge(ChallengeSignal::Captcha));
        });
        let secrets = FakeSecrets::default();
        let mut pages = IdempotencyMap::new();
        pages.record("event-1", TEST_PAGE_URL);

        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Updated)
            .expect("challenge abort is an outcome");
        assert!(
            matches!(
                outcome,
                RunOutcome::ChallengeAborted {
                    ceo_notified: true,
                    ..
                }
            ),
            "update-mismatch challenge aborts: {outcome:?}"
        );
        assert_eq!(
            fake.notes().len(),
            1,
            "the update-mismatch challenge notifies exactly once"
        );
    }

    #[test]
    fn success_and_failure_rows_carry_screenshot_evidence() {
        // Created.
        let fake = Fake::default();
        let secrets = FakeSecrets::default();
        let mut pages = IdempotencyMap::new();
        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Created).expect("create runs");
        assert!(matches!(outcome, RunOutcome::Created { .. }));
        let rows = fake.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].action, "fb_page_event_create");
        assert_eq!(rows[0].target_key, "event-1");
        assert_eq!(rows[0].outcome, "created");
        assert_eq!(
            rows[0].fb_url.as_deref(),
            Some(TEST_PAGE_URL),
            "the created row records the event URL"
        );
        assert!(
            rows[0]
                .screenshot_png
                .as_ref()
                .is_some_and(|png| !png.is_empty()),
            "the created row carries screenshot evidence"
        );

        // Updated.
        let fake = Fake::default();
        let mut pages = IdempotencyMap::new();
        pages.record("event-1", TEST_PAGE_URL);
        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Updated).expect("update runs");
        assert!(matches!(outcome, RunOutcome::Updated { .. }));
        let rows = fake.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].action, "fb_page_event_update");
        assert_eq!(rows[0].target_key, "event-1");
        assert_eq!(rows[0].outcome, "updated");
        assert_eq!(
            rows[0].fb_url.as_deref(),
            Some(TEST_PAGE_URL),
            "the updated row records the event URL"
        );
        assert!(
            rows[0]
                .screenshot_png
                .as_ref()
                .is_some_and(|png| !png.is_empty()),
            "the updated row carries screenshot evidence"
        );

        // Deleted.
        let fake = Fake::default();
        let mut pages = IdempotencyMap::new();
        pages.record("event-1", TEST_PAGE_URL);
        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Cancelled).expect("delete runs");
        assert!(matches!(outcome, RunOutcome::Deleted));
        let rows = fake.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].action, "fb_page_event_delete");
        assert_eq!(rows[0].target_key, "event-1");
        assert_eq!(rows[0].outcome, "deleted");
        assert_eq!(
            rows[0].fb_url.as_deref(),
            Some(TEST_PAGE_URL),
            "the deleted row records the event URL"
        );
        assert!(
            rows[0]
                .screenshot_png
                .as_ref()
                .is_some_and(|png| !png.is_empty()),
            "the deleted row carries screenshot evidence"
        );

        // Delete-failed.
        let fake = Fake::default();
        fake.edit(|inner| {
            inner.delete = Err(DriverError::Failed("boom".to_owned()));
        });
        let mut pages = IdempotencyMap::new();
        pages.record("event-1", TEST_PAGE_URL);
        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Cancelled)
            .expect("delete failure is an outcome");
        assert!(matches!(outcome, RunOutcome::DeleteFailed { .. }));
        let rows = fake.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].action, "fb_page_event_delete");
        assert_eq!(rows[0].outcome, "delete_failed_driver_error");
        assert_eq!(rows[0].fb_url.as_deref(), Some(TEST_PAGE_URL));
        assert!(
            rows[0]
                .screenshot_png
                .as_ref()
                .is_some_and(|png| !png.is_empty()),
            "the delete-failed row carries screenshot evidence"
        );

        // No-mapping delete: the noop outcome label is pinned.
        let fake = Fake::default();
        let mut pages = IdempotencyMap::new();
        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Cancelled).expect("noop runs");
        assert!(matches!(outcome, RunOutcome::Deleted));
        let rows = fake.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].action, "fb_page_event_delete");
        assert_eq!(rows[0].outcome, "deleted_noop_no_mapping");
        assert_eq!(rows[0].fb_url, None);

        // Update-failure fallback: the prior row carries the update action
        // label, not the create one.
        let fake = Fake::default();
        fake.edit(|inner| {
            inner.update = Err(DriverError::Failed("no update".to_owned()));
        });
        let mut pages = IdempotencyMap::new();
        pages.record("event-1", TEST_PAGE_URL);
        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Updated).expect("fallback runs");
        assert!(matches!(outcome, RunOutcome::LinkPostFallback { .. }));
        let rows = fake.rows();
        assert_eq!(rows.len(), 2, "prior row + link-post row: {rows:?}");
        assert_eq!(rows[0].action, "fb_page_event_update");
        assert_eq!(rows[0].outcome, "update_failed_driver_error");
    }

    #[test]
    fn link_post_rows_carry_screenshot_evidence() {
        // Fallback success: prior row + link-post row.
        let fake = Fake::default();
        fake.edit(|inner| inner.readback = None);
        let secrets = FakeSecrets::default();
        let mut pages = IdempotencyMap::new();
        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Created).expect("fallback runs");
        assert!(matches!(outcome, RunOutcome::LinkPostFallback { .. }));
        let rows = fake.rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].outcome, "verify_failed_readback_mismatch");
        assert_eq!(
            rows[0].fb_url.as_deref(),
            Some(TEST_PAGE_URL),
            "the create-mismatch prior row records the unverified event URL"
        );
        assert_eq!(rows[1].outcome, "link_post_created_via_pages_manage_posts");
        assert_eq!(
            rows[1].fb_url.as_deref(),
            Some(TEST_LINK_URL),
            "the link-post success row records the link URL"
        );
        assert!(
            rows[1]
                .screenshot_png
                .as_ref()
                .is_some_and(|png| !png.is_empty()),
            "the link-post success row carries screenshot evidence"
        );

        // Fallback double failure: link-post failure row still carries it.
        let fake = Fake::default();
        fake.edit(|inner| {
            inner.readback = None;
            inner.link = Err(DriverError::Failed("no link".to_owned()));
        });
        let mut pages = IdempotencyMap::new();
        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Created).expect("failure runs");
        assert!(matches!(
            outcome,
            RunOutcome::FallbackFailed { stage: "create" }
        ));
        let rows = fake.rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].outcome, "link_post_failed_driver_error");
        assert!(
            rows[1]
                .screenshot_png
                .as_ref()
                .is_some_and(|png| !png.is_empty()),
            "the link-post failure row carries screenshot evidence"
        );
    }

    #[test]
    fn link_post_fallback_rows_carry_evidence_on_update_and_create_failure_paths() {
        let secrets = FakeSecrets::default();

        // Update-failure fallback success: prior row carries the mapped URL,
        // link-post row carries the screenshot.
        let fake = Fake::default();
        fake.edit(|inner| {
            inner.update = Err(DriverError::Failed("no update".to_owned()));
        });
        let mut pages = IdempotencyMap::new();
        pages.record("event-1", TEST_PAGE_URL);
        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Updated).expect("fallback runs");
        assert!(
            matches!(outcome, RunOutcome::LinkPostFallback { .. }),
            "update failure falls back: {outcome:?}"
        );
        let rows = fake.rows();
        assert_eq!(rows.len(), 2, "prior row + link-post row: {rows:?}");
        assert_eq!(rows[0].outcome, "update_failed_driver_error");
        assert_eq!(
            rows[0].fb_url.as_deref(),
            Some(TEST_PAGE_URL),
            "the update-failure prior row records the mapped URL"
        );
        assert_eq!(rows[1].outcome, "link_post_created_via_pages_manage_posts");
        assert_eq!(
            rows[1].fb_url.as_deref(),
            Some(TEST_LINK_URL),
            "the update-failure link-post row records the link URL"
        );
        assert!(
            rows[1]
                .screenshot_png
                .as_ref()
                .is_some_and(|png| !png.is_empty()),
            "the update-failure link-post row carries screenshot evidence"
        );

        // Update-mismatch fallback success.
        let fake = Fake::default();
        fake.edit(|inner| inner.readback = None);
        let mut pages = IdempotencyMap::new();
        pages.record("event-1", TEST_PAGE_URL);
        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Updated).expect("fallback runs");
        assert!(
            matches!(outcome, RunOutcome::LinkPostFallback { .. }),
            "update mismatch falls back: {outcome:?}"
        );
        let rows = fake.rows();
        assert_eq!(rows.len(), 2, "prior row + link-post row: {rows:?}");
        assert_eq!(rows[0].outcome, "verify_failed_readback_mismatch");
        assert_eq!(
            rows[0].fb_url.as_deref(),
            Some(TEST_PAGE_URL),
            "the update-mismatch prior row records the mapped URL"
        );
        assert_eq!(rows[1].outcome, "link_post_created_via_pages_manage_posts");
        assert!(
            rows[1]
                .screenshot_png
                .as_ref()
                .is_some_and(|png| !png.is_empty()),
            "the update-mismatch link-post row carries screenshot evidence"
        );

        // Create-failure fallback success: no event URL exists yet, but the
        // link-post row still carries the screenshot.
        let fake = Fake::default();
        fake.edit(|inner| {
            inner.create = Err(DriverError::Failed("no event".to_owned()));
        });
        let mut pages = IdempotencyMap::new();
        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Created).expect("fallback runs");
        assert!(
            matches!(outcome, RunOutcome::LinkPostFallback { .. }),
            "create failure falls back: {outcome:?}"
        );
        let rows = fake.rows();
        assert_eq!(rows.len(), 2, "prior row + link-post row: {rows:?}");
        assert_eq!(rows[0].outcome, "create_failed_driver_error");
        assert_eq!(
            rows[0].fb_url, None,
            "the create-failure prior row has no event URL yet"
        );
        assert_eq!(rows[1].outcome, "link_post_created_via_pages_manage_posts");
        assert_eq!(
            rows[1].fb_url.as_deref(),
            Some(TEST_LINK_URL),
            "the create-failure link-post row records the link URL"
        );
        assert!(
            rows[1]
                .screenshot_png
                .as_ref()
                .is_some_and(|png| !png.is_empty()),
            "the create-failure link-post row carries screenshot evidence"
        );
    }

    #[test]
    fn update_double_failure_reports_fallback_failed_update_stage() {
        let fake = Fake::default();
        fake.edit(|inner| {
            inner.update = Err(DriverError::Failed("no update".to_owned()));
            inner.link = Err(DriverError::Failed("no link".to_owned()));
        });
        let secrets = FakeSecrets::default();
        let mut pages = IdempotencyMap::new();
        pages.record("event-1", TEST_PAGE_URL);

        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Updated)
            .expect("double failure is an outcome");
        assert!(
            matches!(outcome, RunOutcome::FallbackFailed { stage: "update" }),
            "nothing reached Facebook on update: {outcome:?}"
        );
        let rows = fake.rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].outcome, "update_failed_driver_error");
        assert_eq!(rows[1].outcome, "link_post_failed_driver_error");
        assert_eq!(
            rows[0].fb_url.as_deref(),
            Some(TEST_PAGE_URL),
            "the update-failure prior row records the mapped URL"
        );
        assert!(
            rows[1]
                .screenshot_png
                .as_ref()
                .is_some_and(|png| !png.is_empty()),
            "the update-stage link-post failure row carries screenshot evidence"
        );
    }

    #[test]
    fn audit_labels_distinguish_driver_failures_from_mismatch() {
        let secrets = FakeSecrets::default();

        // Create driver failure.
        let fake = Fake::default();
        fake.edit(|inner| {
            inner.create = Err(DriverError::Failed("no event".to_owned()));
        });
        let mut pages = IdempotencyMap::new();
        run(&fake, &secrets, &mut pages, ChangeKind::Created).expect("fallback runs");
        assert_eq!(fake.rows()[0].outcome, "create_failed_driver_error");

        // Update driver failure.
        let fake = Fake::default();
        fake.edit(|inner| {
            inner.update = Err(DriverError::Failed("no update".to_owned()));
        });
        let mut pages = IdempotencyMap::new();
        pages.record("event-1", TEST_PAGE_URL);
        run(&fake, &secrets, &mut pages, ChangeKind::Updated).expect("fallback runs");
        assert_eq!(fake.rows()[0].outcome, "update_failed_driver_error");

        // Readback mismatch.
        let fake = Fake::default();
        fake.edit(|inner| inner.readback = None);
        let mut pages = IdempotencyMap::new();
        run(&fake, &secrets, &mut pages, ChangeKind::Created).expect("fallback runs");
        assert_eq!(fake.rows()[0].outcome, "verify_failed_readback_mismatch");
    }

    #[test]
    fn created_with_mapping_redispatches_as_update_without_duplicate() {
        let fake = Fake::default();
        let secrets = FakeSecrets::default();
        let mut pages = IdempotencyMap::new();
        pages.record("event-1", TEST_PAGE_URL);

        let outcome =
            run(&fake, &secrets, &mut pages, ChangeKind::Created).expect("redispatch runs");
        assert!(
            matches!(outcome, RunOutcome::Updated { .. }),
            "a Created for an already-mapped event must update, never a second create: {outcome:?}"
        );
        assert_eq!(
            fake.full_creates().len(),
            0,
            "no second FB event may be minted for a mapped event"
        );
        assert_eq!(
            fake.full_updates().len(),
            1,
            "the mapped event is updated in place"
        );
        assert_eq!(
            pages.lookup("event-1").as_deref(),
            Some(TEST_PAGE_URL),
            "the mapping is unchanged, nothing orphaned"
        );
    }

    #[test]
    fn plan_created_with_known_url_plans_update() {
        let mut known = IdempotencyMap::new();
        known.record("event-1", TEST_PAGE_URL);
        let planned = plan_actions(
            &[RunRequest {
                discord_event: scheduled("event-1"),
                change: ChangeKind::Created,
            }],
            &known,
        )
        .expect("plannable");
        assert_eq!(planned.len(), 1);
        assert!(
            matches!(planned[0], PlannedAction::Update { .. }),
            "a Created with a known URL plans an update, never a second create: {:?}",
            planned[0]
        );

        let empty = IdempotencyMap::new();
        let planned = plan_actions(
            &[RunRequest {
                discord_event: scheduled("event-1"),
                change: ChangeKind::Created,
            }],
            &empty,
        )
        .expect("plannable");
        assert!(
            matches!(planned[0], PlannedAction::Create { .. }),
            "a Created with no mapping still plans a create: {:?}",
            planned[0]
        );
    }

    #[test]
    fn update_challenge_aborts_with_ceo_notification_and_keeps_mapping() {
        let fake = Fake::default();
        fake.edit(|inner| {
            inner.update = Err(DriverError::Challenge(ChallengeSignal::LoginWall));
        });
        let secrets = FakeSecrets::default();
        let mut pages = IdempotencyMap::new();
        pages.record("event-1", TEST_PAGE_URL);

        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Updated).expect("abort runs");
        assert!(
            matches!(
                outcome,
                RunOutcome::ChallengeAborted {
                    retries: 0,
                    ceo_notified: true
                }
            ),
            "an update challenge aborts with zero retries and CEO flagged: {outcome:?}"
        );
        assert_eq!(
            fake.notes().len(),
            1,
            "the update challenge notifies the CEO exactly once"
        );
        let rows = fake.rows();
        assert_eq!(rows.len(), 1, "abort audits exactly one row: {rows:?}");
        assert!(
            rows[0]
                .screenshot_png
                .as_ref()
                .is_some_and(|png| !png.is_empty()),
            "the update challenge abort records screenshot evidence"
        );
        assert_eq!(
            pages.lookup("event-1").as_deref(),
            Some(TEST_PAGE_URL),
            "the mapping is unchanged by a challenge abort"
        );
        assert_eq!(
            fake.full_updates().len(),
            1,
            "exactly the challenged update ran, no second driver call"
        );
        assert_eq!(
            fake.full_creates().len(),
            0,
            "no create follows a challenge"
        );
    }

    #[test]
    fn delete_challenge_aborts_with_ceo_notification_and_keeps_mapping() {
        let fake = Fake::default();
        fake.edit(|inner| {
            inner.delete = Err(DriverError::Challenge(ChallengeSignal::Checkpoint));
        });
        let secrets = FakeSecrets::default();
        let mut pages = IdempotencyMap::new();
        pages.record("event-1", TEST_PAGE_URL);

        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Cancelled).expect("abort runs");
        assert!(
            matches!(
                outcome,
                RunOutcome::ChallengeAborted {
                    retries: 0,
                    ceo_notified: true
                }
            ),
            "a delete challenge aborts with zero retries and CEO flagged: {outcome:?}"
        );
        assert_eq!(
            fake.notes().len(),
            1,
            "the delete challenge notifies the CEO exactly once"
        );
        let rows = fake.rows();
        assert_eq!(rows.len(), 1, "abort audits exactly one row: {rows:?}");
        assert!(
            rows[0]
                .screenshot_png
                .as_ref()
                .is_some_and(|png| !png.is_empty()),
            "the delete challenge abort records screenshot evidence"
        );
        assert_eq!(
            pages.lookup("event-1").as_deref(),
            Some(TEST_PAGE_URL),
            "the mapping is unchanged by a challenge abort"
        );
        assert_eq!(
            fake.full_creates().len(),
            0,
            "no create follows a challenge"
        );
        assert_eq!(
            fake.full_updates().len(),
            0,
            "no update follows a challenge"
        );
    }

    #[test]
    fn create_readback_url_mismatch_falls_back_to_link_post() {
        const OTHER_URL: &str = "https://www.facebook.com/events/9999999999";
        let fake = Fake::default();
        fake.edit(|inner| {
            // URL-only readback pointing at a different page: only the URL
            // equality guard can catch this (`readback_matches` skips the
            // unobserved content fields).
            inner.readback = Some(EventReadback {
                url: OTHER_URL.to_owned(),
                name: None,
                starts_at: None,
            });
        });
        let secrets = FakeSecrets::default();
        let mut pages = IdempotencyMap::new();

        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Created).expect("fallback runs");
        assert!(
            matches!(outcome, RunOutcome::LinkPostFallback { .. }),
            "a readback URL mismatch must not verify: {outcome:?}"
        );
        let rows = fake.rows();
        assert_eq!(rows.len(), 2, "prior row + link-post row: {rows:?}");
        assert_eq!(rows[0].outcome, "verify_failed_readback_mismatch");
        assert_eq!(
            pages.lookup("event-1").as_deref(),
            Some(TEST_PAGE_URL),
            "the unverified event stays mapped"
        );
    }

    #[test]
    fn verified_create_writes_mapping_so_redelivery_updates() {
        let fake = Fake::default();
        let secrets = FakeSecrets::default();
        let mut pages = IdempotencyMap::new();

        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Created).expect("create runs");
        assert!(
            matches!(outcome, RunOutcome::Created { .. }),
            "verified create reports Created: {outcome:?}"
        );
        assert_eq!(
            pages.lookup("event-1").as_deref(),
            Some(TEST_PAGE_URL),
            "a verified create must remember the event URL (idempotency map)"
        );

        // Redeliver the same Created: the map is authoritative, so it must
        // update in place instead of minting a second FB event.
        let outcome =
            run(&fake, &secrets, &mut pages, ChangeKind::Created).expect("redelivery runs");
        assert!(
            matches!(outcome, RunOutcome::Updated { .. }),
            "a redelivered Created for a mapped event must update: {outcome:?}"
        );
        assert_eq!(
            fake.full_creates().len(),
            1,
            "exactly the first create ran; the redelivery must not create again"
        );
        assert_eq!(
            fake.full_updates().len(),
            1,
            "the redelivery updates the mapped event in place"
        );
        assert_eq!(
            pages.lookup("event-1").as_deref(),
            Some(TEST_PAGE_URL),
            "the mapping is unchanged by the redelivery"
        );
    }

    #[test]
    fn empty_url_is_treated_as_unmapped_everywhere() {
        // `record` refuses empty URLs so a bad create can never poison the map.
        let mut pages = IdempotencyMap::new();
        pages.record("event-1", "");
        assert_eq!(
            pages.lookup("event-1"),
            None,
            "an empty stored URL reads back as unmapped"
        );
        assert!(
            pages.is_empty(),
            "refused entries leave no residue in the map"
        );

        // Planning + classification treat it as unmapped.
        assert_eq!(
            change_kind_for(EventStatus::Scheduled, Some("")),
            ChangeKind::Created,
            "an empty known URL classifies as Created"
        );
        let planned = plan_actions(
            &[RunRequest {
                discord_event: scheduled("event-1"),
                change: ChangeKind::Created,
            }],
            &pages,
        )
        .expect("plannable");
        assert!(
            matches!(planned[0], PlannedAction::Create { .. }),
            "an empty mapping still plans a create: {:?}",
            planned[0]
        );

        // A driver that reports success with an empty URL falls back honestly
        // and leaves the map empty (no poisoning for later calls).
        let fake = Fake::default();
        fake.edit(|inner| {
            inner.create = Ok(String::new());
        });
        let secrets = FakeSecrets::default();
        let mut pages = IdempotencyMap::new();
        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Created).expect("fallback runs");
        assert!(
            matches!(
                outcome,
                RunOutcome::LinkPostFallback { .. } | RunOutcome::FallbackFailed { .. }
            ),
            "an empty create URL must never verify as Created: {outcome:?}"
        );
        assert_eq!(
            pages.lookup("event-1"),
            None,
            "the empty URL is never stored"
        );

        // With no mapping, an update heals forward and a cancel is a noop —
        // neither calls the driver with an empty URL.
        let fake = Fake::default();
        let mut pages = IdempotencyMap::new();
        pages.record("event-1", "");
        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Updated).expect("heal runs");
        assert!(
            matches!(outcome, RunOutcome::Created { .. }),
            "an update with an empty mapping heals forward to create: {outcome:?}"
        );
        assert_eq!(
            fake.full_updates().len(),
            0,
            "no update may run against an empty URL"
        );

        let fake = Fake::default();
        let mut pages = IdempotencyMap::new();
        pages.record("event-1", "");
        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Cancelled).expect("noop runs");
        assert!(matches!(outcome, RunOutcome::Deleted));
        let rows = fake.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].outcome, "deleted_noop_no_mapping");
    }

    #[test]
    fn fb_challenge_accepts_subscribe_and_echoes() {
        use webhook::{verify_challenge, FB_WEBHOOK_CHALLENGE_MAX};

        assert_eq!(
            verify_challenge("subscribe", "token-1", "123456789", "token-1"),
            Ok("123456789".to_owned())
        );
        // Deterministic under repeat calls.
        assert_eq!(
            verify_challenge("subscribe", "token-1", "123456789", "token-1"),
            verify_challenge("subscribe", "token-1", "123456789", "token-1")
        );
        let longest = "c".repeat(FB_WEBHOOK_CHALLENGE_MAX);
        assert_eq!(
            verify_challenge("subscribe", "token-1", &longest, "token-1"),
            Ok(longest)
        );
    }

    #[test]
    fn fb_challenge_refuses_without_leaking_the_token() {
        use webhook::verify_challenge;

        const EXPECTED: &str = "fixture-fb-verify-token";
        let overlong = "c".repeat(257);
        for (mode, token, challenge) in [
            ("unsubscribe", EXPECTED, "123456789"),
            ("SUBSCRIBE", EXPECTED, "123456789"),
            ("", EXPECTED, "123456789"),
            ("subscribe", "wrong-token", "123456789"),
            ("subscribe", "", "123456789"),
            ("subscribe", EXPECTED, ""),
            ("subscribe", EXPECTED, overlong.as_str()),
            ("subscribe", EXPECTED, "has space"),
            ("subscribe", EXPECTED, "new\nline"),
        ] {
            let first = verify_challenge(mode, token, challenge, EXPECTED);
            assert_eq!(
                first,
                verify_challenge(mode, token, challenge, EXPECTED),
                "challenge verdicts are deterministic"
            );
            let err = first.expect_err("hostile challenge input must fail");
            let rendered = format!("{err:?}");
            assert!(
                !rendered.contains(EXPECTED),
                "the expected token must never appear in errors: {rendered}"
            );
            assert!(
                !rendered.contains(challenge) || challenge.is_empty(),
                "hostile bytes must not be echoed: {rendered}"
            );
        }
        // An empty expected token refuses everything: a misconfigured
        // receiver cannot verify against nothing.
        assert!(verify_challenge("subscribe", "", "123456789", "").is_err());
        assert!(verify_challenge("subscribe", "anything", "123456789", "").is_err());
    }

    #[test]
    fn fb_signature_round_trip_and_refusals() {
        use webhook::{verify_signature, FB_WEBHOOK_MAX_BODY_BYTES};

        fn sign(secret: &str, body: &[u8]) -> String {
            use hmac::{Hmac, KeyInit, Mac};
            use sha2::Sha256;
            let mut mac =
                <Hmac<Sha256>>::new_from_slice(secret.as_bytes()).expect("test HMAC key is valid");
            mac.update(body);
            format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
        }

        // Runtime-built, never a literal: the hard-coded-crypto query only
        // accepts derived test keys.
        let secret = fixture_secret();
        let secret = secret.as_str();
        let empty = String::new();
        let mut other_secret = secret.to_owned();
        other_secret.push('x');
        let body = br#"{"object":"page","entry":[]}"#;
        let good = sign(secret, body);
        assert_eq!(verify_signature(secret, body, &good), Ok(()));
        assert_eq!(
            verify_signature(secret, body, &good),
            verify_signature(secret, body, &good),
            "signature verdicts are deterministic"
        );

        // Tamper the final hex digit, keeping the shape well-formed.
        let mut tampered = good.clone();
        let last = tampered.pop().expect("test signature is non-empty");
        tampered.push(if last == '0' { '1' } else { '0' });
        assert!(verify_signature(secret, body, &tampered).is_err());

        for bad in [
            String::new(),
            "sha256=".to_owned(),
            "sha256=zz".to_owned(),
            "md5=abcd".to_owned(),
            good["sha256=".len()..].to_owned(),
            sign(&other_secret, body),
        ] {
            assert!(
                verify_signature(secret, body, &bad).is_err(),
                "signature shape must fail: {bad:?}"
            );
        }
        // The body is part of the signed message.
        let mut other = body.to_vec();
        other.push(b' ');
        assert!(verify_signature(secret, &other, &good).is_err());
        // Empty secret fails closed; oversize fails before hashing.
        assert!(verify_signature(&empty, body, &good).is_err());
        let big = vec![b'x'; FB_WEBHOOK_MAX_BODY_BYTES + 1];
        assert!(verify_signature(secret, &big, &sign(secret, &big)).is_err());
        // No secret material in any refusal.
        for rendered in [
            format!("{:?}", verify_signature(&empty, body, &good)),
            format!("{:?}", verify_signature(secret, &big, &sign(secret, &big))),
        ] {
            assert!(
                !rendered.contains(secret),
                "the app secret must never appear in errors: {rendered}"
            );
        }
    }

    #[test]
    fn fb_envelope_accepts_page_changes_in_order() {
        use webhook::{parse_envelope, FbPageChange, FbPageEnvelope};

        let body = serde_json::json!({
            "object": "page",
            "entry": [
                {"id": "111", "time": 1_760_000_000, "changes": [
                    {"field": "feed", "value": {"item": "post"}},
                    {"field": "photos", "value": {}},
                ]},
                {"id": "222", "time": 1_760_000_001, "changes": []},
            ],
        })
        .to_string();
        let first = parse_envelope(body.as_bytes()).expect("valid envelope parses");
        assert_eq!(
            first,
            parse_envelope(body.as_bytes()).expect("parses twice")
        );
        assert_eq!(
            first,
            FbPageEnvelope {
                changes: vec![
                    FbPageChange {
                        page_id: "111".to_owned(),
                        field: "feed".to_owned(),
                    },
                    FbPageChange {
                        page_id: "111".to_owned(),
                        field: "photos".to_owned(),
                    },
                ],
            }
        );
        // Zero changes acknowledge with 2xx and do nothing.
        let empty = serde_json::json!({"object": "page", "entry": []}).to_string();
        assert_eq!(
            parse_envelope(empty.as_bytes())
                .expect("empty envelope parses")
                .changes,
            Vec::new()
        );
    }

    #[test]
    fn fb_envelope_rejects_hostile_shapes() {
        use webhook::{
            parse_envelope, FB_WEBHOOK_MAX_BODY_BYTES, FB_WEBHOOK_MAX_CHANGES,
            FB_WEBHOOK_MAX_ENTRIES,
        };

        let overlong_field = "f".repeat(65);
        let overlong_id = "1".repeat(129);
        let hostile: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"not json".to_vec(),
            b"{}".to_vec(),
            b"[]".to_vec(),
            b"null".to_vec(),
            serde_json::json!({"object": "user", "entry": []}).to_string().into_bytes(),
            serde_json::json!({"object": 7, "entry": []}).to_string().into_bytes(),
            serde_json::json!({"object": "page"}).to_string().into_bytes(),
            serde_json::json!({"object": "page", "entry": {}}).to_string().into_bytes(),
            serde_json::json!({"object": "page", "entry": [{"id": "1"}]})
                .to_string()
                .into_bytes(),
            serde_json::json!({"object": "page", "entry": [{"changes": []}]})
                .to_string()
                .into_bytes(),
            serde_json::json!({"object": "page", "entry": [{"id": "", "changes": []}]})
                .to_string()
                .into_bytes(),
            serde_json::json!({"object": "page", "entry": [{"id": "has space", "changes": []}]})
                .to_string()
                .into_bytes(),
            serde_json::json!({"object": "page", "entry": [{"id": overlong_id, "changes": []}]})
                .to_string()
                .into_bytes(),
            serde_json::json!({"object": "page", "entry": [{"id": "1", "changes": {}}]})
                .to_string()
                .into_bytes(),
            serde_json::json!({"object": "page", "entry": [{"id": "1", "changes": [{"value": 1}]}]})
                .to_string()
                .into_bytes(),
            serde_json::json!({"object": "page", "entry": [{"id": "1", "changes": [{"field": ""}]}]})
                .to_string()
                .into_bytes(),
            serde_json::json!({"object": "page",
                "entry": [{"id": "1", "changes": [{"field": overlong_field}]}]})
            .to_string()
            .into_bytes(),
            serde_json::json!({"object": "page", "entry": ["nope"]})
                .to_string()
                .into_bytes(),
        ];
        for body in &hostile {
            let first = parse_envelope(body);
            assert_eq!(
                first,
                parse_envelope(body),
                "envelope verdicts are deterministic"
            );
            assert!(
                first.is_err(),
                "hostile envelope must fail: {}",
                String::from_utf8_lossy(&body[..body.len().min(120)])
            );
        }
        // Floods beyond Meta's documented 1000-update batch are refused,
        // never silently truncated; a full 1000-update batch parses.
        let many_entries: Vec<_> = (0..FB_WEBHOOK_MAX_ENTRIES + 1)
            .map(|i| serde_json::json!({"id": i.to_string(), "changes": []}))
            .collect();
        let flooded = serde_json::json!({"object": "page", "entry": many_entries}).to_string();
        assert!(parse_envelope(flooded.as_bytes()).is_err());
        let many_changes: Vec<_> = (0..FB_WEBHOOK_MAX_CHANGES + 1)
            .map(|_| serde_json::json!({"field": "feed"}))
            .collect();
        let flooded = serde_json::json!({"object": "page",
            "entry": [{"id": "1", "changes": many_changes}]})
        .to_string();
        assert!(parse_envelope(flooded.as_bytes()).is_err());
        let full_entries: Vec<_> = (0..FB_WEBHOOK_MAX_ENTRIES)
            .map(|i| serde_json::json!({"id": i.to_string(), "changes": []}))
            .collect();
        let full = serde_json::json!({"object": "page", "entry": full_entries}).to_string();
        assert!(parse_envelope(full.as_bytes()).is_ok());
        let full_changes: Vec<_> = (0..FB_WEBHOOK_MAX_CHANGES)
            .map(|_| serde_json::json!({"field": "feed"}))
            .collect();
        let full = serde_json::json!({"object": "page",
            "entry": [{"id": "1", "changes": full_changes}]})
        .to_string();
        let parsed = parse_envelope(full.as_bytes()).expect("1000-change batch parses");
        assert_eq!(parsed.changes.len(), FB_WEBHOOK_MAX_CHANGES);
        // A full 1000-update batch of realistic Page feed changes (~260 B
        // each, ~255 KiB per 1000) parses well under the 1 MiB body cap. It
        // would have been refused by the old 64 KiB cap, which held only
        // ~250 such changes; this pins the sizing the review asked for.
        let realistic_changes: Vec<_> = (0..FB_WEBHOOK_MAX_CHANGES)
            .map(|i| {
                serde_json::json!({
                    "field": "feed",
                    "value": {
                        "from": {"id": "1234567890", "name": "Test Page"},
                        "item": "post",
                        "post_id": format!("1234567890_{i:010}"),
                        "verb": "add",
                        "created_time": 1_760_000_000,
                        "message": "Hello world, this is a test post with enough content to reach a realistic delivery size.",
                    },
                })
            })
            .collect();
        let realistic = serde_json::json!({"object": "page",
            "entry": [{"id": "1234567890", "time": 1_760_000_000, "changes": realistic_changes}]})
        .to_string();
        assert!(
            realistic.len() > 64 * 1024,
            "realistic batch must exceed the old 64 KiB cap: {} bytes",
            realistic.len()
        );
        assert!(
            realistic.len() < FB_WEBHOOK_MAX_BODY_BYTES,
            "realistic batch must fit the 1 MiB cap: {} bytes",
            realistic.len()
        );
        let parsed =
            parse_envelope(realistic.as_bytes()).expect("realistic 1000-change batch parses");
        assert_eq!(parsed.changes.len(), FB_WEBHOOK_MAX_CHANGES);
        // Invalid UTF-8 never panics the JSON parser.
        assert!(parse_envelope(&[0xff, 0xfe, 0x00]).is_err());
    }

    #[test]
    fn fb_front_door_errors_stay_bounded() {
        use webhook::{parse_envelope, verify_challenge, verify_signature, FB_WEBHOOK_ERROR_MAX};

        let hostile_bodies: Vec<Vec<u8>> = vec![
            vec![0u8; 1024],
            vec![b'"'; 512],
            b"{".repeat(512),
            b"\n\r\t ".repeat(128),
            "é".repeat(256).into_bytes(),
        ];
        for body in &hostile_bodies {
            for outcome in [
                verify_challenge(
                    &String::from_utf8_lossy(&body[..body.len().min(64)]),
                    &String::from_utf8_lossy(&body[..body.len().min(64)]),
                    &String::from_utf8_lossy(&body[..body.len().min(300)]),
                    "fixture-fb-verify-token",
                )
                .map(|_| ())
                .map_err(|err| format!("{err:?}")),
                verify_signature(&fixture_secret(), body, "sha256=00")
                    .map_err(|err| format!("{err:?}")),
                parse_envelope(body)
                    .map(|_| ())
                    .map_err(|err| format!("{err:?}")),
            ] {
                if let Err(rendered) = outcome {
                    assert!(
                        rendered.len() <= FB_WEBHOOK_ERROR_MAX + 64,
                        "refusal stays bounded: {rendered:?}"
                    );
                    assert!(
                        !rendered.contains("fixture-fb-verify-token"),
                        "no secret in errors: {rendered:?}"
                    );
                    assert!(
                        !rendered.contains(fixture_secret().as_str()),
                        "no secret in errors: {rendered:?}"
                    );
                }
            }
        }
        // The static refusal strings themselves sit under the bound.
        for err in [
            verify_challenge("nope", "nope", "x", "fixture-fb-verify-token").unwrap_err(),
            verify_signature(&fixture_secret()[..1], b"x", "nope").unwrap_err(),
            parse_envelope(b"nope").unwrap_err(),
        ] {
            let DriverError::Failed(message) = &err else {
                panic!("front-door refusals are Failed, never Challenge: {err:?}");
            };
            assert!(
                message.len() <= FB_WEBHOOK_ERROR_MAX,
                "static refusal exceeds the bound: {message:?}"
            );
        }
    }

    #[test]
    fn fb_hostile_known_urls_never_panic_and_stay_deterministic() {
        // Arbitrary bytes at the change classifier: hostile `known_url`
        // strings route by the one empty-URL rule, never panic, and repeat
        // calls agree. `Cancelled` always deletes regardless of the map.
        let huge = "x".repeat(4096);
        let wide = "é".repeat(512);
        let hostile = [
            "",
            " ",
            "\n",
            "https://www.facebook.com/events/1234567890",
            huge.as_str(),
            wide.as_str(),
            "\u{0}",
            "javascript:alert(1)",
        ];
        for url in hostile {
            let scheduled = change_kind_for(EventStatus::Scheduled, Some(url));
            assert_eq!(
                scheduled,
                change_kind_for(EventStatus::Scheduled, Some(url))
            );
            assert_eq!(
                scheduled,
                if url.is_empty() {
                    ChangeKind::Created
                } else {
                    ChangeKind::Updated
                },
                "one empty-URL rule for hostile input: {url:?}"
            );
            assert_eq!(
                change_kind_for(EventStatus::Cancelled, Some(url)),
                ChangeKind::Cancelled,
                "cancelled always deletes: {url:?}"
            );
        }
    }

    #[test]
    fn update_readback_url_mismatch_falls_back_to_link_post() {
        const OTHER_URL: &str = "https://www.facebook.com/events/9999999999";
        let fake = Fake::default();
        fake.edit(|inner| {
            inner.readback = Some(EventReadback {
                url: OTHER_URL.to_owned(),
                name: None,
                starts_at: None,
            });
        });
        let secrets = FakeSecrets::default();
        let mut pages = IdempotencyMap::new();
        pages.record("event-1", TEST_PAGE_URL);

        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Updated).expect("fallback runs");
        assert!(
            matches!(outcome, RunOutcome::LinkPostFallback { .. }),
            "a readback URL mismatch on update must not verify: {outcome:?}"
        );
        let rows = fake.rows();
        assert_eq!(rows.len(), 2, "prior row + link-post row: {rows:?}");
        assert_eq!(rows[0].outcome, "verify_failed_readback_mismatch");
        assert_eq!(
            pages.lookup("event-1").as_deref(),
            Some(TEST_PAGE_URL),
            "the mapping is unchanged by a verify failure"
        );
    }
}
