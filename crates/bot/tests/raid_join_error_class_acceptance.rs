//! Acceptance: raid/join-risk errors carry `error_class` and queue drops count.
//!
//! Paired acceptance for the raid observability slice: the fifteen `error!`
//! sites in `raid_runtime.rs` and `join_risk_runtime.rs` carry no `error_class`
//! while the ticket and boot paths do, queue-`Full` is warn-only with no
//! count, and queue-`Closed` is fully silent. The builder tags every site,
//! counts both drop reasons per runtime, and renders the counters on the
//! existing metrics surface.
//!
//! # Protected file
//!
//! The builder must not edit this file. If a pin is wrong, return the
//! correction to the test author; the fix lands here, never in the build.
//!
//! # Pinned seam (public API only, no builder seams)
//!
//! Every behavioral test drives the runtimes through signatures that exist
//! today, so this file compiles on the current head and fails on assertions
//! (behavioral RED), never on unresolved imports:
//!
//! ```text
//! two_bot::raid_runtime::{start, StoreSettings, SettingsSource, CheckedSettingsSource}
//! two_bot::join_risk_runtime::{start, StoreSettings, SettingsSource}
//! two_bot_core::metrics::{global, Metrics::render}
//! two_bot_discord::{ActionExecutor::with_proxy, JoinObserver}
//! ```
//!
//! The GREEN contract the builder implements without touching this file:
//!
//! - `start()` keeps its signature and wires drop recording into the
//!   process-global metrics registry, so `global().render(None)` advances.
//! - Each site below emits `error_class = "<stem>"` (or `msg = "<stem>"`
//!   with bounded labels, per the internal-action convention); either shape
//!   passes. Exact accepted stems are the test names plus the two new
//!   queue-closed stems.
//! - `Full` advances `{metric}{reason="queue_full"}` per dropped join;
//!   `Closed` logs an error and advances `{metric}{reason="queue_closed"}`.
//! - New series render as `two_bot_raid_dropped_joins_total{reason=...}`
//!   and `two_bot_join_risk_dropped_joins_total{reason=...}`.
//!
//! # What is pinned
//!
//! - Behavioral (log capture + global exposition deltas): every site that is
//!   reachable offline — raid tuning refusal, alert evidence, undeliverable,
//!   post failure, worker panic; join-risk tuning refusal, worker panic,
//!   store failure; and both Full/Closed counters per runtime.
//! - Source pins (`include_str!`, exact `error_class`/`msg` token): the seven
//!   sites no offline behavior can reach — the two mention-refusal guards
//!   (both `staff_message` constructors hardcode `MentionPolicy::None`), the
//!   policy-rejection guard (it re-validates an already-valid `RaidTuning`),
//!   the missing-account-age guard (`member_id` is a `u64`, so its string
//!   always parses), and the three claim-path sites (evidence,
//!   undeliverable, post failure), which need a persisted store claim and
//!   this suite never touches a database.
//!
//! # Running
//!
//! ```text
//! cargo test -p two-bot --locked --test raid_join_error_class_acceptance
//! ```
//!
//! Offline: the only sockets are a loopback mock Discord double and refused
//! loopback database dials (no listener, no database, no external network).

#[path = "../../core/tests/support/tracing_capture.rs"]
mod tracing_capture;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Router;
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tokio::net::TcpListener;
use two_bot::join_risk_runtime::{
    self, JoinRiskSettings, JoinRiskTuningRefusal, SettingsSource as RiskSettingsSource,
    StoreSettings as RiskStoreSettings,
};
use two_bot::raid_runtime::{
    self, CheckedSettingsSource, RaidSettings, SettingsSource as RaidSettingsSource,
    StoreSettings as RaidStoreSettings,
};
use two_bot_core::join_risk_store::JoinRiskStore;
use two_bot_core::metrics;
use two_bot_core::RaidTuning;
use two_bot_cutover::parse::date_to_snowflake;
use two_bot_discord::{ActionExecutor, JoinObservation, JoinObserver};

const GUILD: u64 = 22;
const CHANNEL: &str = "12";
const NOW: i64 = 1_790_780_400_000;
const RAID_DROPPED: &str = "two_bot_raid_dropped_joins_total";
const RISK_DROPPED: &str = "two_bot_join_risk_dropped_joins_total";
/// Must match `raid_runtime::QUEUE_CAPACITY` (both runtimes share the const;
/// it is `pub(crate)`, so this external suite cites the value — a capacity
/// change fails the Full tests loudly as a delta mismatch, not silently).
const QUEUE_FILL: u64 = 2048;

