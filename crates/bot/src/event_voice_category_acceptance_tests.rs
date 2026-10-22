#![cfg(test)]

//! Acceptance tests: required event-voice category config + per-tick
//! fail-closed preflight (host decision D4, accepted changes R12/R13).
//!
//! The event-voice category is REQUIRED configuration: there is no default,
//! nothing is auto-picked, and until a usable category exists the feature is
//! inactive, the website shows the TBD reason `no_category`, and staff get
//! one alert. Every tick preflights Manage Channels + View Channel on the
//! configured category before any create; on 403 the tick stays TBD with
//! reason `perm_denied`, emits the per-tick counter + alert, retries with
//! backoff, and never crashes the tick. A mod moving the channel to another
//! category is accepted (cached parent updated, no fight); a move into a
//! private category flips the website link to members-only. PATCH COMPLETED
//! requires Manage Events: on 403 the local handoff still completes and the
//! PATCH is retried, so the after-party is never blocked on the event PATCH.
//!
//! Criterion map (acceptance card):
//!
//! - C1 (unset id -> inactive, `no_category`, one staff alert):
//!   `loader_*`, `unset_category_*`, `reconfiguring_to_the_same_value_*`.
//! - C2 (deleted-category id -> same inactivity, no create attempted):
//!   `deleted_category_*`. "Same" means the same `Waiting(NoCategory)`
//!   decision and alert shape; the one discovery GET that proves the id is
//!   deleted is inherent (an id cannot be known-deleted without looking)
//!   and is backoff-limited.
//! - C3 (category 403 -> `perm_denied` + counter/alert, tick survives,
//!   backoff retry): `category_403_*`, `insufficient_category_grant_*`,
//!   `incomplete_permission_snapshot_*`, `alert_latch_rearms_*`,
//!   `reconfiguring_to_a_new_category_*`.
//! - C4 (mod move accepted and cached; private move -> members-only link):
//!   `mod_category_move_*`, `move_into_private_category_*`,
//!   `ready_create_target_ignores_mod_moves`.
//! - C5 (event PATCH 403 -> local handoff completes, PATCH retried):
//!   `event_patch_*`, `queued_patch_*`, `patch_*`.
//! - Shared TBD vocabulary (`opens_soon`, `rate_limited`, `perm_denied`,
//!   `no_category`, `unavailable`): `tbd_reasons_match_*`.
//! - Transient discovery failures fail closed without alert spam:
//!   `discovery_429_*`, `discovery_transport_failure_*`.
//!
//! "No create attempted" is pinned at this layer as the gate's decision:
//! the T-15 ticker card must only create on `PreflightDecision::Ready`, and
//! `create_parent()` is `None` while unset. Website TBD rendering and the
//! `web_v1` columns belong to the web card; these tests pin the reason
//! vocabulary and link flags that card consumes.
//!
//! Builder contract (this file and its two-line `mod` include in
//! `voice_rooms_tests.rs` are immutable; the Builder builds on this branch
//! and never edits them):
//!
//! - New `crate::event_voice_category` module, declared `pub mod` in
//!   `crates/bot/src/lib.rs` (lib scope: these tests run in lib tests;
//!   declare it in `main.rs` too if the tick driver needs it, mirroring
//!   the `containment_runtime` dual declaration).
//! - `pub const EVENT_VOICE_CATEGORY_KEY: &str =
//!   "TWO_EVENT_VOICE_CATEGORY_ID"`; `pub fn
//!   configured_event_voice_category(env: impl Fn(&str) -> Option<String>)
//!   -> Option<Snowflake>` consulting exactly that key, trimming, treating
//!   missing/blank as unset, and failing closed (`None`, never a panic)
//!   on unparsable, zero, or non-canonical values (leading zeros, commas,
//!   negative, trailing junk).
//! - `pub enum EventVoiceTbdReason { OpensSoon, RateLimited, PermDenied,
//!   NoCategory, Unavailable }` (`Debug + Clone + Copy + PartialEq + Eq`)
//!   with `pub fn as_str(self) -> &'static str` returning exactly
//!   `opens_soon`, `rate_limited`, `perm_denied`, `no_category`,
//!   `unavailable`.
//! - `pub struct CategorySnapshot { pub id: Snowflake, pub
//!   bot_permissions: Option<Permissions>, pub everyone_can_view: bool }`
//!   (`Permissions` is `twilight_model::guild::Permissions`); `pub trait
//!   CategoryReader: Send + Sync` with `fn category_grant(&self, guild:
//!   Snowflake, category: Snowflake) -> impl Future<Output =
//!   Result<CategorySnapshot, RoomHttpError>> + Send` (production: one
//!   category GET plus the bot's effective permissions there).
//! - `pub struct StaffAlert { pub reason: EventVoiceTbdReason, pub
//!   category: Option<Snowflake> }`; `pub trait StaffAlertSink: Send +
//!   Sync` with `fn send_staff_alert(&self, alert: StaffAlert)`.
//! - `pub enum PreflightDecision { Ready { category: Snowflake }, Waiting
//!   { reason: EventVoiceTbdReason } }` (`Debug + Clone + Copy + PartialEq
//!   + Eq`).
//! - `pub struct EventVoiceCategoryGate` with `pub fn new(configured:
//!   Option<Snowflake>) -> Self`, `pub fn configured(&self) ->
//!   Option<Snowflake>`, `pub fn effective_category(&self) ->
//!   Option<Snowflake>` (configured until a mod move is observed),
//!   `pub fn members_only_link(&self) -> bool`, `pub fn create_parent(&self)
//!   -> Option<Snowflake>` (always the configured id; mod moves never
//!   change the create target), `pub fn note_mod_move(&mut self,
//!   observed_parent: Snowflake, everyone_can_view: bool)`, `pub fn
//!   reconfigure(&mut self, configured: Option<Snowflake>)` (a changed id
//!   clears backoff, the cached move, and the alert latch; the same id
//!   keeps them, so a per-tick env re-read never re-spams), and `pub async
//!   fn preflight(&mut self, reader: &impl CategoryReader, alerts: &impl
//!   StaffAlertSink, guild: Snowflake, now: Instant) -> PreflightDecision`.
//!   Preflight: `None` configured returns `Waiting(NoCategory)` without a
//!   reader call; `NotFound` returns `Waiting(NoCategory)` with backoff;
//!   `AccessDenied` returns `Waiting(PermDenied)` with backoff; a snapshot
//!   whose permissions lack `VIEW_CHANNEL | MANAGE_CHANNELS`, or are
//!   `None`, returns `Waiting(PermDenied)` and is rechecked every tick;
//!   `RateLimited` returns `Waiting(RateLimited)` with backoff; any other
//!   transport failure returns `Waiting(Unavailable)` with backoff. Failed
//!   preflights never propagate an error. `Waiting` records one
//!   `two_bot_event_voice_tick_total{outcome="<reason>"}` observation per
//!   call, including backoff-skipped ticks. Alerts fire at most once per
//!   episode for `no_category` and `perm_denied` only, re-arming after a
//!   `Ready`; `rate_limited` and `unavailable` never alert.
//! - `pub const PREFLIGHT_DENIED_BACKOFF: Duration`: while a failed
//!   preflight's backoff has not elapsed, `preflight` returns the same
//!   `Waiting` reason without calling the reader.
//! - `pub trait EventStatusPatcher: Send + Sync` with `fn
//!   patch_event_completed(&self, guild: Snowflake, event: Snowflake) ->
//!   impl Future<Output = Result<(), RoomHttpError>> + Send`; `pub enum
//!   PatchOutcome { Published, QueuedForRetry }` (`Debug + Clone + Copy +
//!   PartialEq + Eq`); `pub const EVENT_PATCH_RETRY_AFTER: Duration`; `pub
//!   struct PendingCompletedPatch` with `pub fn for_event(guild: Snowflake,
//!   event: Snowflake) -> Self`, `pub fn event(&self) -> Snowflake`, `pub
//!   fn is_pending(&self) -> bool`, and `pub async fn ensure_published(&mut
//!   self, patcher: &impl EventStatusPatcher, now: Instant) ->
//!   PatchOutcome`. `AccessDenied` and `RateLimited` queue for retry
//!   (returning normally, never failing the tick; the caller completes the
//!   local handoff first and the helper never blocks it); `NotFound` (event
//!   gone) resolves as `Published` instead of looping; success publishes.
//!   A published patch makes no further wire calls.
//! - Metrics: `metrics.event_voice_tick(outcome)` recording
//!   `two_bot_event_voice_tick_total{outcome="<reason>"}` with a bounded
//!   outcome vocabulary containing at least `no_category`, `perm_denied`,
//!   and `rate_limited` (R23's full per-tick family lives with the ticker
//!   card; extend the same series there). Catalog the new metric for the
//!   observability conformance tests.
//! - Env/docs: `TWO_EVENT_VOICE_CATEGORY_ID` joins
//!   `docs/configuration.md`, `wrangler/src/container-env.ts`
//!   `FORWARDED_FLAGS` (plus the container-env drift test), and the
//!   settings catalog the drift tests pin.
//!
//! RED: this file does not compile until the module exists; the missing
//! `crate::event_voice_category` import below is the first failure. GREEN
//! was proved on a fully-reverted scratch implementation; the `control_`
//! test uses only today's seams.

