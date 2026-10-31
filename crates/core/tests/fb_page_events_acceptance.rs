//! FB Page events from Discord events — risk-control acceptance (TOG-22974).
//!
//! TESTS ONLY. This file pins the 7 risk controls from the parent plan
//! ([TOG-22903](/TOG/issues/TOG-22903), controls 2-8) as failing tests. No
//! production code changes in this slice; the Builder implements the
//! production side on this branch without touching this file.
//!
//! ## Builder contract
//!
//! Create `crates/core/src/fb_page_events.rs` (wired through `lib.rs`) exposing
//! exactly the items imported below with the signatures used below:
//!
//! - `PageTarget::{StagingTestPage, ProductionPage}`
//! - `FbPageEventsConfig { kill_switch_on: bool, staging_evidence: bool, target: PageTarget }`
//! - `ChangeKind::{Created, Updated, Cancelled}` + `change_kind_for(status, known_url)`
//! - `RunRequest { discord_event: ScheduledEvent, change: ChangeKind }`
//! - `BrowserDriver` trait (`create_event`, `update_event`, `delete_event`,
//!   `screenshot_png`, `readback_url`, `link_post_fallback`; all `Send + Sync`)
//! - `DriverCall::{Create, Update, Delete, Screenshot, Readback, LinkPost}`
//! - `DriverError::{Challenge(ChallengeSignal), Failed(String)}`
//! - `ChallengeSignal::{Checkpoint, Captcha, LoginWall, TwoFaReprompt}`
//! - `SecretStore` trait (`fetch_page_token`) + `PageToken` (Debug-redacted)
//! - `SecretError::{Unavailable, RotationInProgress}`
//! - `NotifySink` trait (`notify`) + `CeoNotification::challenge(detail)`
//! - `AuditSink` trait (`push`) + `AuditRow { action, target_key, outcome, fb_url }`
//!   (shaped like `FeedAudit` in `crates/core/src/feeds_store.rs:271`, following
//!   the `crates/bot/src/feed_jobs.rs` audit pattern)
//! - `IdempotencyMap` (`lookup`, `record`, `remove`, `len`)
//! - `PlannedAction::{Create, Update, Delete, LinkPostFallback}`
//! - `Refused::{Bulk, StagingEvidenceMissing, SecretUnavailable}`
//! - `RunOutcome::{Created, Updated, Deleted, LinkPostFallback, ChallengeAborted, Disabled}`
//! - `plan_actions(requests, known) -> Result<Vec<PlannedAction>, Refused>`
//! - `run_once(driver, secrets, pages, audit, notify, config, request)
//!   -> Result<RunOutcome, Refused>` (synchronous; no network, no real Meta access)
//!
//! ## Existing seams grounding each control
//!
//! - `normalize_event` / `EventStatus` (`crates/core/src/scheduled_events.rs`):
//!   every request below starts from a normalized Discord mirror row; Cancelled
//!   rows route to delete.
//! - `replace_events` (`crates/core/src/website_store.rs:298`): the atomic-swap
//!   contract is why the idempotency map must be update-not-duplicate at the
//!   orchestrator layer, never a whole-guild sweep.
//! - `FeatureGates` (`crates/core/src/feature_commands.rs:342`): the kill
//!   switch follows the env-gate pattern; the kill test pins the mapping.
//! - `write_audit` / `FeedAudit` + `feed_jobs.rs` audit pattern: every automated
//!   action emits one audit row; the audit test pins the shape.
//!
//! ## Rules for these tests
//!
//! Fake driver only (trait boundary above). No Meta access, no credentials, no
//! network. Fixture secrets are synthetic (`fixture-…`) and must never appear
//! in logs, errors, or audit rows.

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};

use two_bot_core::fb_page_events::{
    change_kind_for, plan_actions, run_once, AuditRow, AuditSink, BrowserDriver, CeoNotification,
    ChallengeSignal, DriverCall, DriverError, FbPageEventsConfig, IdempotencyMap, NotifySink,
    PageTarget, PlannedAction, Refused, RunOutcome, RunRequest, SecretError, SecretStore,
};
use two_bot_core::{normalize_event, EventStatus, FeatureGates, RawScheduledEvent, ScheduledEvent};