const RAID_SOURCE: &str = include_str!("../src/raid_runtime.rs");
const RISK_SOURCE: &str = include_str!("../src/join_risk_runtime.rs");

/// One observed site: either log shape passes, mirroring the contract that
/// the builder may use `error_class` or `msg` + bounded labels per site.
#[track_caller]
fn assert_tagged(logs: &str, stem: &str) {
    let error_class = format!("error_class=\"{stem}\"");
    let msg = format!("msg=\"{stem}\"");
    assert!(
        logs.lines().any(|line| {
            line.starts_with("ERROR ") && (line.contains(&error_class) || line.contains(&msg))
        }),
        "site {stem} must log error_class or msg at ERROR level; delta:\n{logs}"
    );
}

/// Source pin for sites no offline behavior can reach: the exact token must
/// appear in the runtime source (either log shape, either rustfmt spacing).
#[track_caller]
fn assert_source_tagged(source: &str, file: &str, stem: &str) {
    for shape in [
        format!("error_class = \"{stem}\""),
        format!("error_class=\"{stem}\""),
        format!("msg = \"{stem}\""),
        format!("msg=\"{stem}\""),
    ] {
        if source.contains(&shape) {
            return;
        }
    }
    panic!("{file} site {stem} must carry error_class or msg");
}

fn render() -> String {
    metrics::global().render(None)
}

fn series_value(render: &str, metric: &str, reason: &str) -> Option<u64> {
    let prefix = format!("{metric}{{reason=\"{reason}\"}} ");
    render
        .lines()
        .find_map(|line| line.strip_prefix(&prefix)?.trim().parse().ok())
}

#[track_caller]
fn assert_delta(site: &str, before: Option<u64>, after: Option<u64>, expected: u64) {
    match (before, after) {
        (Some(b), Some(a)) => assert_eq!(
            a - b,
            expected,
            "{site}: dropped counter must advance by {expected}"
        ),
        _ => panic!("{site}: series missing from exposition (before={before:?}, after={after:?})"),
    }
}

fn ensure_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

// ---------------------------------------------------------------------------
// Loopback mock Discord double (this suite's own fixture, no sharing).
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct RestRequest {
    method: String,
    path: String,
}

struct ScriptedResponse(Response);

impl ScriptedResponse {
    fn status(status: u16) -> Self {
        Self(
            StatusCode::from_u16(status)
                .expect("valid mock status")
                .into_response(),
        )
    }

    fn json(status: u16, body: serde_json::Value) -> Self {
        Self(
            (
                StatusCode::from_u16(status).expect("valid mock status"),
                axum::Json(body),
            )
                .into_response(),
        )
    }
}

struct MockRest {
    origin: String,
    recorded: Arc<Mutex<Vec<RestRequest>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl MockRest {
    async fn with_responder(
        responder: impl Fn(&RestRequest) -> ScriptedResponse + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind rest");
        let origin = format!("http://{}", listener.local_addr().expect("rest addr"));
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let requests = Arc::clone(&recorded);
        let responder = Arc::new(responder);
        let app = Router::new().fallback(move |request: Request| {
            let requests = Arc::clone(&requests);
            let responder = Arc::clone(&responder);
            async move {
                let request = RestRequest {
                    method: request.method().to_string(),
                    path: request.uri().path().to_owned(),
                };
                requests.lock().expect("recorded").push(request.clone());
                responder(&request).0
            }
        });
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve mock rest");
        });
        Self {
            origin,
            recorded,
            handle,
        }
    }

    fn origin(&self) -> String {
        self.origin.clone()
    }

    fn requests(&self) -> Vec<RestRequest> {
        self.recorded.lock().expect("recorded").clone()
    }

    async fn shutdown(self) {
        self.handle.abort();
    }
}