use super::*;
use crate::event_voice_category::{
    configured_event_voice_category, CategoryReader, CategorySnapshot, EventStatusPatcher,
    EventVoiceCategoryGate, EventVoiceTbdReason, PatchOutcome, PendingCompletedPatch,
    PreflightDecision, StaffAlert, StaffAlertSink, EVENT_PATCH_RETRY_AFTER,
    EVENT_VOICE_CATEGORY_KEY, PREFLIGHT_DENIED_BACKOFF,
};
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use twilight_model::guild::Permissions;
use two_bot_discord::voice_rooms::RoomHttpError;

const EVENT_GUILD: u64 = 900;
const EVENT_CATEGORY: u64 = 700;
const MOVED_CATEGORY: u64 = 701;
const EVENT_ID: u64 = 800;

fn lookup(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

fn full_grant() -> Permissions {
    Permissions::VIEW_CHANNEL
        | Permissions::CONNECT
        | Permissions::MANAGE_CHANNELS
        | Permissions::MOVE_MEMBERS
}

fn snapshot(category: u64, permissions: Option<Permissions>) -> CategorySnapshot {
    CategorySnapshot {
        id: category,
        bot_permissions: permissions,
        everyone_can_view: true,
    }
}

/// Strict scripted discovery double: any reader call past the end of the
/// script panics, so over-calling fails loudly instead of silently passing.
struct ScriptReader {
    calls: Mutex<Vec<(u64, u64)>>,
    script: Mutex<VecDeque<Result<CategorySnapshot, RoomHttpError>>>,
}

impl ScriptReader {
    fn new(script: Vec<Result<CategorySnapshot, RoomHttpError>>) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            script: Mutex::new(script.into()),
        }
    }

    fn calls(&self) -> Vec<(u64, u64)> {
        self.calls.lock().unwrap().clone()
    }
}

