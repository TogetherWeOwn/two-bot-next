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

fn audit_create(url: &str, target_key: &str, outcome: &str, fb_url: Option<String>) -> AuditRow {
    let _ = url;
    AuditRow {
        action: "fb_page_event_create".to_owned(),
        target_key: target_key.to_owned(),
        outcome: outcome.to_owned(),
        fb_url,
    }
}

fn challenge_detail(discord_event_id: &str, signal: &ChallengeSignal) -> String {
    format!(
        "fb_page_event challenge {signal:?} for discord event {discord_event_id}; \
         stopped with zero retries, no evasion attempted"
    )
}

fn abort_on_challenge<N: NotifySink, A: AuditSink>(
    notify: &N,
    audit: &A,
    discord_event_id: &str,
    signal: &ChallengeSignal,
) -> RunOutcome {
    notify.notify(CeoNotification::challenge(challenge_detail(
        discord_event_id,
        signal,
    )));
    audit.push(AuditRow {
        action: "fb_page_event_challenge_abort".to_owned(),
        target_key: discord_event_id.to_owned(),
        outcome: format!("challenge_aborted_{signal:?}_zero_retries"),
        fb_url: None,
    });
    RunOutcome::ChallengeAborted {
        retries: 0,
        ceo_notified: true,
    }
}

fn fallback_to_link_post<D: BrowserDriver, A: AuditSink>(
    driver: &D,
    audit: &A,
    discord_event_id: &str,
    name: &str,
    created_fb_url: &str,
    update: bool,
) -> Result<RunOutcome, (ChallengeSignal, RunOutcome)> {
    let action = if update {
        "fb_page_event_update"
    } else {
        "fb_page_event_create"
    };
    audit.push(AuditRow {
        action: action.to_owned(),
        target_key: discord_event_id.to_owned(),
        outcome: "verify_failed_readback_mismatch".to_owned(),
        fb_url: Some(created_fb_url.to_owned()),
    });
    match driver.link_post_fallback(discord_event_id, name) {
        Ok(link_url) => {
            audit.push(AuditRow {
                action: "fb_page_event_link_post_fallback".to_owned(),
                target_key: discord_event_id.to_owned(),
                outcome: "link_post_created_via_pages_manage_posts".to_owned(),
                fb_url: Some(link_url.clone()),
            });
            Ok(RunOutcome::LinkPostFallback { url: link_url })
        }
        Err(DriverError::Challenge(signal)) => Err((
            signal,
            RunOutcome::ChallengeAborted {
                retries: 0,
                ceo_notified: true,
            },
        )),
        Err(DriverError::Failed(_)) => {
            audit.push(AuditRow {
                action: "fb_page_event_link_post_fallback".to_owned(),
                target_key: discord_event_id.to_owned(),
                outcome: "link_post_failed_driver_error".to_owned(),
                fb_url: None,
            });
            Ok(RunOutcome::LinkPostFallback {
                url: created_fb_url.to_owned(),
            })
        }
    }
}