impl Drop for MockRest {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

#[derive(Clone, Copy)]
enum Fixture {
    /// The bot may View and Send in the staff channel.
    Open,
    /// The bot may View but not Send.
    ViewOnly,
    /// Permissions are fine but Discord rejects the post.
    PostRejected,
}

async fn discord(fixture: Fixture) -> MockRest {
    MockRest::with_responder(move |request: &RestRequest| {
        let path = request.path.strip_prefix("/api/v10").unwrap();
        match (request.method.as_str(), path) {
            ("GET", "/users/@me") => {
                ScriptedResponse::json(200, json!({"id":"999","bot":true,"username":"bot"}))
            }
            ("GET", "/guilds/22") => {
                ScriptedResponse::json(200, json!({"id":"22","owner_id":"888"}))
            }
            ("GET", "/guilds/22/roles") => {
                let bits = if matches!(fixture, Fixture::ViewOnly) {
                    "1024"
                } else {
                    "3072"
                };
                ScriptedResponse::json(200, json!([{"id":"22","permissions":bits}]))
            }
            ("GET", "/guilds/22/members/999") => {
                ScriptedResponse::json(200, json!({"user":{"id":"999"},"roles":[]}))
            }
            ("GET", "/channels/12") => ScriptedResponse::json(
                200,
                json!({"id":"12","guild_id":"22","type":0,"permission_overwrites":[]}),
            ),
            ("POST", "/channels/12/messages") => {
                if matches!(fixture, Fixture::PostRejected) {
                    ScriptedResponse::status(403)
                } else {
                    ScriptedResponse::json(201, json!({"id":"99"}))
                }
            }
            _ => panic!("unexpected mock Discord route: {} {}", request.method, path),
        }
    })
    .await
}

fn executor(mock: &MockRest) -> ActionExecutor {
    ensure_crypto_provider();
    ActionExecutor::with_proxy("raid-test-only-token".into(), Some(mock.origin())).unwrap()
}

fn posts(mock: &MockRest) -> Vec<RestRequest> {
    mock.requests()
        .into_iter()
        .filter(|request| request.method == "POST")
        .collect()
}

/// A pool that can never connect: nothing listens on loopback port 1, so the
/// first acquire fails fast with connection-refused and no database exists.
fn dead_pool() -> PgPool {
    PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(2))
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("lazy pool never fails to construct")
}

fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

fn join(guild_id: u64, member_id: u64, at_ms: i64) -> JoinObservation {
    JoinObservation {
        guild_id,
        member_id,
        source: "unknown".into(),
        joined_at_ms: at_ms,
    }
}

/// A member whose snowflake decodes to a young account (joins still fail
/// before any claim here; the helper keeps the age realistic).
fn risk_join(member_offset: u64) -> JoinObservation {
    let member_id: u64 = date_to_snowflake((NOW + member_offset as i64) as u64)
        .parse()
        .expect("snowflakes are numeric");
    join(GUILD, member_id, NOW + member_offset as i64)
}

fn settings(threshold: f64, channel: Option<&str>) -> RaidSettings {
    RaidSettings {
        tuning: RaidTuning::new(60.0, threshold).unwrap(),
        staff_channel: channel.map(str::to_owned),
    }
}

fn risk_settings(threshold: f64, channel: Option<&str>) -> JoinRiskSettings {
    JoinRiskSettings {
        tuning: RaidTuning::new(60.0, threshold).unwrap(),
        bulk_join_window_until_ms: None,
        staff_channel: channel.map(str::to_owned),
    }
}

/// Serves the given settings one observation at a time, repeating the last.
struct Scripted(Vec<RaidSettings>);

impl RaidSettingsSource for Scripted {
    async fn current(&mut self) -> RaidSettings {
        if self.0.len() > 1 {
            self.0.remove(0)
        } else {
            self.0[0].clone()
        }
    }
}

/// Panics on its first call, then behaves.
struct PanicsOnce {
    panicked: bool,
    then: RaidSettings,
}

impl RaidSettingsSource for PanicsOnce {
    async fn current(&mut self) -> RaidSettings {
        if !self.panicked {
            self.panicked = true;
            panic!("settings source failure (test)");
        }
        self.then.clone()
    }
}

/// Stalls the worker inside its first settings read: the flag proves the
/// worker holds one join and the channel is empty, so the fill count below
/// is exact rather than racy.
struct PendingRaid {
    entered: Arc<AtomicBool>,
}

impl RaidSettingsSource for PendingRaid {
    async fn current(&mut self) -> RaidSettings {
        self.entered.store(true, Ordering::SeqCst);
        std::future::pending::<()>().await;
        unreachable!("stalled settings never resolve");
    }
}

/// Serves the given settings one observation at a time, repeating the last.
struct ScriptedRisk(Vec<JoinRiskSettings>);

impl RiskSettingsSource for ScriptedRisk {
    async fn current(&mut self) -> Result<JoinRiskSettings, JoinRiskTuningRefusal> {
        Ok(if self.0.len() > 1 {
            self.0.remove(0)
        } else {
            self.0[0].clone()
        })
    }
}

