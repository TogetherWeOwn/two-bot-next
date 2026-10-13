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

    #[must_use]
    pub fn lookup(&self, discord_event_id: &str) -> Option<String> {
        self.inner.get(discord_event_id).cloned()
    }

    pub fn record(&mut self, discord_event_id: &str, fb_url: &str) {
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
    Created { url: String },
    Updated { url: String },
    Deleted,
    LinkPostFallback { url: String },
    /// Delete failed: the FB event is still live, so the mapping is kept and
    /// a later cancel can retry. Never reported as [`RunOutcome::Deleted`]
    /// (control 4, never silent success).
    DeleteFailed { fb_url: String },
    /// Primary action and the link-post fallback both failed: nothing new
    /// reached Facebook. `stage` is `"create"` or `"update"`. Never shaped
    /// as [`RunOutcome::LinkPostFallback`] (never silent success).
    FallbackFailed { stage: &'static str },
    ChallengeAborted { retries: u32, ceo_notified: bool },
    Disabled,
}

/// Collapse a batch of requests to at most one action (control 3).
///
/// - Empty input plans nothing.
/// - Two or more *distinct* Discord event ids are refused as [`Refused::Bulk`]
///   (no backfill sweep, no bulk fan-out).
/// - Duplicate deliveries of the same event collapse to the latest request.
/// - `Created` plans a create; `Updated` plans an update when the map already
///   holds a URL (otherwise a create, so a lost map entry heals forward);
///   `Cancelled` plans a delete.
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
        ChangeKind::Created => PlannedAction::Create {
            discord_event_id: id,
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
    observed.name.as_deref().is_none_or(|name| name == draft.name)
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
///    heals forward inside [`execute`] without a second fetch (control 6);
/// 4. exactly one mutating driver call for create/update/delete; challenges
///    abort with zero retries + one CEO notification (control 2);
/// 5. every mutating path captures a screenshot artifact, persisted on the
///    audit row, plus a readback; a failed readback falls back to the
///    link-post path and audits both (control 5);
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
            let fb_url = match driver.create_event_full(&draft) {
                Ok(url) => url,
                Err(DriverError::Challenge(signal)) => {
                    return abort_on_challenge(driver, notify, audit, &event_id, &signal);
                }
                Err(DriverError::Failed(_)) => {
                    return fallback_after_create_failure(driver, audit, notify, &draft);
                }
            };
            let shot = driver.screenshot_png();
            match driver.readback_event(&fb_url) {
                Some(observed)
                    if observed.url == fb_url && readback_matches(&draft, &observed) =>
                {
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
                Some(observed)
                    if observed.url == fb_url && readback_matches(&draft, &observed) =>
                {
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
                    Err(signal) => {
                        abort_on_challenge(driver, notify, audit, &event_id, &signal)
                    }
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
        Err(signal) => abort_on_challenge(
            driver,
            notify,
            audit,
            &draft.discord_event_id,
            &signal,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

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
            self.inner.lock().expect("fake lock").notes.push(notification);
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
        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Cancelled)
            .expect("retry runs");
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

        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Created)
            .expect("fallback is an outcome");
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

        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Created)
            .expect("fallback is an outcome");
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

        let outcome = run(&fake, &secrets, &mut pages, ChangeKind::Updated)
            .expect("heal-forward runs");
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
        let fake = Fake::default();
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
}
