#![cfg(test)]
//! Protected offline acceptance: `LeaseKeeper` refuses unusable keeper claims.
//!
//! Baseline bug (the paired build owns the fix): `LeaseKeeper::start` takes
//! two `Option` claims and panics on both unusable shapes. `(None, None)`
//! panics synchronously in `.expect`, and `(Some, Some)` panics inside the
//! spawned renewal task via `unreachable!`, where the panic kills renewal
//! silently (`lost` never set, handle unobserved). No test pins exactly-one.
//!
//! Protected file: the builder must NEVER edit this file. Return necessary
//! test corrections to the test author. This file must compile both on the
//! baseline and after the builder lands; the trait fallbacks below provide the
//! new API surface on the baseline, and an inherent item shadows its fallback
//! once added.
//!
//! Builder contract (all required for GREEN):
//! 1. Add `impl LeaseKeeper { pub(crate) fn try_start(store: SelfRoleStore,
//!    event: Option<EventClaim>, panel: Option<PanelClaim>, lost:
//!    Arc<AtomicBool>) -> Result<Self, KeeperError> }` (name the error type
//!    freely). `(None, None)` and `(Some, Some)` return `Err`; exactly-one
//!    returns `Ok` with unchanged renewal semantics.
//! 2. The error is typed: it implements `std::error::Error`, its `Display` and
//!    `Debug` name the claim-shape problem (both contain "claim",
//!    case-insensitive), and neither rendering echoes claim tokens.
//! 3. Remove the claim-shape panic sites from the renewal path: the
//!    `.expect("a renewal keeper has a claim")` and the
//!    `unreachable!("one keeper per claim")`.
//! 4. Keep `LeaseKeeper::start` with its current `(store,
//!    Option<EventClaim>, Option<PanelClaim>, lost) -> Self` signature (this
//!    file's baseline fallback calls it), reimplemented without a claim-shape
//!    panic — for example by delegating invalid shapes to an immediately-lost
//!    keeper.
//! 5. Add `#[doc(hidden)] pub fn PanelClaim::offline_fixture(renew_after_ms:
//!    u64) -> Self` in `two_bot_cutover` for offline tests: fixed key
//!    (`keeper-offline-guild` / `keeper-offline-member` /
//!    `keeper-offline-panel`), token
//!    `Secret::new("keeper-offline-panel-sentinel")`, generation `1`, default
//!    target, the given `renew_after_ms`, `maintenance: false`.
//!
//! Offline: every test builds a lazy pool that is closed before any use, so no
//! socket opens and no database is touched. Renewal ticks against the closed
//! pool fail immediately and latch `lost` honestly. No network, no database,
//! no `#[ignore]`.
//!
//! Expected results on the baseline: `neither_claim_construction_refuses_*`
//! RED (panics instead of refusing — the bug), `*_panic_sites` RED (the panic
//! strings are present), `both_claims_*` and `panel_claim_*` RED (the baseline
//! has no offline `PanelClaim` constructor, so the fixture fallback refuses
//! honestly — this RED turns behavioral GREEN once the builder's fixture and
//! refusal land), `event_claim_*` and `drop_aborts_*` GREEN (controls).

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use two_bot_core::Secret;
use two_bot_cutover::self_role_store::{AuditEffects, EventClaim, PanelClaim, SelfRoleStore};

use super::LeaseKeeper;

const EVENT_SENTINEL: &str = "keeper-offline-event-sentinel";
const PANEL_SENTINEL: &str = "keeper-offline-panel-sentinel";
const RUNTIME_SOURCE: &str = include_str!("self_role_runtime.rs");