const START: &str = "2026-09-06T18:00:00.000Z";
const PAGE_URL: &str = "https://www.facebook.com/events/1234567890";
const LINK_POST_URL: &str = "https://www.facebook.com/fixture-page/posts/abcdef";
const FIXTURE_TOKEN: &str = "fixture-fb-page-credential";

/// Build a normalized Discord mirror row (existing `scheduled_events` seam).
fn discord_event(id: &str, status: i64) -> ScheduledEvent {
    normalize_event(&RawScheduledEvent {
        id: Some(id.to_owned()),
        name: Some("Sunday Squad".to_owned()),
        scheduled_start_time: Some(START.to_owned()),
        channel_id: Some("voice-1".to_owned()),
        description: Some("Join the weekly games night.".to_owned()),
        status: Some(status),
    })
    .expect("fixture normalizes")
}

fn staging_config() -> FbPageEventsConfig {
    FbPageEventsConfig {
        kill_switch_on: true,
        staging_evidence: true,
        target: PageTarget::StagingTestPage,
    }
}

#[derive(Debug, Default)]
struct FakeState {
    calls: Vec<DriverCall>,
    challenge: bool,
    readback: Option<String>,
}

#[derive(Debug, Default)]
struct FakeDriver {
    state: Mutex<FakeState>,
}

impl FakeDriver {
    fn quiet() -> Self {
        Self {
            state: Mutex::new(FakeState {
                calls: Vec::new(),
                challenge: false,
                readback: Some(PAGE_URL.to_owned()),
            }),
        }
    }

    fn challenging() -> Self {
        Self {
            state: Mutex::new(FakeState {
                calls: Vec::new(),
                challenge: true,
                readback: None,
            }),
        }
    }

    fn unreadable() -> Self {
        Self {
            state: Mutex::new(FakeState {
                calls: Vec::new(),
                challenge: false,
                readback: None,
            }),
        }
    }

    fn mutating_calls(&self) -> Vec<DriverCall> {
        self.state
            .lock()
            .expect("fake lock")
            .calls
            .iter()
            .filter(|call| !matches!(call, DriverCall::Screenshot | DriverCall::Readback { .. }))
            .cloned()
            .collect()
    }

    fn all_calls(&self) -> Vec<DriverCall> {
        self.state.lock().expect("fake lock").calls.clone()
    }
}

impl BrowserDriver for FakeDriver {
    fn create_event(&self, discord_event_id: &str, _name: &str) -> Result<String, DriverError> {
        let mut state = self.state.lock().expect("fake lock");
        if state.challenge {
            return Err(DriverError::Challenge(ChallengeSignal::Checkpoint));
        }
        state.calls.push(DriverCall::Create {
            discord_event_id: discord_event_id.to_owned(),
        });
        Ok(PAGE_URL.to_owned())
    }

    fn update_event(&self, fb_url: &str, discord_event_id: &str) -> Result<(), DriverError> {
        let mut state = self.state.lock().expect("fake lock");
        if state.challenge {
            return Err(DriverError::Challenge(ChallengeSignal::Captcha));
        }
        state.calls.push(DriverCall::Update {
            fb_url: fb_url.to_owned(),
            discord_event_id: discord_event_id.to_owned(),
        });
        Ok(())
    }

    fn delete_event(&self, fb_url: &str) -> Result<(), DriverError> {
        let mut state = self.state.lock().expect("fake lock");
        if state.challenge {
            return Err(DriverError::Challenge(ChallengeSignal::LoginWall));
        }
        state.calls.push(DriverCall::Delete {
            fb_url: fb_url.to_owned(),
        });
        Ok(())
    }

    fn screenshot_png(&self) -> Vec<u8> {
        self.state
            .lock()
            .expect("fake lock")
            .calls
            .push(DriverCall::Screenshot);
        vec![0x89, b'P', b'N', b'G']
    }