/// Panics on its first call, then behaves.
struct PanicsOnceRisk {
    panicked: bool,
    then: JoinRiskSettings,
}

impl RiskSettingsSource for PanicsOnceRisk {
    async fn current(&mut self) -> Result<JoinRiskSettings, JoinRiskTuningRefusal> {
        if !self.panicked {
            self.panicked = true;
            panic!("settings source failure (test)");
        }
        Ok(self.then.clone())
    }
}

/// Stalls the worker inside its first settings read (see [`PendingRaid`]).
struct PendingRisk {
    entered: Arc<AtomicBool>,
}

impl RiskSettingsSource for PendingRisk {
    async fn current(&mut self) -> Result<JoinRiskSettings, JoinRiskTuningRefusal> {
        self.entered.store(true, Ordering::SeqCst);
        std::future::pending::<()>().await;
        unreachable!("stalled settings never resolve");
    }
}

async fn wait_for_stall(entered: &AtomicBool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !entered.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("worker must stall in its settings read");
}

/// Feed `joins` through a started raid runtime, then drain it
/// deterministically: dropping the only sender lets the worker finish its
/// queue and exit.
async fn run_raid(
    source: impl RaidSettingsSource,
    mock: &MockRest,
    joins: impl IntoIterator<Item = JoinObservation>,
) {
    let (runtime, worker) = raid_runtime::start(GUILD, source, executor(mock));
    for observation in joins {
        runtime.observe_join(observation);
    }
    drop(runtime);
    worker.await.expect("worker ends cleanly");
}

fn burst(count: u64, step_ms: i64) -> Vec<JoinObservation> {
    (0..count)
        .map(|n| join(GUILD, 1000 + n, NOW + n as i64 * step_ms))
        .collect()
}

// ---------------------------------------------------------------------------
// Raid `error_class` sites.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn raid_tuning_refusal_carries_error_class() {
    let capture = tracing_capture::global();
    let mut source = RaidStoreSettings::new(
        dead_pool(),
        GUILD,
        vars(&[("TWO_RAID_JOIN_THRESHOLD", "abc")]),
    );
    let start = capture.text().len();
    let refusal = source
        .try_current()
        .await
        .expect_err("unusable threshold refuses");
    assert_eq!(refusal.key(), "TWO_RAID_JOIN_THRESHOLD");
    assert_tagged(&capture.text().split_off(start), "raid_tuning_refused");
}

#[tokio::test]
async fn raid_alert_evidence_carries_error_class() {
    let mock = discord(Fixture::Open).await;
    let capture = tracing_capture::global();
    let start = capture.text().len();
    // Threshold 3, log-only: the third join alerts with no channel to post to.
    run_raid(Scripted(vec![settings(3.0, None)]), &mock, burst(5, 1000)).await;
    assert!(mock.requests().is_empty(), "no channel: no Discord I/O");
    assert_tagged(&capture.text().split_off(start), "raid_alert");
    mock.shutdown().await;
}

#[tokio::test]
async fn raid_undeliverable_carries_error_class() {
    let mock = discord(Fixture::ViewOnly).await;
    let capture = tracing_capture::global();
    let start = capture.text().len();
    run_raid(
        Scripted(vec![settings(3.0, Some(CHANNEL))]),
        &mock,
        burst(5, 1000),
    )
    .await;
    assert!(
        posts(&mock).is_empty(),
        "no Send permission: nothing posted"
    );
    assert_tagged(&capture.text().split_off(start), "raid_alert_undeliverable");
    mock.shutdown().await;
}

#[tokio::test]
async fn raid_post_failure_carries_error_class() {
    let mock = discord(Fixture::PostRejected).await;
    let capture = tracing_capture::global();
    let start = capture.text().len();
    run_raid(
        Scripted(vec![settings(3.0, Some(CHANNEL))]),
        &mock,
        burst(8, 1000),
    )
    .await;
    assert_eq!(posts(&mock).len(), 1, "one attempt, never retried");
    assert_tagged(&capture.text().split_off(start), "raid_alert_post_failed");
    mock.shutdown().await;
}