/// Baseline fallback for the builder's `LeaseKeeper::try_start` (contract
/// item 1). Calls the legacy `start` directly: neither-claim panics
/// synchronously (the test catches it), both-claims returns `Ok` with a
/// silently-doomed renewal task, exactly-one returns `Ok`. An inherent
/// `try_start` shadows this fallback once the builder adds it.
trait KeeperStartFallback {
    type Error: std::fmt::Debug + std::fmt::Display + std::error::Error;
    fn try_start(
        store: SelfRoleStore,
        event: Option<EventClaim>,
        panel: Option<PanelClaim>,
        lost: Arc<AtomicBool>,
    ) -> Result<LeaseKeeper, Self::Error>;
}

impl KeeperStartFallback for LeaseKeeper {
    type Error = std::convert::Infallible;
    fn try_start(
        store: SelfRoleStore,
        event: Option<EventClaim>,
        panel: Option<PanelClaim>,
        lost: Arc<AtomicBool>,
    ) -> Result<LeaseKeeper, Self::Error> {
        Ok(Self::start(store, event, panel, lost))
    }
}

/// Baseline fallback for the builder's offline `PanelClaim` constructor
/// (contract item 5). The baseline builds panel claims only from a database,
/// so this refuses honestly: RED until the builder's inherent
/// `PanelClaim::offline_fixture` shadows it.
trait PanelFixtureFallback {
    fn offline_fixture(renew_after_ms: u64) -> PanelClaim;
}

impl PanelFixtureFallback for PanelClaim {
    fn offline_fixture(_renew_after_ms: u64) -> PanelClaim {
        panic!(
            "RED: baseline PanelClaim has no offline constructor; \
             the builder lands inherent PanelClaim::offline_fixture (see header contract)"
        );
    }
}

/// A store that can never touch a database: the lazy pool is closed before any
/// use, so renewal attempts fail immediately and latch `lost` honestly.
async fn closed_store() -> SelfRoleStore {
    let options = PgConnectOptions::new()
        .host("127.0.0.1")
        .port(1)
        .username("offline_unused")
        .database("offline_unused");
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_lazy_with(options);
    pool.close().await;
    SelfRoleStore::new(pool)
}

fn event_claim(renew_after_ms: u64) -> EventClaim {
    EventClaim {
        event_id: "keeper-offline-event".to_owned(),
        token: Secret::new(EVENT_SENTINEL.to_owned()),
        generation: 1,
        recovered: false,
        intent_initialized: true,
        compensating: false,
        exchange_pending: false,
        effects: AuditEffects::default(),
        desired_role_ids: Vec::new(),
        pre_mutation_role_ids: Vec::new(),
        renew_after_ms,
    }
}

/// Render a caught panic payload. Match-based (never `expect`) so the tests
/// need no `Debug` on keeper types.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = payload.downcast_ref::<&str>() {
        message.to_string()
    } else {
        "<non-string panic>".to_owned()
    }
}