    fn readback_url(&self, fb_url: &str) -> Option<String> {
        let mut state = self.state.lock().expect("fake lock");
        state.calls.push(DriverCall::Readback {
            fb_url: fb_url.to_owned(),
        });
        state.readback.clone()
    }

    fn link_post_fallback(
        &self,
        discord_event_id: &str,
        _name: &str,
    ) -> Result<String, DriverError> {
        let mut state = self.state.lock().expect("fake lock");
        if state.challenge {
            return Err(DriverError::Challenge(ChallengeSignal::TwoFaReprompt));
        }
        state.calls.push(DriverCall::LinkPost {
            discord_event_id: discord_event_id.to_owned(),
        });
        Ok(LINK_POST_URL.to_owned())
    }
}

#[derive(Debug, Default)]
struct FakeSecrets {
    fetches: AtomicUsize,
    fail: bool,
}

impl FakeSecrets {
    fn ok() -> Self {
        Self {
            fetches: AtomicUsize::new(0),
            fail: false,
        }
    }

    fn failing() -> Self {
        Self {
            fetches: AtomicUsize::new(0),
            fail: true,
        }
    }
}

impl SecretStore for FakeSecrets {
    fn fetch_page_token(&self) -> Result<two_bot_core::fb_page_events::PageToken, SecretError> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            return Err(SecretError::RotationInProgress);
        }
        Ok(two_bot_core::fb_page_events::PageToken::new(
            FIXTURE_TOKEN.to_owned(),
        ))
    }
}

#[derive(Debug, Default)]
struct FakeAudit {
    rows: Mutex<Vec<AuditRow>>,
}

impl AuditSink for FakeAudit {
    fn push(&self, row: AuditRow) {
        self.rows.lock().expect("fake lock").push(row);
    }
}

impl FakeAudit {
    fn rows(&self) -> Vec<AuditRow> {
        self.rows.lock().expect("fake lock").clone()
    }
}

#[derive(Debug, Default)]
struct FakeNotify {
    notes: Mutex<Vec<CeoNotification>>,
}

impl NotifySink for FakeNotify {
    fn notify(&self, notification: CeoNotification) {
        self.notes.lock().expect("fake lock").push(notification);
    }
}

impl FakeNotify {
    fn notes(&self) -> Vec<CeoNotification> {
        self.notes.lock().expect("fake lock").clone()
    }
}

fn harness(
    driver: &FakeDriver,
    secrets: &FakeSecrets,
    pages: &mut IdempotencyMap,
    audit: &FakeAudit,
    notify: &FakeNotify,
    config: &FbPageEventsConfig,
    request: &RunRequest,
) -> Result<RunOutcome, Refused> {
    run_once(driver, secrets, pages, audit, notify, config, request)
}

/// 1. challenge-stop-and-notify: a Meta challenge aborts the run with zero
///    retries, zero mutating driver calls, and one queued CEO notification.
#[test]
fn challenge_signal_aborts_run_with_zero_retries_and_ceo_notification_queued() {
    let driver = FakeDriver::challenging();
    let secrets = FakeSecrets::ok();
    let mut pages = IdempotencyMap::default();
    let audit = FakeAudit::default();
    let notify = FakeNotify::default();
    let request = RunRequest {
        discord_event: discord_event("event-1", 1),
        change: change_kind_for(EventStatus::Scheduled, None),
    };

    let outcome = harness(
        &driver,
        &secrets,
        &mut pages,
        &audit,
        &notify,
        &staging_config(),
        &request,
    )
    .expect("challenge abort is an outcome, not a refusal");

    assert!(
        matches!(
            outcome,
            RunOutcome::ChallengeAborted {
                retries: 0,
                ceo_notified: true
            }
        ),
        "challenge must abort with zero retries and CEO flagged: {outcome:?}"
    );
    assert!(
        driver.mutating_calls().is_empty(),
        "no mutating call may fire after a challenge: {:?}",
        driver.all_calls()
    );
    assert_eq!(
        notify.notes().len(),
        1,
        "exactly one CEO notification is queued, never zero, never retried"
    );
    assert_eq!(
        pages.len(),
        0,
        "an aborted run records no idempotency entry"
    );
}