impl CategoryReader for ScriptReader {
    async fn category_grant(
        &self,
        guild: u64,
        category: u64,
    ) -> Result<CategorySnapshot, RoomHttpError> {
        self.calls.lock().unwrap().push((guild, category));
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| panic!("category reader called more times than scripted"))
    }
}

#[derive(Default)]
struct ScriptSink {
    alerts: Mutex<Vec<StaffAlert>>,
}

impl ScriptSink {
    fn alerts(&self) -> Vec<(EventVoiceTbdReason, Option<u64>)> {
        self.alerts
            .lock()
            .unwrap()
            .iter()
            .map(|alert| (alert.reason, alert.category))
            .collect()
    }
}

impl StaffAlertSink for ScriptSink {
    fn send_staff_alert(&self, alert: StaffAlert) {
        self.alerts.lock().unwrap().push(alert);
    }
}

/// Strict scripted PATCH double: same fail-loud rule as [`ScriptReader`].
struct ScriptPatcher {
    calls: Mutex<Vec<(u64, u64)>>,
    script: Mutex<VecDeque<Result<(), RoomHttpError>>>,
}

impl ScriptPatcher {
    fn new(script: Vec<Result<(), RoomHttpError>>) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            script: Mutex::new(script.into()),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

impl EventStatusPatcher for ScriptPatcher {
    async fn patch_event_completed(&self, guild: u64, event: u64) -> Result<(), RoomHttpError> {
        self.calls.lock().unwrap().push((guild, event));
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| panic!("event patcher called more times than scripted"))
    }
}

fn sample(text: &str, series: &str) -> Option<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix(series)?.strip_prefix(' ')?.parse().ok())
}

fn tick_series_total(outcome: &str) -> u64 {
    let text = two_bot_core::metrics::global().render(None);
    let series = format!("two_bot_event_voice_tick_total{{outcome=\"{outcome}\"}}");
    sample(&text, &series).unwrap_or(0)
}