/// The typed-refusal shape from contract item 2: a real error type whose
/// renderings name the claim-shape problem and never echo claim tokens.
fn assert_claim_shape_refusal(error: &impl std::error::Error) {
    let display = error.to_string();
    let debug = format!("{error:?}");
    for rendered in [&display, &debug] {
        assert!(
            rendered.to_ascii_lowercase().contains("claim"),
            "refusal must name the claim-shape problem: {rendered}"
        );
        for sentinel in [EVENT_SENTINEL, PANEL_SENTINEL] {
            assert!(
                !rendered.contains(sentinel),
                "refusal must not echo claim tokens: {rendered}"
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn neither_claim_construction_refuses_with_typed_error() {
    let store = closed_store().await;
    let lost = Arc::new(AtomicBool::new(false));
    let attempt = catch_unwind(AssertUnwindSafe(|| {
        LeaseKeeper::try_start(store, None, None, Arc::clone(&lost))
    }));
    let result = match attempt {
        Ok(result) => result,
        Err(payload) => {
            let what = panic_message(&payload);
            panic!(
                "construction must refuse with a typed error, not panic (panicked with: {what})"
            );
        }
    };
    match result {
        Err(error) => assert_claim_shape_refusal(&error),
        Ok(_) => panic!("neither-claim construction must refuse"),
    }
    assert!(
        !lost.load(Ordering::SeqCst),
        "a refused keeper spawns no renewal"
    );
}

#[tokio::test(start_paused = true)]
async fn both_claims_construction_refuses() {
    let store = closed_store().await;
    let lost = Arc::new(AtomicBool::new(false));
    // Long renewals: no tick can fire during refusal; a keeper (if wrongly
    // built) is dropped with its task unpolled.
    let event = event_claim(3_600_000);
    let panel = PanelClaim::offline_fixture(3_600_000);
    let attempt = catch_unwind(AssertUnwindSafe(|| {
        LeaseKeeper::try_start(store, Some(event), Some(panel), Arc::clone(&lost))
    }));
    let result = match attempt {
        Ok(result) => result,
        Err(payload) => {
            let what = panic_message(&payload);
            panic!(
                "construction must refuse with a typed error, not panic (panicked with: {what})"
            );
        }
    };
    match result {
        Err(error) => assert_claim_shape_refusal(&error),
        Ok(_) => panic!("both-claims construction must refuse"),
    }
    assert!(
        !lost.load(Ordering::SeqCst),
        "a refused keeper spawns no renewal"
    );
}

#[tokio::test(start_paused = true)]
async fn event_claim_still_renews_on_schedule() {
    let store = closed_store().await;
    let lost = Arc::new(AtomicBool::new(false));
    let keeper = LeaseKeeper::try_start(store, Some(event_claim(25)), None, Arc::clone(&lost))
        .expect("exactly-one event claim is accepted");
    tokio::task::yield_now().await;
    assert!(
        !lost.load(Ordering::SeqCst),
        "no renewal before the first tick"
    );
    tokio::time::advance(Duration::from_millis(25)).await;
    for _ in 0..4 {
        if lost.load(Ordering::SeqCst) {
            break;
        }
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(61)).await;
    }
    assert!(
        lost.load(Ordering::SeqCst),
        "the renewal tick fired and latched the failed renewal"
    );
    drop(keeper);
}

#[tokio::test(start_paused = true)]
async fn panel_claim_still_renews_on_schedule() {
    let store = closed_store().await;
    let lost = Arc::new(AtomicBool::new(false));
    let panel = PanelClaim::offline_fixture(25);
    let keeper = LeaseKeeper::try_start(store, None, Some(panel), Arc::clone(&lost))
        .expect("exactly-one panel claim is accepted");
    tokio::task::yield_now().await;
    assert!(
        !lost.load(Ordering::SeqCst),
        "no renewal before the first tick"
    );
    tokio::time::advance(Duration::from_millis(25)).await;
    for _ in 0..4 {
        if lost.load(Ordering::SeqCst) {
            break;
        }
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(61)).await;
    }
    assert!(
        lost.load(Ordering::SeqCst),
        "the renewal tick fired and latched the failed renewal"
    );
    drop(keeper);
}

#[tokio::test(start_paused = true)]
async fn drop_aborts_pending_renewal() {
    let store = closed_store().await;
    let lost = Arc::new(AtomicBool::new(false));
    let keeper =
        LeaseKeeper::try_start(store, Some(event_claim(3_600_000)), None, Arc::clone(&lost))
            .expect("exactly-one event claim is accepted");
    tokio::task::yield_now().await;
    drop(keeper);
    tokio::time::advance(Duration::from_secs(7_200)).await;
    tokio::task::yield_now().await;
    assert!(
        !lost.load(Ordering::SeqCst),
        "dropping the keeper aborts its renewal task"
    );
}

#[test]
fn renewal_path_has_no_claim_shape_panic_sites() {
    for site in ["one keeper per claim", "a renewal keeper has a claim"] {
        assert!(
            !RUNTIME_SOURCE.contains(site),
            "claim-shape panic site must be gone: {site}"
        );
    }
    assert!(
        RUNTIME_SOURCE.contains("fn try_start"),
        "fallible LeaseKeeper::try_start must exist"
    );
}