/// 2. low-volume: duplicate deliveries collapse to one action; bulk is refused.
#[test]
fn low_volume_single_action_per_event_and_bulk_refused() {
    let event = discord_event("event-1", 1);
    let mut known = IdempotencyMap::default();
    let duplicate = vec![
        RunRequest {
            discord_event: event.clone(),
            change: change_kind_for(EventStatus::Scheduled, None),
        },
        RunRequest {
            discord_event: event.clone(),
            change: change_kind_for(EventStatus::Scheduled, None),
        },
    ];
    let planned = plan_actions(&duplicate, &known).expect("plannable");
    assert_eq!(
        planned.len(),
        1,
        "duplicate Discord deliveries must collapse to one action: {planned:?}"
    );
    assert!(
        matches!(planned[0], PlannedAction::Create { .. }),
        "first sighting plans a create: {:?}",
        planned[0]
    );

    known.record("event-1", PAGE_URL);
    let update = vec![RunRequest {
        discord_event: event,
        change: change_kind_for(EventStatus::Scheduled, known.lookup("event-1").as_deref()),
    }];
    let planned = plan_actions(&update, &known).expect("plannable");
    assert_eq!(planned.len(), 1, "an update plans exactly one action");
    assert!(
        matches!(planned[0], PlannedAction::Update { .. }),
        "a known id maps to update, never a second create: {:?}",
        planned[0]
    );

    let bulk = vec![
        RunRequest {
            discord_event: discord_event("event-1", 1),
            change: change_kind_for(EventStatus::Scheduled, None),
        },
        RunRequest {
            discord_event: discord_event("event-2", 1),
            change: change_kind_for(EventStatus::Scheduled, None),
        },
    ];
    assert!(
        matches!(plan_actions(&bulk, &known), Err(Refused::Bulk)),
        "bulk input (two distinct events, backfill sweep) is refused outright"
    );
}

/// 3. idempotency: update reuses the mapped URL (never duplicates);
///    Discord cancel deletes the mapped event and drops the entry.
#[test]
fn idempotency_update_reuses_mapped_url_and_cancel_deletes() {
    let driver = FakeDriver::quiet();
    let secrets = FakeSecrets::ok();
    let mut pages = IdempotencyMap::default();
    pages.record("event-1", PAGE_URL);
    let audit = FakeAudit::default();
    let notify = FakeNotify::default();

    let update = RunRequest {
        discord_event: discord_event("event-1", 1),
        change: change_kind_for(EventStatus::Scheduled, pages.lookup("event-1").as_deref()),
    };
    let outcome = harness(
        &driver,
        &secrets,
        &mut pages,
        &audit,
        &notify,
        &staging_config(),
        &update,
    )
    .expect("update runs");
    assert!(
        matches!(outcome, RunOutcome::Updated { ref url } if url == PAGE_URL),
        "update must reuse the mapped URL, not mint a second event: {outcome:?}"
    );
    assert!(
        !driver
            .all_calls()
            .iter()
            .any(|call| matches!(call, DriverCall::Create { .. })),
        "update path must never call create: {:?}",
        driver.all_calls()
    );

    let cancel = RunRequest {
        discord_event: discord_event("event-1", 4),
        change: change_kind_for(EventStatus::Cancelled, pages.lookup("event-1").as_deref()),
    };
    assert_eq!(
        cancel.discord_event.status,
        EventStatus::Cancelled,
        "the cancel fixture must ride the Cancelled seam"
    );
    let outcome = harness(
        &driver,
        &secrets,
        &mut pages,
        &audit,
        &notify,
        &staging_config(),
        &cancel,
    )
    .expect("cancel runs");
    assert!(
        matches!(outcome, RunOutcome::Deleted),
        "Discord cancel must delete the mapped FB event: {outcome:?}"
    );
    assert_eq!(
        pages.lookup("event-1"),
        None,
        "the idempotency entry is dropped after delete"
    );
}