#[test]
fn loader_reads_exactly_the_category_key() {
    assert_eq!(EVENT_VOICE_CATEGORY_KEY, "TWO_EVENT_VOICE_CATEGORY_ID");
    let asked = std::cell::RefCell::new(Vec::new());
    let configured = configured_event_voice_category(|key| {
        asked.borrow_mut().push(key.to_owned());
        None
    });
    assert_eq!(configured, None);
    assert_eq!(
        asked.into_inner(),
        vec![EVENT_VOICE_CATEGORY_KEY.to_owned()],
        "the loader must consult exactly the one catalogued key, once"
    );
}

#[test]
fn unset_category_disables_the_feature() {
    let values = lookup(&[]);
    assert_eq!(
        configured_event_voice_category(|key| values.get(key).cloned()),
        None
    );
}

#[test]
fn blank_values_count_as_unset() {
    for blank in ["", " ", "  \t "] {
        let values = lookup(&[(EVENT_VOICE_CATEGORY_KEY, blank)]);
        assert_eq!(
            configured_event_voice_category(|key| values.get(key).cloned()),
            None,
            "{blank:?} must count as unset"
        );
    }
}

#[test]
fn valid_id_parses() {
    for (raw, id) in [
        ("700", 700),
        ("987654321098765432", 987_654_321_098_765_432),
        ("  700  ", 700),
    ] {
        let values = lookup(&[(EVENT_VOICE_CATEGORY_KEY, raw)]);
        assert_eq!(
            configured_event_voice_category(|key| values.get(key).cloned()),
            Some(id),
            "{raw:?} must parse"
        );
    }
}

#[test]
fn unparsable_zero_and_noncanonical_ids_fail_closed() {
    for bad in ["abc", "0", "00", "01", "-5", "7.5", "12,34", "700x", "x700"] {
        let values = lookup(&[(EVENT_VOICE_CATEGORY_KEY, bad)]);
        assert_eq!(
            configured_event_voice_category(|key| values.get(key).cloned()),
            None,
            "{bad:?} must fail closed to inactive, never panic or guess"
        );
    }
}

#[test]
fn tbd_reasons_match_the_shared_website_vocabulary() {
    assert_eq!(EventVoiceTbdReason::OpensSoon.as_str(), "opens_soon");
    assert_eq!(EventVoiceTbdReason::RateLimited.as_str(), "rate_limited");
    assert_eq!(EventVoiceTbdReason::PermDenied.as_str(), "perm_denied");
    assert_eq!(EventVoiceTbdReason::NoCategory.as_str(), "no_category");
    assert_eq!(EventVoiceTbdReason::Unavailable.as_str(), "unavailable");
}

#[tokio::test]
async fn unset_category_stays_tbd_without_touching_discord() {
    let reader = ScriptReader::new(vec![]);
    let alerts = ScriptSink::default();
    let mut gate = EventVoiceCategoryGate::new(None);
    assert_eq!(gate.configured(), None);
    assert_eq!(gate.effective_category(), None);
    assert_eq!(gate.create_parent(), None);
    assert!(!gate.members_only_link());

    let now = Instant::now();
    for _ in 0..3 {
        assert_eq!(
            gate.preflight(&reader, &alerts, EVENT_GUILD, now).await,
            PreflightDecision::Waiting {
                reason: EventVoiceTbdReason::NoCategory
            }
        );
    }
    assert!(
        reader.calls().is_empty(),
        "unset config needs no discovery: zero Discord reads"
    );
}

#[tokio::test]
async fn unset_category_records_a_per_tick_no_category_counter() {
    let reader = ScriptReader::new(vec![]);
    let alerts = ScriptSink::default();
    let mut gate = EventVoiceCategoryGate::new(None);
    let before = tick_series_total("no_category");
    let now = Instant::now();
    for _ in 0..2 {
        gate.preflight(&reader, &alerts, EVENT_GUILD, now).await;
    }
    assert!(
        tick_series_total("no_category") - before >= 2,
        "every inactive tick records the no_category counter"
    );
}

#[tokio::test]
async fn unset_category_alerts_staff_exactly_once_per_gap() {
    let reader = ScriptReader::new(vec![]);
    let alerts = ScriptSink::default();
    let mut gate = EventVoiceCategoryGate::new(None);
    let now = Instant::now();
    for _ in 0..3 {
        gate.preflight(&reader, &alerts, EVENT_GUILD, now).await;
    }
    assert_eq!(
        alerts.alerts(),
        vec![(EventVoiceTbdReason::NoCategory, None)],
        "one staff alert per config gap, never per-tick spam"
    );
}