#[tokio::test]
async fn raid_worker_panic_carries_error_class() {
    let mock = discord(Fixture::Open).await;
    let capture = tracing_capture::global();
    let start = capture.text().len();
    // The first observation panics in its settings read and is lost; the
    // worker keeps serving, so the next three joins still alert.
    run_raid(
        PanicsOnce {
            panicked: false,
            then: settings(3.0, Some(CHANNEL)),
        },
        &mock,
        burst(4, 1000),
    )
    .await;
    assert_eq!(posts(&mock).len(), 1, "the worker survives the panic");
    assert_tagged(&capture.text().split_off(start), "raid_watch_failed");
    mock.shutdown().await;
}

#[tokio::test]
async fn raid_queue_full_increments_dropped_counter() {
    let mock = discord(Fixture::Open).await;
    let entered = Arc::new(AtomicBool::new(false));
    let (runtime, worker) = raid_runtime::start(
        GUILD,
        PendingRaid {
            entered: Arc::clone(&entered),
        },
        executor(&mock),
    );
    runtime.observe_join(join(GUILD, 7, NOW));
    wait_for_stall(&entered).await;
    let before = series_value(&render(), RAID_DROPPED, "queue_full");
    for n in 0..QUEUE_FILL {
        runtime.observe_join(join(GUILD, 8 + n, NOW + n as i64));
    }
    for n in 0..3 {
        runtime.observe_join(join(GUILD, 900_000 + n, NOW));
    }
    let after = series_value(&render(), RAID_DROPPED, "queue_full");
    worker.abort();
    mock.shutdown().await;
    assert_delta("raid queue_full", before, after, 3);
}

#[tokio::test]
async fn raid_queue_closed_logs_error_and_increments() {
    let mock = discord(Fixture::Open).await;
    let capture = tracing_capture::global();
    let (runtime, worker) =
        raid_runtime::start(GUILD, Scripted(vec![settings(3.0, None)]), executor(&mock));
    // Awaiting the aborted worker observes full teardown: its receiver is
    // dropped, so the next observation deterministically hits `Closed`.
    worker.abort();
    let _ = worker.await;
    let start = capture.text().len();
    let before = series_value(&render(), RAID_DROPPED, "queue_closed");
    runtime.observe_join(join(GUILD, 7, NOW));
    let after = series_value(&render(), RAID_DROPPED, "queue_closed");
    mock.shutdown().await;
    assert_tagged(&capture.text().split_off(start), "raid_queue_closed");
    assert_delta("raid queue_closed", before, after, 1);
}

// ---------------------------------------------------------------------------
// Join-risk `error_class` sites.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn join_risk_tuning_refusal_carries_error_class() {
    let capture = tracing_capture::global();
    let mut source = RiskStoreSettings::new(
        dead_pool(),
        GUILD,
        vars(&[("TWO_JOIN_RISK_THRESHOLD", "abc")]),
    );
    let start = capture.text().len();
    let refusal = source
        .current()
        .await
        .expect_err("unusable threshold refuses");
    assert_eq!(refusal.key(), "TWO_JOIN_RISK_THRESHOLD");
    assert_tagged(&capture.text().split_off(start), "join_risk_tuning_refused");
}

#[tokio::test]
async fn join_risk_worker_panic_carries_error_class() {
    let mock = discord(Fixture::Open).await;
    let capture = tracing_capture::global();
    let start = capture.text().len();
    // The first observation panics in its settings read and is lost; the
    // worker keeps serving, so the second join reaches the (failing) store.
    let (runtime, worker) = join_risk_runtime::start(
        GUILD,
        PanicsOnceRisk {
            panicked: false,
            then: risk_settings(3.0, None),
        },
        JoinRiskStore::from_pool(dead_pool()),
        executor(&mock),
    );
    runtime.observe_join(risk_join(0));
    runtime.observe_join(risk_join(1));
    drop(runtime);
    worker.await.expect("worker ends cleanly");
    let logs = capture.text().split_off(start);
    mock.shutdown().await;
    assert_tagged(&logs, "join_risk_failed");
    assert_tagged(&logs, "join_risk_store_failed");
}

#[tokio::test]
async fn join_risk_store_failure_carries_error_class() {
    let mock = discord(Fixture::Open).await;
    let capture = tracing_capture::global();
    let start = capture.text().len();
    let (runtime, worker) = join_risk_runtime::start(
        GUILD,
        ScriptedRisk(vec![risk_settings(3.0, None)]),
        JoinRiskStore::from_pool(dead_pool()),
        executor(&mock),
    );
    runtime.observe_join(risk_join(0));
    drop(runtime);
    worker.await.expect("worker ends cleanly");
    mock.shutdown().await;
    assert_tagged(&capture.text().split_off(start), "join_risk_store_failed");
}