/// 4. verify: screenshot + readback per action; on readback failure the run
///    falls back to the Graph API Page link-post path, auditing both.
#[test]
fn verify_failure_falls_back_to_link_post_and_audits_both() {
    let driver = FakeDriver::unreadable();
    let secrets = FakeSecrets::ok();
    let mut pages = IdempotencyMap::default();
    let audit = FakeAudit::default();
    let notify = FakeNotify::default();
    let request = RunRequest {
        discord_event: discord_event("event-9", 1),
        change: change_kind_for(EventStatus::Scheduled, None),
    };

    let outcome = harness(
        &driver,
        &secrets,
        &mut pages,
        &audit,
        &notify,
        &staging_config(),
        &request,
    )
    .expect("fallback is an outcome, not a refusal");
    assert!(
        matches!(outcome, RunOutcome::LinkPostFallback { ref url } if url == LINK_POST_URL),
        "failed readback must take the link-post fallback, never silent success: {outcome:?}"
    );
    assert!(
        driver
            .all_calls()
            .iter()
            .any(|call| matches!(call, DriverCall::Screenshot)),
        "a screenshot artifact is captured even on the fallback path: {:?}",
        driver.all_calls()
    );
    assert!(
        driver
            .all_calls()
            .iter()
            .any(|call| matches!(call, DriverCall::LinkPost { .. })),
        "the fallback posts a Page link via the API path: {:?}",
        driver.all_calls()
    );
    let outcomes: Vec<String> = audit.rows().iter().map(|row| row.outcome.clone()).collect();
    assert!(
        outcomes.iter().any(|o| o.contains("verify_failed")),
        "the failed verification is audited: {outcomes:?}"
    );
    assert!(
        outcomes.iter().any(|o| o.contains("link_post")),
        "the fallback post is audited too: {outcomes:?}"
    );
}

/// 5. secret redaction: the secret store is mocked, and no log, error, or
///    audit text carries secret material.
#[test]
fn logs_and_errors_carry_no_secret_material_with_mocked_secret_store() {
    let driver = FakeDriver::quiet();
    let secrets = FakeSecrets::ok();
    let mut pages = IdempotencyMap::default();
    let audit = FakeAudit::default();
    let notify = FakeNotify::default();
    let request = RunRequest {
        discord_event: discord_event("event-1", 1),
        change: change_kind_for(EventStatus::Scheduled, None),
    };

    let outcome = harness(
        &driver,
        &secrets,
        &mut pages,
        &audit,
        &notify,
        &staging_config(),
        &request,
    )
    .expect("happy path runs");
    assert_eq!(
        secrets.fetches.load(Ordering::SeqCst),
        1,
        "the page credential comes from the (mocked) secret store, exactly once"
    );
    for rendered in [
        format!("{outcome:?}"),
        format!("{:?}", audit.rows()),
        format!("{:?}", notify.notes()),
    ] {
        assert!(
            !rendered.contains(FIXTURE_TOKEN),
            "synthetic credential reached test-visible output: {rendered}"
        );
    }

    let token = two_bot_core::fb_page_events::PageToken::new(FIXTURE_TOKEN.to_owned());
    for rendered in [format!("{token:?}"), format!("{token:#?}")] {
        assert!(
            !rendered.contains(FIXTURE_TOKEN),
            "the token type itself must redact on Debug: {rendered}"
        );
    }

    let failing = FakeSecrets::failing();
    let mut pages = IdempotencyMap::default();
    let refused = harness(
        &driver,
        &failing,
        &mut pages,
        &audit,
        &notify,
        &staging_config(),
        &request,
    );
    assert!(
        matches!(refused, Err(Refused::SecretUnavailable)),
        "rotation/failure mid-run fails closed: {refused:?}"
    );
    let rendered = format!("{refused:?}");
    assert!(
        !rendered.contains(FIXTURE_TOKEN),
        "fail-closed errors carry no secret material: {rendered}"
    );
}