#[tokio::test]
async fn reconfiguring_to_the_same_value_does_not_respam() {
    let reader = ScriptReader::new(vec![]);
    let alerts = ScriptSink::default();
    let mut gate = EventVoiceCategoryGate::new(None);
    let now = Instant::now();
    gate.preflight(&reader, &alerts, EVENT_GUILD, now).await;
    // Production re-reads env every tick: an unchanged value must keep the
    // latch, or the "one alert" guarantee degrades into per-tick spam.
    gate.reconfigure(None);
    gate.preflight(&reader, &alerts, EVENT_GUILD, now).await;
    assert_eq!(alerts.alerts().len(), 1);
}

#[tokio::test]
async fn reconfiguring_to_a_new_category_clears_backoff_and_cache() {
    let reader = ScriptReader::new(vec![
        Err(RoomHttpError::AccessDenied),
        Ok(snapshot(EVENT_CATEGORY, Some(full_grant()))),
    ]);
    let alerts = ScriptSink::default();
    let mut gate = EventVoiceCategoryGate::new(Some(EVENT_CATEGORY));
    let now = Instant::now();
    assert_eq!(
        gate.preflight(&reader, &alerts, EVENT_GUILD, now).await,
        PreflightDecision::Waiting {
            reason: EventVoiceTbdReason::PermDenied
        }
    );
    gate.note_mod_move(MOVED_CATEGORY, false);
    assert!(gate.members_only_link());

    // A changed id is a fresh episode: backoff, the cached move, and the
    // alert latch reset, and the next tick preflights immediately.
    gate.reconfigure(Some(MOVED_CATEGORY));
    assert_eq!(gate.configured(), Some(MOVED_CATEGORY));
    assert_eq!(gate.effective_category(), Some(MOVED_CATEGORY));
    assert!(!gate.members_only_link());
    assert_eq!(
        gate.preflight(&reader, &alerts, EVENT_GUILD, now).await,
        PreflightDecision::Ready {
            category: MOVED_CATEGORY
        }
    );
    assert_eq!(
        reader.calls(),
        vec![(EVENT_GUILD, EVENT_CATEGORY), (EVENT_GUILD, MOVED_CATEGORY)]
    );
}

#[tokio::test]
async fn alert_latch_rearms_after_recovery() {
    let reader = ScriptReader::new(vec![
        Err(RoomHttpError::AccessDenied),
        Ok(snapshot(EVENT_CATEGORY, Some(full_grant()))),
        Err(RoomHttpError::AccessDenied),
    ]);
    let alerts = ScriptSink::default();
    let mut gate = EventVoiceCategoryGate::new(Some(EVENT_CATEGORY));
    let denied = Instant::now();
    gate.preflight(&reader, &alerts, EVENT_GUILD, denied).await;
    gate.preflight(
        &reader,
        &alerts,
        EVENT_GUILD,
        denied + PREFLIGHT_DENIED_BACKOFF,
    )
    .await;
    gate.preflight(
        &reader,
        &alerts,
        EVENT_GUILD,
        denied + PREFLIGHT_DENIED_BACKOFF + Duration::from_secs(1),
    )
    .await;
    assert_eq!(
        alerts.alerts(),
        vec![
            (EventVoiceTbdReason::PermDenied, Some(EVENT_CATEGORY)),
            (EventVoiceTbdReason::PermDenied, Some(EVENT_CATEGORY)),
        ],
        "a recovered-then-denied episode alerts once more"
    );
}

#[tokio::test]
async fn deleted_category_matches_unset_inactivity() {
    let reader = ScriptReader::new(vec![Err(RoomHttpError::NotFound)]);
    let alerts = ScriptSink::default();
    let mut gate = EventVoiceCategoryGate::new(Some(EVENT_CATEGORY));
    let now = Instant::now();
    for _ in 0..3 {
        assert_eq!(
            gate.preflight(&reader, &alerts, EVENT_GUILD, now).await,
            PreflightDecision::Waiting {
                reason: EventVoiceTbdReason::NoCategory
            },
            "a deleted category is TBD no_category, never Ready, so the \
             ticker (which only creates on Ready) attempts no create"
        );
    }
    assert_eq!(
        alerts.alerts(),
        vec![(EventVoiceTbdReason::NoCategory, Some(EVENT_CATEGORY))],
        "one alert naming the deleted id"
    );
}