#[tokio::test]
async fn join_risk_queue_full_increments_dropped_counter() {
    let mock = discord(Fixture::Open).await;
    let entered = Arc::new(AtomicBool::new(false));
    let (runtime, worker) = join_risk_runtime::start(
        GUILD,
        PendingRisk {
            entered: Arc::clone(&entered),
        },
        JoinRiskStore::from_pool(dead_pool()),
        executor(&mock),
    );
    runtime.observe_join(risk_join(0));
    wait_for_stall(&entered).await;
    let before = series_value(&render(), RISK_DROPPED, "queue_full");
    for n in 1..=QUEUE_FILL {
        runtime.observe_join(risk_join(n));
    }
    for n in 0..3 {
        runtime.observe_join(risk_join(500_000 + n));
    }
    let after = series_value(&render(), RISK_DROPPED, "queue_full");
    worker.abort();
    mock.shutdown().await;
    assert_delta("join-risk queue_full", before, after, 3);
}

#[tokio::test]
async fn join_risk_queue_closed_logs_error_and_increments() {
    let mock = discord(Fixture::Open).await;
    let capture = tracing_capture::global();
    let (runtime, worker) = join_risk_runtime::start(
        GUILD,
        ScriptedRisk(vec![risk_settings(3.0, None)]),
        JoinRiskStore::from_pool(dead_pool()),
        executor(&mock),
    );
    // Awaiting the aborted worker observes full teardown: its receiver is
    // dropped, so the next observation deterministically hits `Closed`.
    worker.abort();
    let _ = worker.await;
    let start = capture.text().len();
    let before = series_value(&render(), RISK_DROPPED, "queue_closed");
    runtime.observe_join(risk_join(0));
    let after = series_value(&render(), RISK_DROPPED, "queue_closed");
    mock.shutdown().await;
    assert_tagged(&capture.text().split_off(start), "join_risk_queue_closed");
    assert_delta("join-risk queue_closed", before, after, 1);
}

// ---------------------------------------------------------------------------
// Source pins: exact `error_class` tokens for sites no offline behavior can
// reach. Each pin documents why behavior cannot get there.
// ---------------------------------------------------------------------------

#[test]
fn raid_mention_refusal_source_pins_error_class() {
    // `RaidAlert::staff_message` hardcodes `MentionPolicy::None`, so the
    // guard never fires; the builder still tags it for the day it can.
    assert_source_tagged(RAID_SOURCE, "raid_runtime.rs", "raid_alert_refused");
}

#[test]
fn join_risk_policy_rejection_source_pins_error_class() {
    // The worker re-validates an already-valid `RaidTuning`, so the
    // constructor cannot fail; the builder still tags the guard.
    assert_source_tagged(
        RISK_SOURCE,
        "join_risk_runtime.rs",
        "join_risk_policy_rejected",
    );
}

#[test]
fn join_risk_missing_account_age_source_pins_error_class() {
    // `member_id` is a `u64`, so its string always parses and the decode
    // always succeeds; the builder still tags the guard.
    assert_source_tagged(
        RISK_SOURCE,
        "join_risk_runtime.rs",
        "join_risk_no_account_age",
    );
}

#[test]
fn join_risk_evidence_source_pins_error_class() {
    // Evidence logs only after a persisted store claim, which needs a live
    // database; this offline suite never touches one.
    assert_source_tagged(RISK_SOURCE, "join_risk_runtime.rs", "join_risk_evidence");
}

#[test]
fn join_risk_mention_refusal_source_pins_error_class() {
    // `JoinRiskEvidence::staff_message` hardcodes `MentionPolicy::None`, so
    // the guard never fires; the builder still tags it for the day it can.
    assert_source_tagged(RISK_SOURCE, "join_risk_runtime.rs", "join_risk_refused");
}

#[test]
fn join_risk_undeliverable_source_pins_error_class() {
    // Undeliverable is decided only after a persisted store claim, which
    // needs a live database; this offline suite never touches one.
    assert_source_tagged(
        RISK_SOURCE,
        "join_risk_runtime.rs",
        "join_risk_undeliverable",
    );
}

#[test]
fn join_risk_post_failed_source_pins_error_class() {
    // Post failure is decided only after a persisted store claim, which
    // needs a live database; this offline suite never touches one.
    assert_source_tagged(RISK_SOURCE, "join_risk_runtime.rs", "join_risk_post_failed");
}