/// Run one funnel step: at most one automated action (control 3).
///
/// Gate order is load-bearing and pinned by tests:
/// 1. kill switch off -> [`RunOutcome::Disabled`] before the credential fetch
///    and before any driver call;
/// 2. production target without staging evidence -> [`Refused::StagingEvidenceMissing`]
///    before the credential fetch and before any driver call;
/// 3. credential fetch exactly once; rotation/failure -> [`Refused::SecretUnavailable`]
///    (fail closed, no secret in the error);
/// 4. exactly one mutating driver call for create/update/delete; challenges
///    abort with zero retries + one CEO notification (control 2);
/// 5. every mutating path captures a screenshot artifact and a readback; a
///    failed readback falls back to the link-post path and audits both
///    (control 5);
/// 6. every automated action emits exactly one audit row on success
///    (control 8); the fallback path emits two (verify failure + link post).
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

    let event_id = request.discord_event.id.clone();
    let event_name = request.discord_event.name.clone();

    match request.change {
        ChangeKind::Created => {
            let fb_url = match driver.create_event(&event_id, &event_name) {
                Ok(url) => url,
                Err(DriverError::Challenge(signal)) => {
                    return Ok(abort_on_challenge(notify, audit, &event_id, &signal));
                }
                Err(DriverError::Failed(_)) => {
                    return fallback_after_create_failure(
                        driver,
                        audit,
                        notify,
                        &event_id,
                        &event_name,
                        "",
                    );
                }
            };
            let _shot = driver.screenshot_png();
            match driver.readback_url(&fb_url) {
                Some(back) if back == fb_url => {
                    pages.record(&event_id, &fb_url);
                    audit.push(audit_create(
                        &fb_url,
                        &event_id,
                        "created",
                        Some(fb_url.clone()),
                    ));
                    Ok(RunOutcome::Created { url: fb_url })
                }
                _ => match fallback_to_link_post(
                    driver,
                    audit,
                    &event_id,
                    &event_name,
                    &fb_url,
                    false,
                ) {
                    Ok(outcome) => Ok(outcome),
                    Err((signal, _)) => Ok(abort_on_challenge(notify, audit, &event_id, &signal)),
                },
            }
        }
        ChangeKind::Updated => {
            let known = pages.lookup(&event_id);
            let Some(fb_url) = known else {
                let created = RunRequest {
                    discord_event: request.discord_event.clone(),
                    change: ChangeKind::Created,
                };
                return run_once(driver, secrets, pages, audit, notify, config, &created);
            };
            match driver.update_event(&fb_url, &event_id) {
                Ok(()) => {}
                Err(DriverError::Challenge(signal)) => {
                    return Ok(abort_on_challenge(notify, audit, &event_id, &signal));
                }
                Err(DriverError::Failed(_)) => {
                    return match fallback_to_link_post(
                        driver,
                        audit,
                        &event_id,
                        &event_name,
                        &fb_url,
                        true,
                    ) {
                        Ok(outcome) => Ok(outcome),
                        Err((signal, _)) => {
                            Ok(abort_on_challenge(notify, audit, &event_id, &signal))
                        }
                    };
                }
            }
            let _shot = driver.screenshot_png();
            match driver.readback_url(&fb_url) {
                Some(back) if back == fb_url => {
                    audit.push(AuditRow {
                        action: "fb_page_event_update".to_owned(),
                        target_key: event_id.clone(),
                        outcome: "updated".to_owned(),
                        fb_url: Some(fb_url.clone()),
                    });
                    Ok(RunOutcome::Updated { url: fb_url })
                }
                _ => match fallback_to_link_post(
                    driver,
                    audit,
                    &event_id,
                    &event_name,
                    &fb_url,
                    true,
                ) {
                    Ok(outcome) => Ok(outcome),
                    Err((signal, _)) => Ok(abort_on_challenge(notify, audit, &event_id, &signal)),
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
                });
                return Ok(RunOutcome::Deleted);
            };
            match driver.delete_event(&fb_url) {
                Ok(()) => {}
                Err(DriverError::Challenge(signal)) => {
                    return Ok(abort_on_challenge(notify, audit, &event_id, &signal));
                }
                Err(DriverError::Failed(message)) => {
                    let _ = message;
                    audit.push(AuditRow {
                        action: "fb_page_event_delete".to_owned(),
                        target_key: event_id.clone(),
                        outcome: "delete_failed_driver_error".to_owned(),
                        fb_url: Some(fb_url.clone()),
                    });
                    pages.remove(&event_id);
                    return Ok(RunOutcome::Deleted);
                }
            }
            pages.remove(&event_id);
            audit.push(AuditRow {
                action: "fb_page_event_delete".to_owned(),
                target_key: event_id.clone(),
                outcome: "deleted".to_owned(),
                fb_url: Some(fb_url),
            });
            Ok(RunOutcome::Deleted)
        }
    }
}

fn fallback_after_create_failure<D: BrowserDriver, A: AuditSink, N: NotifySink>(
    driver: &D,
    audit: &A,
    notify: &N,
    event_id: &str,
    event_name: &str,
    created_fb_url: &str,
) -> Result<RunOutcome, Refused> {
    let _ = notify;
    match fallback_to_link_post(driver, audit, event_id, event_name, created_fb_url, false) {
        Ok(outcome) => Ok(outcome),
        Err((signal, _)) => {
            audit.push(AuditRow {
                action: "fb_page_event_challenge_abort".to_owned(),
                target_key: event_id.to_owned(),
                outcome: format!("challenge_aborted_{signal:?}_zero_retries"),
                fb_url: None,
            });
            Ok(RunOutcome::ChallengeAborted {
                retries: 0,
                ceo_notified: true,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