#[tokio::test]
async fn deleted_category_backoff_limits_discovery_reads() {
    let reader = ScriptReader::new(vec![
        Err(RoomHttpError::NotFound),
        Err(RoomHttpError::NotFound),
    ]);
    let alerts = ScriptSink::default();
    let mut gate = EventVoiceCategoryGate::new(Some(EVENT_CATEGORY));
    let start = Instant::now();
    gate.preflight(&reader, &alerts, EVENT_GUILD, start).await;
    gate.preflight(&reader, &alerts, EVENT_GUILD, start).await;
    assert_eq!(reader.calls().len(), 1, "backoff skips the re-read");
    gate.preflight(
        &reader,
        &alerts,
        EVENT_GUILD,
        start + PREFLIGHT_DENIED_BACKOFF,
    )
    .await;
    assert_eq!(reader.calls().len(), 2, "backoff expiry retries");
}

#[tokio::test]
async fn category_403_stays_tbd_perm_denied_and_survives_the_tick() {
    let reader = ScriptReader::new(vec![Err(RoomHttpError::AccessDenied)]);
    let alerts = ScriptSink::default();
    let mut gate = EventVoiceCategoryGate::new(Some(EVENT_CATEGORY));
    // Returning at all is the assertion: the 403 never propagates, never
    // panics, and never crashes the tick.
    assert_eq!(
        gate.preflight(&reader, &alerts, EVENT_GUILD, Instant::now())
            .await,
        PreflightDecision::Waiting {
            reason: EventVoiceTbdReason::PermDenied
        }
    );
}

#[tokio::test]
async fn category_403_records_counter_and_alerts_once_per_episode() {
    let reader = ScriptReader::new(vec![Err(RoomHttpError::AccessDenied)]);
    let alerts = ScriptSink::default();
    let mut gate = EventVoiceCategoryGate::new(Some(EVENT_CATEGORY));
    let before = tick_series_total("perm_denied");
    let now = Instant::now();
    for _ in 0..3 {
        gate.preflight(&reader, &alerts, EVENT_GUILD, now).await;
    }
    assert!(
        tick_series_total("perm_denied") - before >= 3,
        "every denied tick records the counter, including backoff-skipped ticks"
    );
    assert_eq!(
        alerts.alerts(),
        vec![(EventVoiceTbdReason::PermDenied, Some(EVENT_CATEGORY))],
        "one alert per denied episode"
    );
}

#[tokio::test]
async fn insufficient_category_grant_is_perm_denied_and_rechecked_every_tick() {
    let weak = full_grant() - Permissions::MANAGE_CHANNELS;
    assert!(weak.contains(Permissions::VIEW_CHANNEL));
    let reader = ScriptReader::new(vec![
        Ok(snapshot(EVENT_CATEGORY, Some(weak))),
        Ok(snapshot(EVENT_CATEGORY, Some(weak))),
        Ok(snapshot(EVENT_CATEGORY, Some(weak))),
    ]);
    let alerts = ScriptSink::default();
    let mut gate = EventVoiceCategoryGate::new(Some(EVENT_CATEGORY));
    let now = Instant::now();
    for _ in 0..3 {
        assert_eq!(
            gate.preflight(&reader, &alerts, EVENT_GUILD, now).await,
            PreflightDecision::Waiting {
                reason: EventVoiceTbdReason::PermDenied
            },
            "View without Manage Channels cannot host event creates"
        );
    }
    assert_eq!(
        reader.calls().len(),
        3,
        "a cheap steady-state denial is preflighted every tick, so a mod \
         fixing the overwrite heals on the next tick instead of after backoff"
    );
}

#[tokio::test]
async fn incomplete_permission_snapshot_fails_closed() {
    let reader = ScriptReader::new(vec![Ok(snapshot(EVENT_CATEGORY, None))]);
    let alerts = ScriptSink::default();
    let mut gate = EventVoiceCategoryGate::new(Some(EVENT_CATEGORY));
    assert_eq!(
        gate.preflight(&reader, &alerts, EVENT_GUILD, Instant::now())
            .await,
        PreflightDecision::Waiting {
            reason: EventVoiceTbdReason::PermDenied
        },
        "an incomplete snapshot must fail closed, never assume the grant"
    );
}