/// 6. staging-first: a production Page target without the staging-evidence
///    flag is refused before any driver call.
#[test]
fn production_target_refused_without_staging_evidence() {
    let driver = FakeDriver::quiet();
    let secrets = FakeSecrets::ok();
    let mut pages = IdempotencyMap::default();
    let audit = FakeAudit::default();
    let notify = FakeNotify::default();
    let config = FbPageEventsConfig {
        kill_switch_on: true,
        staging_evidence: false,
        target: PageTarget::ProductionPage,
    };
    let request = RunRequest {
        discord_event: discord_event("event-1", 1),
        change: change_kind_for(EventStatus::Scheduled, None),
    };

    let refused = harness(
        &driver, &secrets, &mut pages, &audit, &notify, &config, &request,
    );
    assert!(
        matches!(refused, Err(Refused::StagingEvidenceMissing)),
        "production without staging evidence is refused: {refused:?}"
    );
    assert!(
        driver.all_calls().is_empty(),
        "the gate fires before any driver call: {:?}",
        driver.all_calls()
    );
}

/// 7a. kill switch: off means zero driver calls and a Disabled outcome.
/// Grounds the `FeatureGates` env-gate pattern.
#[test]
fn kill_switch_off_makes_zero_driver_calls() {
    let vars = HashMap::from([("TWO_AUTOMATIONS".to_owned(), "0".to_owned())]);
    let gates = FeatureGates::from_map(&vars).expect("gates parse");
    assert!(
        !gates.automations,
        "the fixture mirrors the switch being off"
    );

    let driver = FakeDriver::quiet();
    let secrets = FakeSecrets::ok();
    let mut pages = IdempotencyMap::default();
    let audit = FakeAudit::default();
    let notify = FakeNotify::default();
    let config = FbPageEventsConfig {
        kill_switch_on: gates.automations,
        staging_evidence: true,
        target: PageTarget::StagingTestPage,
    };
    let request = RunRequest {
        discord_event: discord_event("event-1", 1),
        change: change_kind_for(EventStatus::Scheduled, None),
    };

    let outcome = harness(
        &driver, &secrets, &mut pages, &audit, &notify, &config, &request,
    )
    .expect("disabled is an outcome, not a refusal");
    assert!(
        matches!(outcome, RunOutcome::Disabled),
        "switch off reports Disabled: {outcome:?}"
    );
    assert!(
        driver.all_calls().is_empty(),
        "switch off means zero driver calls: {:?}",
        driver.all_calls()
    );
    assert_eq!(
        secrets.fetches.load(Ordering::SeqCst),
        0,
        "switch off must not even fetch the credential"
    );
}

/// 7b. audit: every automated action emits exactly one audit row shaped like
/// the `feed_jobs` / `write_audit` pattern (action, target key, outcome).
#[test]
fn every_automated_action_emits_one_audit_row() {
    let driver = FakeDriver::quiet();
    let secrets = FakeSecrets::ok();
    let mut pages = IdempotencyMap::default();
    let audit = FakeAudit::default();
    let notify = FakeNotify::default();
    let request = RunRequest {
        discord_event: discord_event("event-1", 1),
        change: change_kind_for(EventStatus::Scheduled, None),
    };

    let outcome = harness(
        &driver,
        &secrets,
        &mut pages,
        &audit,
        &notify,
        &staging_config(),
        &request,
    )
    .expect("happy path runs");
    let url = match outcome {
        RunOutcome::Created { url } => url,
        other => panic!("expected a created event, got {other:?}"),
    };

    let rows = audit.rows();
    assert_eq!(
        rows.len(),
        1,
        "exactly one audit row per automated action: {rows:?}"
    );
    assert_eq!(
        rows[0].target_key, "event-1",
        "audit names the Discord event"
    );
    assert_eq!(
        rows[0].fb_url.as_deref(),
        Some(url.as_str()),
        "audit carries the FB URL"
    );
    assert!(
        rows[0].action.contains("fb_page_event"),
        "audit action names the funnel (feed_jobs pattern): {:?}",
        rows[0]
    );
    assert!(
        !rows[0].outcome.is_empty(),
        "audit outcome is always recorded: {:?}",
        rows[0]
    );
}