#[tokio::test]
async fn discovery_429_stays_tbd_rate_limited_without_an_alert() {
    let reader = ScriptReader::new(vec![Err(RoomHttpError::RateLimited {
        retry_after_ms: 5_000,
        global: false,
    })]);
    let alerts = ScriptSink::default();
    let mut gate = EventVoiceCategoryGate::new(Some(EVENT_CATEGORY));
    let before = tick_series_total("rate_limited");
    let now = Instant::now();
    assert_eq!(
        gate.preflight(&reader, &alerts, EVENT_GUILD, now).await,
        PreflightDecision::Waiting {
            reason: EventVoiceTbdReason::RateLimited
        }
    );
    gate.preflight(&reader, &alerts, EVENT_GUILD, now).await;
    assert_eq!(reader.calls().len(), 1, "backoff skips the re-read");
    assert!(
        tick_series_total("rate_limited") - before >= 2,
        "transient TBDs still record their per-tick counter"
    );
    assert!(
        alerts.alerts().is_empty(),
        "transient rate limits never page staff"
    );
}

#[tokio::test]
async fn discovery_transport_failure_stays_tbd_unavailable_without_an_alert() {
    let reader = ScriptReader::new(vec![Err(RoomHttpError::UnknownOutcome)]);
    let alerts = ScriptSink::default();
    let mut gate = EventVoiceCategoryGate::new(Some(EVENT_CATEGORY));
    assert_eq!(
        gate.preflight(&reader, &alerts, EVENT_GUILD, Instant::now())
            .await,
        PreflightDecision::Waiting {
            reason: EventVoiceTbdReason::Unavailable
        }
    );
    assert!(
        alerts.alerts().is_empty(),
        "transient transport failures never page staff"
    );
}

#[test]
fn mod_category_move_is_accepted_and_cached() {
    let mut gate = EventVoiceCategoryGate::new(Some(EVENT_CATEGORY));
    assert_eq!(gate.effective_category(), Some(EVENT_CATEGORY));
    gate.note_mod_move(MOVED_CATEGORY, true);
    assert_eq!(
        gate.effective_category(),
        Some(MOVED_CATEGORY),
        "a mod move is accepted: the cached parent follows the channel"
    );
    assert!(!gate.members_only_link());
    assert_eq!(
        gate.create_parent(),
        Some(EVENT_CATEGORY),
        "moves never change the create target: future creates still go to \
         the configured category"
    );
}

#[test]
fn move_into_private_category_flips_members_only_link() {
    let mut gate = EventVoiceCategoryGate::new(Some(EVENT_CATEGORY));
    gate.note_mod_move(MOVED_CATEGORY, false);
    assert_eq!(gate.effective_category(), Some(MOVED_CATEGORY));
    assert!(
        gate.members_only_link(),
        "a move into a private category flips the website link to members-only"
    );
    gate.note_mod_move(EVENT_CATEGORY, true);
    assert!(
        !gate.members_only_link(),
        "moving back somewhere public clears the flag"
    );
}

#[tokio::test]
async fn ready_create_target_ignores_mod_moves() {
    let reader = ScriptReader::new(vec![Ok(snapshot(EVENT_CATEGORY, Some(full_grant())))]);
    let alerts = ScriptSink::default();
    let mut gate = EventVoiceCategoryGate::new(Some(EVENT_CATEGORY));
    gate.note_mod_move(MOVED_CATEGORY, false);
    assert_eq!(
        gate.preflight(&reader, &alerts, EVENT_GUILD, Instant::now())
            .await,
        PreflightDecision::Ready {
            category: EVENT_CATEGORY
        },
        "preflight validates the configured category: the moved live \
         channel already exists, and the next create still targets config"
    );
    assert_eq!(reader.calls(), vec![(EVENT_GUILD, EVENT_CATEGORY)]);
}

#[tokio::test]
async fn event_patch_403_queues_a_retry_without_failing() {
    let patcher = ScriptPatcher::new(vec![Err(RoomHttpError::AccessDenied)]);
    let mut pending = PendingCompletedPatch::for_event(EVENT_GUILD, EVENT_ID);
    assert_eq!(pending.event(), EVENT_ID);
    assert!(!pending.is_pending());
    // Returning QueuedForRetry (instead of propagating the 403) is what
    // keeps the caller's already-completed local handoff standing: the
    // after-party is never blocked on the event PATCH.
    assert_eq!(
        pending.ensure_published(&patcher, Instant::now()).await,
        PatchOutcome::QueuedForRetry
    );
    assert!(pending.is_pending());
    assert_eq!(patcher.call_count(), 1);
}

#[tokio::test]
async fn queued_patch_retries_after_backoff_and_publishes() {
    let patcher = ScriptPatcher::new(vec![Err(RoomHttpError::AccessDenied), Ok(())]);
    let mut pending = PendingCompletedPatch::for_event(EVENT_GUILD, EVENT_ID);
    let start = Instant::now();
    assert_eq!(
        pending.ensure_published(&patcher, start).await,
        PatchOutcome::QueuedForRetry
    );
    assert_eq!(
        pending.ensure_published(&patcher, start).await,
        PatchOutcome::QueuedForRetry
    );
    assert_eq!(patcher.call_count(), 1, "backoff skips the retry");
    assert_eq!(
        pending
            .ensure_published(&patcher, start + EVENT_PATCH_RETRY_AFTER)
            .await,
        PatchOutcome::Published
    );
    assert!(!pending.is_pending());
    assert_eq!(patcher.call_count(), 2, "backoff expiry retries the PATCH");
    assert_eq!(
        pending
            .ensure_published(&patcher, start + EVENT_PATCH_RETRY_AFTER)
            .await,
        PatchOutcome::Published
    );
    assert_eq!(
        patcher.call_count(),
        2,
        "a published patch makes no further wire calls"
    );
}

#[tokio::test]
async fn queued_patch_survives_repeated_403s() {
    let patcher = ScriptPatcher::new(vec![
        Err(RoomHttpError::AccessDenied),
        Err(RoomHttpError::AccessDenied),
    ]);
    let mut pending = PendingCompletedPatch::for_event(EVENT_GUILD, EVENT_ID);
    let start = Instant::now();
    pending.ensure_published(&patcher, start).await;
    assert_eq!(
        pending
            .ensure_published(&patcher, start + EVENT_PATCH_RETRY_AFTER)
            .await,
        PatchOutcome::QueuedForRetry
    );
    assert!(pending.is_pending());
    assert_eq!(patcher.call_count(), 2);
}

#[tokio::test]
async fn patch_429_also_queues_a_retry() {
    let patcher = ScriptPatcher::new(vec![Err(RoomHttpError::RateLimited {
        retry_after_ms: 5_000,
        global: false,
    })]);
    let mut pending = PendingCompletedPatch::for_event(EVENT_GUILD, EVENT_ID);
    assert_eq!(
        pending.ensure_published(&patcher, Instant::now()).await,
        PatchOutcome::QueuedForRetry
    );
    assert!(pending.is_pending());
}

#[tokio::test]
async fn patch_to_a_deleted_event_resolves_instead_of_looping() {
    let patcher = ScriptPatcher::new(vec![Err(RoomHttpError::NotFound)]);
    let mut pending = PendingCompletedPatch::for_event(EVENT_GUILD, EVENT_ID);
    assert_eq!(
        pending.ensure_published(&patcher, Instant::now()).await,
        PatchOutcome::Published,
        "COMPLETED on a deleted event is moot: resolve instead of 404ing forever"
    );
    assert!(!pending.is_pending());
}

#[tokio::test]
async fn first_try_patch_success_publishes_immediately() {
    let patcher = ScriptPatcher::new(vec![Ok(())]);
    let mut pending = PendingCompletedPatch::for_event(EVENT_GUILD, EVENT_ID);
    assert_eq!(
        pending.ensure_published(&patcher, Instant::now()).await,
        PatchOutcome::Published
    );
    assert!(!pending.is_pending());
    assert_eq!(patcher.call_count(), 1);
}

#[test]
fn control_room_http_error_vocabulary() {
    // Today's seams only: the discovery/PATCH contract speaks RoomHttpError,
    // so the three outcomes these tests script must exist with these shapes.
    let denied = RoomHttpError::AccessDenied;
    let missing = RoomHttpError::NotFound;
    let limited = RoomHttpError::RateLimited {
        retry_after_ms: 1,
        global: false,
    };
    assert_eq!(format!("{denied:?}"), "AccessDenied");
    assert_eq!(format!("{missing:?}"), "NotFound");
    assert!(format!("{limited:?}").contains("RateLimited"));
    let again = denied;
    assert_eq!(denied, again);
}
