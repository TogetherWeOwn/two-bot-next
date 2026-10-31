#![cfg(test)]
//! Protected acceptance: supervised worker loops and the generation wait.
//!
//! BUILDER CONTRACT: this file is frozen. Build on the acceptance branch,
//! keep every test here verbatim, and make the RED tests pass by changing
//! production code only. Do not edit the fixtures or assertions: they are
//! the spec. If a seam must move, coordinate with the Test Author instead of
//! editing this file.
//!
//! Today the three worker loops end only when their inbox sender drops, the
//! production constructors discard the worker handles as `_worker`, and the
//! gateway generation wait busy-polls a counter with fixed 10 ms sleeps.
//! Shutdown cannot drain or join the workers, and the generation wait burns a
//! wakeup every 10 ms until the counter flips.
//!
//! The contract under test. For each runtime (`RaidRuntime`,
//! `ContainmentRuntime`, `JoinRiskRuntime`) the Builder adds, with exactly
//! these names and shapes:
//!
//! ```ignore
//! impl RaidRuntime {
//!     pub fn shutdown(&self);
//!     pub fn finished(&self) -> u64;
//!     pub fn dropped_shutdown(&self) -> u64;
//! }
//! ```
//!
//! * `start()` builds an internal shutdown latch plus shared counters and
//!   hands them to the worker; its signature is unchanged.
//! * The worker selects inbox receives against the latch. A latched shutdown
//!   finishes the in-flight item, drops the rest of the inbox without
//!   starting them, logs the fixed-vocabulary `worker_shutdown_drain` event
//!   with `finished`/`dropped` fields, and exits, so the handle joins.
//! * `finished` counts dequeued items whose handling completed, including
//!   caught panics; `dropped_shutdown` counts queued items never passed to
//!   the handler. Per-item `catch_unwind` is preserved.
//! * An unlatched sender-drop keeps the legacy full drain, so the existing
//!   drop-to-exit suites stay green untouched.
//! * `start_from_env`/`chain_from_env` take the service shutdown signal,
//!   hold the worker handles, and join-or-abort them on shutdown, following
//!   the `eventsub_reconcile::{spawn_if_enabled, join}` template. No more
//!   `_worker` discards on these three workers; `main.rs` passes shutdown
//!   into each production constructor.
//!
//! For the generation wait the Builder ships one of two shapes:
//!
//! * Notify/watch: the wait resolves off a notification paired with the
//!   single bump site in `transport_disconnected` (a `Notify`, or a watch
//!   whose `.changed()` the wait observes). Wake latency is immediate.
//! * Backoff: the wait keeps polling but every sleep in the wait region is
//!   strictly below the old 10 ms interval, with jitter, so wake latency is
//!   equal-or-better for flips inside the first window.
//!
//! In both shapes the fixed `sleep(from_millis(10))` loop beside a
//! `generation.load` is gone. The `notify_reference_wake_latency` control
//! below pins the latency bound executably: a flip 5 ms after wait-start
//! must resolve in under 10 ms.
//!
//! Stability surface (the Builder keeps all of these): the three `start()`
//! signatures, `observe_join`/`observe_audit_entry`, the settings-source
//! traits, the store constructors, the mock-Discord helpers, the
//! `transport_disconnected` name, and the full-queue drop (never block)
//! observer semantics.
//!
//! Offline only: gated in-memory inbox loads, lazy pools that can never
//! connect, and loopback mock Discord. No database, no live Discord.

use std::collections::HashSet;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use twilight_model::guild::audit_log::AuditLogEventType;
use two_bot_core::backup::guild_config::STAGING_BOT_APPLICATION_ID;
use two_bot_core::containment::{
    DEFAULT_CONTAINMENT_HEAT_THRESHOLD, DEFAULT_CONTAINMENT_MAX_AGE_MS,
    DEFAULT_CONTAINMENT_WINDOW_MS,
};
use two_bot_core::containment_store::ContainmentStore;
use two_bot_core::join_risk_store::JoinRiskStore;
use two_bot_core::RaidTuning;
use two_bot_cutover::parse::date_to_snowflake;
use two_bot_discord::{
    ActionExecutor, AuditEntryObserver, AuditLogObservation, JoinObservation, JoinObserver,
};

use crate::containment_runtime::{
    self, ContainmentRuntime, ContainmentSettings, SettingsSource as ContainmentSettingsSource,
};
use crate::discord_test_common::{MockRest, RestRequest, ScriptedResponse};
use crate::gateway::ensure_crypto_provider;
use crate::join_risk_runtime::{
    self, JoinRiskRuntime, JoinRiskSettings, JoinRiskTuningRefusal,
    SettingsSource as JoinRiskSettingsSource,
};
use crate::raid_runtime::{self, RaidRuntime, RaidSettings, SettingsSource as RaidSettingsSource};

/// Join bound for every supervised drain below: the drain finishes one
/// in-flight item and drops the queue, so seconds are generous on loaded CI.
const DRAIN_BOUND: Duration = Duration::from_secs(5);
const GUILD: u64 = 22;
const CHANNEL: &str = "12";
/// Inbox load per drain test: one in-flight item plus this many queued.
const QUEUED: u64 = 31;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("wall clock")
        .as_millis() as i64
}

fn snowflake_now() -> u64 {
    date_to_snowflake(now_ms() as u64)
        .parse()
        .expect("snowflakes are numeric")
}

/// A pool that can never connect: refused loopback, so any store read fails
/// fast offline and no test database is touched.
fn lazy_pool() -> PgPool {
    PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(500))
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("lazy pool builds without connecting")
}

/// Identity reads only; any other Discord route panics, proving the drain
/// path performs no Discord IO after (containment) identity verification.
async fn identity_only_mock() -> MockRest {
    MockRest::with_responder(|request: &RestRequest| {
        let path = request.path.strip_prefix("/api/v10").unwrap();
        match (request.method.as_str(), path) {
            ("GET", "/users/@me") => {
                ScriptedResponse::json(200, json!({"id": "999", "bot": true, "username": "bot"}))
            }
            ("GET", "/applications/@me") => {
                ScriptedResponse::json(200, json!({"id": STAGING_BOT_APPLICATION_ID}))
            }
            _ => panic!(
                "offline drain test: unexpected Discord route {} {}",
                request.method, path
            ),
        }
    })
    .await
}

/// Full staff-post fixture for the supervised delivery pin: the bot may View
/// and Send in the staff channel.
async fn raid_open_mock() -> MockRest {
    MockRest::with_responder(|request: &RestRequest| {
        let path = request.path.strip_prefix("/api/v10").unwrap();
        match (request.method.as_str(), path) {
            ("GET", "/users/@me") => {
                ScriptedResponse::json(200, json!({"id": "999", "bot": true, "username": "bot"}))
            }
            ("GET", "/guilds/22") => {
                ScriptedResponse::json(200, json!({"id": "22", "owner_id": "888"}))
            }
            ("GET", "/guilds/22/roles") => {
                ScriptedResponse::json(200, json!([{"id": "22", "permissions": "3072"}]))
            }
            ("GET", "/guilds/22/members/999") => {
                ScriptedResponse::json(200, json!({"user": {"id": "999"}, "roles": []}))
            }
            ("GET", "/channels/12") => ScriptedResponse::json(
                200,
                json!({"id": "12", "guild_id": "22", "type": 0, "permission_overwrites": []}),
            ),
            ("POST", "/channels/12/messages") => ScriptedResponse::json(201, json!({"id": "99"})),
            _ => panic!(
                "supervised delivery test: unexpected Discord route {} {}",
                request.method, path
            ),
        }
    })
    .await
}

fn executor(mock: &MockRest) -> ActionExecutor {
    ensure_crypto_provider();
    ActionExecutor::with_proxy(
        "worker-supervision-test-only-token".into(),
        Some(mock.origin()),
    )
    .unwrap()
}

fn posts(mock: &MockRest) -> Vec<RestRequest> {
    mock.requests()
        .into_iter()
        .filter(|request| request.method == "POST")
        .collect()
}

/// Shared drain gate. In drain mode the first settings read parks until the
/// test opens it and later reads park forever; in flow mode every read
/// passes. Every read emits one event so the test counts started items
/// without sleeping.
#[derive(Clone)]
struct GateCtl {
    gate: Arc<Notify>,
    open: Arc<AtomicBool>,
    flow_through: bool,
    calls: Arc<AtomicU64>,
    events: mpsc::UnboundedSender<()>,
}

impl GateCtl {
    fn script(flow_through: bool, pre_open: bool) -> (Self, mpsc::UnboundedReceiver<()>) {
        let (events, rx) = mpsc::unbounded_channel();
        (
            Self {
                gate: Arc::new(Notify::new()),
                open: Arc::new(AtomicBool::new(pre_open)),
                flow_through,
                calls: Arc::new(AtomicU64::new(0)),
                events,
            },
            rx,
        )
    }

    fn drain() -> (Self, mpsc::UnboundedReceiver<()>) {
        Self::script(false, false)
    }

    fn flow() -> (Self, mpsc::UnboundedReceiver<()>) {
        Self::script(true, true)
    }

    fn open(&self) {
        self.open.store(true, Ordering::SeqCst);
        self.gate.notify_waiters();
    }

    async fn park_first(&self) {
        while !self.open.load(Ordering::SeqCst) {
            self.gate.notified().await;
        }
    }

    async fn check_in(&self, panic_on: Option<u64>) {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.events.send(());
        if panic_on == Some(call) {
            panic!("injected settings failure on read {call}");
        }
        if self.flow_through {
            return;
        }
        if call == 1 {
            self.park_first().await;
        } else {
            std::future::pending::<()>().await;
        }
    }
}

struct RaidGated {
    ctl: GateCtl,
    settings: RaidSettings,
    panic_on: Option<u64>,
}

impl RaidSettingsSource for RaidGated {
    async fn current(&mut self) -> RaidSettings {
        self.ctl.check_in(self.panic_on).await;
        self.settings.clone()
    }
}

struct ContainmentGated {
    ctl: GateCtl,
    settings: ContainmentSettings,
    panic_on: Option<u64>,
}

impl ContainmentSettingsSource for ContainmentGated {
    async fn current(&mut self) -> ContainmentSettings {
        self.ctl.check_in(self.panic_on).await;
        self.settings.clone()
    }
}

struct JoinRiskGated {
    ctl: GateCtl,
    settings: JoinRiskSettings,
    panic_on: Option<u64>,
}

impl JoinRiskSettingsSource for JoinRiskGated {
    async fn current(&mut self) -> Result<JoinRiskSettings, JoinRiskTuningRefusal> {
        self.ctl.check_in(self.panic_on).await;
        Ok(self.settings.clone())
    }
}

fn raid_settings(threshold: f64, channel: Option<&str>) -> RaidSettings {
    RaidSettings {
        tuning: RaidTuning::new(60.0, threshold).unwrap(),
        staff_channel: channel.map(str::to_owned),
    }
}

fn containment_settings() -> ContainmentSettings {
    ContainmentSettings {
        window_ms: DEFAULT_CONTAINMENT_WINDOW_MS,
        max_age_ms: DEFAULT_CONTAINMENT_MAX_AGE_MS,
        heat_threshold: DEFAULT_CONTAINMENT_HEAT_THRESHOLD,
        staff_channel: None,
    }
}

fn join_risk_settings() -> JoinRiskSettings {
    JoinRiskSettings {
        tuning: RaidTuning::new(60.0, 5.0).unwrap(),
        bulk_join_window_until_ms: None,
        staff_channel: None,
    }
}

/// Baseline adapter for the supervised worker API. Each method resolves to
/// this inert stub on the baseline and to the Builder's inherent runtime
/// method once added, because inherent methods win. The legacy controls
/// below call every method qualified so the trait stays live after GREEN.
trait SupervisedWorker {
    fn shutdown(&self);
    fn finished(&self) -> u64;
    fn dropped_shutdown(&self) -> u64;
}

impl SupervisedWorker for RaidRuntime {
    fn shutdown(&self) {}
    fn finished(&self) -> u64 {
        0
    }
    fn dropped_shutdown(&self) -> u64 {
        0
    }
}

impl SupervisedWorker for ContainmentRuntime {
    fn shutdown(&self) {}
    fn finished(&self) -> u64 {
        0
    }
    fn dropped_shutdown(&self) -> u64 {
        0
    }
}

impl SupervisedWorker for JoinRiskRuntime {
    fn shutdown(&self) {}
    fn finished(&self) -> u64 {
        0
    }
    fn dropped_shutdown(&self) -> u64 {
        0
    }
}

async fn recv_started(rx: &mut mpsc::UnboundedReceiver<()>, what: &str) {
    timeout(DRAIN_BOUND, rx.recv())
        .await
        .expect(what)
        .expect("worker exited before starting the expected item");
}

async fn join_worker<R>(handle: JoinHandle<R>, what: &str) -> R {
    timeout(DRAIN_BOUND, handle)
        .await
        .expect(what)
        .expect("supervised worker task panicked")
}

fn assert_quiet(rx: &mut mpsc::UnboundedReceiver<()>) {
    assert!(
        rx.try_recv().is_err(),
        "no further items may start after the drain"
    );
}

fn raid_join(member_id: u64, at_ms: i64) -> JoinObservation {
    JoinObservation {
        guild_id: GUILD,
        member_id,
        source: "unknown".into(),
        joined_at_ms: at_ms,
    }
}

#[tokio::test]
async fn raid_supervised_shutdown_drops_queued_with_report() {
    let mock = identity_only_mock().await;
    let (ctl, mut rx) = GateCtl::drain();
    let gated = RaidGated {
        ctl: ctl.clone(),
        settings: raid_settings(100.0, None),
        panic_on: None,
    };
    let (runtime, worker) = raid_runtime::start(GUILD, gated, executor(&mock));
    for n in 0..=QUEUED {
        runtime.observe_join(raid_join(1000 + n, now_ms()));
    }
    recv_started(&mut rx, "raid worker must start the first item").await;
    runtime.shutdown();
    ctl.open();
    join_worker(worker, "raid worker must join within the drain bound").await;
    assert_eq!(runtime.finished(), 1, "exactly the in-flight item finishes");
    assert_eq!(
        runtime.dropped_shutdown(),
        QUEUED,
        "queued items are dropped with a count"
    );
    assert_quiet(&mut rx);
}

#[tokio::test]
async fn raid_supervised_healthy_flow_exits_clean() {
    let mock = identity_only_mock().await;
    let (ctl, mut rx) = GateCtl::flow();
    let gated = RaidGated {
        ctl,
        settings: raid_settings(100.0, None),
        panic_on: None,
    };
    let (runtime, worker) = raid_runtime::start(GUILD, gated, executor(&mock));
    for n in 0..4 {
        runtime.observe_join(raid_join(2000 + n, now_ms()));
    }
    for _ in 0..4 {
        recv_started(&mut rx, "raid worker must process every healthy item").await;
    }
    runtime.shutdown();
    join_worker(worker, "idle supervised raid worker must exit on shutdown").await;
    assert_eq!(runtime.finished(), 4);
    assert_eq!(runtime.dropped_shutdown(), 0);
    assert_quiet(&mut rx);
}

#[tokio::test]
async fn raid_supervised_burst_still_posts_before_exit() {
    let mock = raid_open_mock().await;
    let (ctl, mut rx) = GateCtl::flow();
    let gated = RaidGated {
        ctl,
        settings: raid_settings(3.0, Some(CHANNEL)),
        panic_on: None,
    };
    let (runtime, worker) = raid_runtime::start(GUILD, gated, executor(&mock));
    let first = now_ms();
    for n in 0..3 {
        runtime.observe_join(raid_join(3000 + n, first + n as i64 * 1000));
    }
    for _ in 0..3 {
        recv_started(&mut rx, "raid worker must score every burst join").await;
    }
    // Delivery happens before shutdown: the supervised path keeps handle semantics.
    let deadline = std::time::Instant::now() + DRAIN_BOUND;
    while posts(&mock).is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "burst alert must post before shutdown"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(posts(&mock).len(), 1, "one burst alert posts");
    runtime.shutdown();
    join_worker(worker, "raid worker must exit after posting").await;
    assert_eq!(runtime.finished(), 3);
    assert_eq!(runtime.dropped_shutdown(), 0);
}

#[tokio::test]
async fn raid_legacy_sender_drop_still_drains_all() {
    let mock = identity_only_mock().await;
    let (ctl, mut rx) = GateCtl::flow();
    let gated = RaidGated {
        ctl,
        settings: raid_settings(100.0, None),
        panic_on: None,
    };
    let (runtime, worker) = raid_runtime::start(GUILD, gated, executor(&mock));
    for n in 0..4 {
        runtime.observe_join(raid_join(4000 + n, now_ms()));
    }
    <RaidRuntime as SupervisedWorker>::shutdown(&runtime);
    assert_eq!(<RaidRuntime as SupervisedWorker>::finished(&runtime), 0);
    assert_eq!(
        <RaidRuntime as SupervisedWorker>::dropped_shutdown(&runtime),
        0
    );
    drop(runtime);
    join_worker(worker, "legacy sender-drop must still drain and exit").await;
    for _ in 0..4 {
        recv_started(&mut rx, "legacy drain must start every item").await;
    }
    assert_quiet(&mut rx);
}

#[tokio::test]
async fn raid_legacy_item_panic_does_not_kill_worker() {
    let mock = identity_only_mock().await;
    let (ctl, mut rx) = GateCtl::flow();
    let gated = RaidGated {
        ctl,
        settings: raid_settings(100.0, None),
        panic_on: Some(2),
    };
    let (runtime, worker) = raid_runtime::start(GUILD, gated, executor(&mock));
    for n in 0..3 {
        runtime.observe_join(raid_join(5000 + n, now_ms()));
    }
    <RaidRuntime as SupervisedWorker>::shutdown(&runtime);
    drop(runtime);
    join_worker(worker, "worker must survive a panicking item").await;
    for _ in 0..3 {
        recv_started(&mut rx, "every item starts despite the panic").await;
    }
    assert_quiet(&mut rx);
}

fn containment_entry(entry_id: u64) -> AuditLogObservation {
    AuditLogObservation {
        guild_id: GUILD,
        entry_id,
        action: AuditLogEventType::MemberKick,
        executor_id: Some(5001),
        target_id: Some(6001),
    }
}

fn containment_start(
    gated: ContainmentGated,
    mock: &MockRest,
) -> (Arc<ContainmentRuntime>, JoinHandle<()>) {
    containment_runtime::start(
        GUILD,
        gated,
        ContainmentStore::from_pool(lazy_pool()),
        executor(mock),
        HashSet::new(),
        HashSet::new(),
        false,
    )
}

#[tokio::test]
async fn containment_supervised_shutdown_drops_queued_with_report() {
    let mock = identity_only_mock().await;
    let (ctl, mut rx) = GateCtl::drain();
    let gated = ContainmentGated {
        ctl: ctl.clone(),
        settings: containment_settings(),
        panic_on: None,
    };
    let (runtime, worker) = containment_start(gated, &mock);
    for _ in 0..=QUEUED {
        runtime.observe_audit_entry(containment_entry(snowflake_now()));
    }
    recv_started(&mut rx, "containment worker must start the first entry").await;
    runtime.shutdown();
    ctl.open();
    join_worker(
        worker,
        "containment worker must join within the drain bound",
    )
    .await;
    assert_eq!(runtime.finished(), 1, "exactly the in-flight item finishes");
    assert_eq!(
        runtime.dropped_shutdown(),
        QUEUED,
        "queued items are dropped with a count"
    );
    assert_quiet(&mut rx);
}

#[tokio::test]
async fn containment_supervised_healthy_flow_exits_clean() {
    let mock = identity_only_mock().await;
    let (ctl, mut rx) = GateCtl::flow();
    let gated = ContainmentGated {
        ctl,
        settings: containment_settings(),
        panic_on: None,
    };
    let (runtime, worker) = containment_start(gated, &mock);
    for _ in 0..4 {
        runtime.observe_audit_entry(containment_entry(snowflake_now()));
    }
    for _ in 0..4 {
        recv_started(
            &mut rx,
            "containment worker must process every healthy item",
        )
        .await;
    }
    runtime.shutdown();
    join_worker(
        worker,
        "idle supervised containment worker must exit on shutdown",
    )
    .await;
    assert_eq!(runtime.finished(), 4);
    assert_eq!(runtime.dropped_shutdown(), 0);
    assert_quiet(&mut rx);
}

#[tokio::test]
async fn containment_legacy_sender_drop_still_drains_all() {
    let mock = identity_only_mock().await;
    let (ctl, mut rx) = GateCtl::flow();
    let gated = ContainmentGated {
        ctl,
        settings: containment_settings(),
        panic_on: None,
    };
    let (runtime, worker) = containment_start(gated, &mock);
    for _ in 0..4 {
        runtime.observe_audit_entry(containment_entry(snowflake_now()));
    }
    <ContainmentRuntime as SupervisedWorker>::shutdown(&runtime);
    assert_eq!(
        <ContainmentRuntime as SupervisedWorker>::finished(&runtime),
        0
    );
    assert_eq!(
        <ContainmentRuntime as SupervisedWorker>::dropped_shutdown(&runtime),
        0
    );
    drop(runtime);
    join_worker(worker, "legacy sender-drop must still drain and exit").await;
    for _ in 0..4 {
        recv_started(&mut rx, "legacy drain must start every item").await;
    }
    assert_quiet(&mut rx);
}

#[tokio::test]
async fn containment_legacy_item_panic_does_not_kill_worker() {
    let mock = identity_only_mock().await;
    let (ctl, mut rx) = GateCtl::flow();
    let gated = ContainmentGated {
        ctl,
        settings: containment_settings(),
        panic_on: Some(2),
    };
    let (runtime, worker) = containment_start(gated, &mock);
    for _ in 0..3 {
        runtime.observe_audit_entry(containment_entry(snowflake_now()));
    }
    <ContainmentRuntime as SupervisedWorker>::shutdown(&runtime);
    drop(runtime);
    join_worker(worker, "worker must survive a panicking item").await;
    for _ in 0..3 {
        recv_started(&mut rx, "every item starts despite the panic").await;
    }
    assert_quiet(&mut rx);
}

fn join_risk_join() -> JoinObservation {
    JoinObservation {
        guild_id: GUILD,
        member_id: snowflake_now(),
        source: "unknown".into(),
        joined_at_ms: now_ms(),
    }
}

fn join_risk_start(
    gated: JoinRiskGated,
    mock: &MockRest,
) -> (Arc<JoinRiskRuntime>, JoinHandle<()>) {
    join_risk_runtime::start(
        GUILD,
        gated,
        JoinRiskStore::from_pool(lazy_pool()),
        executor(mock),
    )
}

#[tokio::test]
async fn join_risk_supervised_shutdown_drops_queued_with_report() {
    let mock = identity_only_mock().await;
    let (ctl, mut rx) = GateCtl::drain();
    let gated = JoinRiskGated {
        ctl: ctl.clone(),
        settings: join_risk_settings(),
        panic_on: None,
    };
    let (runtime, worker) = join_risk_start(gated, &mock);
    for _ in 0..=QUEUED {
        runtime.observe_join(join_risk_join());
    }
    recv_started(&mut rx, "join-risk worker must start the first join").await;
    runtime.shutdown();
    ctl.open();
    join_worker(worker, "join-risk worker must join within the drain bound").await;
    assert_eq!(runtime.finished(), 1, "exactly the in-flight item finishes");
    assert_eq!(
        runtime.dropped_shutdown(),
        QUEUED,
        "queued items are dropped with a count"
    );
    assert_quiet(&mut rx);
}

#[tokio::test]
async fn join_risk_supervised_healthy_flow_exits_clean() {
    let mock = identity_only_mock().await;
    let (ctl, mut rx) = GateCtl::flow();
    let gated = JoinRiskGated {
        ctl,
        settings: join_risk_settings(),
        panic_on: None,
    };
    let (runtime, worker) = join_risk_start(gated, &mock);
    for _ in 0..4 {
        runtime.observe_join(join_risk_join());
    }
    for _ in 0..4 {
        recv_started(&mut rx, "join-risk worker must process every healthy item").await;
    }
    runtime.shutdown();
    join_worker(
        worker,
        "idle supervised join-risk worker must exit on shutdown",
    )
    .await;
    assert_eq!(runtime.finished(), 4);
    assert_eq!(runtime.dropped_shutdown(), 0);
    assert_quiet(&mut rx);
}

#[tokio::test]
async fn join_risk_legacy_sender_drop_still_drains_all() {
    let mock = identity_only_mock().await;
    let (ctl, mut rx) = GateCtl::flow();
    let gated = JoinRiskGated {
        ctl,
        settings: join_risk_settings(),
        panic_on: None,
    };
    let (runtime, worker) = join_risk_start(gated, &mock);
    for _ in 0..4 {
        runtime.observe_join(join_risk_join());
    }
    <JoinRiskRuntime as SupervisedWorker>::shutdown(&runtime);
    assert_eq!(<JoinRiskRuntime as SupervisedWorker>::finished(&runtime), 0);
    assert_eq!(
        <JoinRiskRuntime as SupervisedWorker>::dropped_shutdown(&runtime),
        0
    );
    drop(runtime);
    join_worker(worker, "legacy sender-drop must still drain and exit").await;
    for _ in 0..4 {
        recv_started(&mut rx, "legacy drain must start every item").await;
    }
    assert_quiet(&mut rx);
}

#[tokio::test]
async fn join_risk_legacy_item_panic_does_not_kill_worker() {
    let mock = identity_only_mock().await;
    let (ctl, mut rx) = GateCtl::flow();
    let gated = JoinRiskGated {
        ctl,
        settings: join_risk_settings(),
        panic_on: Some(2),
    };
    let (runtime, worker) = join_risk_start(gated, &mock);
    for _ in 0..3 {
        runtime.observe_join(join_risk_join());
    }
    <JoinRiskRuntime as SupervisedWorker>::shutdown(&runtime);
    drop(runtime);
    join_worker(worker, "worker must survive a panicking item").await;
    for _ in 0..3 {
        recv_started(&mut rx, "every item starts despite the panic").await;
    }
    assert_quiet(&mut rx);
}

// Structural pins below: the production constructors and the generation wait
// have no offline behavioral seam (environment, pools, gateway state), so the
// scans assert their shape on comment- and string-stripped source. Each scan
// fails on the baseline for the reason it names.

const RAID_SOURCE: &str = include_str!("raid_runtime.rs");
const CONTAINMENT_SOURCE: &str = include_str!("containment_runtime.rs");
const JOIN_RISK_SOURCE: &str = include_str!("join_risk_runtime.rs");
const GATEWAY_SOURCE: &str = include_str!("gateway.rs");
const MAIN_SOURCE: &str = include_str!("main.rs");
const EVENTSUB_SOURCE: &str = include_str!("eventsub_reconcile.rs");

fn worker_sources() -> [(&'static str, &'static str); 3] {
    [
        ("raid_runtime.rs", RAID_SOURCE),
        ("containment_runtime.rs", CONTAINMENT_SOURCE),
        ("join_risk_runtime.rs", JOIN_RISK_SOURCE),
    ]
}

fn production_constructors() -> [&'static str; 3] {
    [
        "raid_runtime::start_from_env",
        "join_risk_runtime::chain_from_env",
        "containment_runtime::start_from_env",
    ]
}

// Comment- and string-aware source masking, after the
// `timing_sleep_removal.rs` acceptance guard: doc prose or log text must
// never satisfy a structural pin.
fn skip_non_code(bytes: &[u8], index: usize) -> Option<usize> {
    if bytes.get(index..index + 2) == Some(b"//") {
        return Some(
            bytes[index + 2..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(bytes.len(), |offset| index + 2 + offset),
        );
    }
    if bytes.get(index..index + 2) == Some(b"/*") {
        let mut cursor = index + 2;
        let mut depth = 1;
        while cursor + 1 < bytes.len() {
            match &bytes[cursor..cursor + 2] {
                b"/*" => {
                    depth += 1;
                    cursor += 2;
                }
                b"*/" => {
                    depth -= 1;
                    cursor += 2;
                    if depth == 0 {
                        return Some(cursor);
                    }
                }
                _ => cursor += 1,
            }
        }
        return Some(bytes.len());
    }

    let raw_start = if bytes.get(index) == Some(&b'r') {
        Some(index)
    } else if bytes.get(index..index + 2) == Some(b"br") {
        Some(index + 1)
    } else {
        None
    };
    if let Some(raw_start) = raw_start {
        let mut quote = raw_start + 1;
        while bytes.get(quote) == Some(&b'#') {
            quote += 1;
        }
        if bytes.get(quote) == Some(&b'"') {
            let hashes = quote - raw_start - 1;
            let mut cursor = quote + 1;
            while cursor < bytes.len() {
                if bytes[cursor] == b'"'
                    && bytes.get(cursor + 1..cursor + 1 + hashes)
                        == Some(&bytes[raw_start + 1..quote])
                {
                    return Some(cursor + 1 + hashes);
                }
                cursor += 1;
            }
            return Some(bytes.len());
        }
    }

    if matches!(bytes.get(index), Some(b'"' | b'\'')) {
        let quote_index = index;
        let quote = bytes[quote_index];
        let mut cursor = quote_index + 1;
        while cursor < bytes.len() {
            if quote == b'\'' && cursor - quote_index > 12 {
                return None;
            }
            match bytes[cursor] {
                b'\\' => cursor = (cursor + 2).min(bytes.len()),
                byte if byte == quote => return Some(cursor + 1),
                b'\n' if quote == b'\'' => return None,
                _ => cursor += 1,
            }
        }
        return (quote == b'"').then_some(bytes.len());
    }
    None
}

fn code_only(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut code = bytes.to_vec();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code(bytes, cursor) {
            for index in cursor..next {
                if !matches!(bytes[index], b'\n' | b'\r') {
                    code[index] = b' ';
                }
            }
            cursor = next;
        } else {
            cursor += 1;
        }
    }
    String::from_utf8(code).expect("masking source preserves UTF-8")
}

fn code_lines(source: &str) -> Vec<String> {
    code_only(source).lines().map(str::to_owned).collect()
}

fn matches_in(lines: &[String], needle: &str) -> Vec<usize> {
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains(needle))
        .map(|(index, _)| index)
        .collect()
}

fn window(lines: &[String], center: usize, radius: usize) -> String {
    let low = center.saturating_sub(radius);
    let high = (center + radius + 1).min(lines.len());
    lines[low..high].join("\n")
}

/// Balanced call arguments starting at the first `(` at or after `call`.
fn call_arguments(code: &str, call: usize) -> String {
    let bytes = code.as_bytes();
    let mut cursor = call;
    while cursor < bytes.len() && bytes[cursor] != b'(' {
        if let Some(next) = skip_non_code(bytes, cursor) {
            cursor = next;
        } else {
            cursor += 1;
        }
    }
    assert!(
        cursor < bytes.len(),
        "call site must open its argument list"
    );
    let open = cursor;
    let mut depth = 0;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code(bytes, cursor) {
            cursor = next;
            continue;
        }
        match bytes[cursor] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return code[open..=cursor].to_owned();
                }
            }
            _ => {}
        }
        cursor += 1;
    }
    panic!("unterminated call starting at byte {call}");
}

fn millis_literals(context: &str) -> Vec<u64> {
    let mut values = Vec::new();
    let mut rest = context;
    while let Some(at) = rest.find("from_millis(") {
        rest = &rest[at + "from_millis(".len()..];
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if let Ok(value) = digits.parse::<u64>() {
            values.push(value);
        }
    }
    values
}

#[test]
fn worker_handles_are_held_not_discarded() {
    for (name, source) in worker_sources() {
        let code = code_only(source);
        assert!(
            !code.contains("_worker"),
            "{name} must hold its worker handle instead of discarding it as _worker"
        );
    }
}

#[test]
fn supervised_runtime_api_exists() {
    for (name, source) in worker_sources() {
        let code = code_only(source);
        for item in ["fn shutdown", "fn finished", "fn dropped_shutdown"] {
            assert!(
                code.contains(item),
                "{name} must define the supervised runtime API ({item})"
            );
        }
    }
}

#[test]
fn supervised_drain_event_is_logged() {
    for (name, source) in worker_sources() {
        let code = code_only(source);
        assert!(
            code.contains("worker_shutdown_drain"),
            "{name} must log the fixed-vocabulary worker_shutdown_drain event"
        );
    }
}

#[test]
fn production_constructors_plumb_shutdown() {
    for (name, source) in worker_sources() {
        let code = code_only(source);
        assert!(
            code.contains("shutdown"),
            "{name} must plumb a shutdown signal to its worker"
        );
    }
}

#[test]
fn main_passes_shutdown_to_constructors() {
    let main = code_only(MAIN_SOURCE);
    for call in production_constructors() {
        let at = main
            .find(call)
            .expect("production wiring must keep its constructors");
        let args = call_arguments(&main, at);
        assert!(
            args.contains("shutdown"),
            "main.rs must pass shutdown to {call}"
        );
    }
}

#[test]
fn shutdown_joins_or_aborts_handles() {
    let mut abort_found = false;
    for (_, source) in worker_sources() {
        if code_only(source).contains("abort") {
            abort_found = true;
        }
    }
    let main_lines = code_lines(MAIN_SOURCE);
    for call in production_constructors() {
        for line in matches_in(&main_lines, call) {
            if window(&main_lines, line, 30).contains("abort") {
                abort_found = true;
            }
        }
    }
    assert!(
        abort_found,
        "shutdown must join-or-abort the worker handles (see eventsub_reconcile::join)"
    );
}

/// Non-assert generation counter reads: the production wait sites. Test
/// assertions on the counter are skipped so builder tests cannot trip the pins.
fn generation_reads(lines: &[String]) -> Vec<usize> {
    matches_in(lines, "generation.load")
        .into_iter()
        .filter(|index| !lines[*index].contains("assert"))
        .collect()
}

#[test]
fn generation_wait_has_no_fixed_poll() {
    let lines = code_lines(GATEWAY_SOURCE);
    let mut violations = Vec::new();
    for read in generation_reads(&lines) {
        let context = window(&lines, read, 12);
        if context.contains("from_millis(10)") && context.contains("sleep") {
            violations.push(format!("line {}:\n{context}", read + 1));
        }
    }
    assert!(
        violations.is_empty(),
        "generation wait must not fixed-poll every 10ms:\n{}",
        violations.join("\n---\n")
    );
}

struct WaitMechanism {
    notify: bool,
    watch: bool,
    backoff: bool,
}

fn wait_mechanism(lines: &[String]) -> WaitMechanism {
    let mut mechanism = WaitMechanism {
        notify: false,
        watch: false,
        backoff: false,
    };
    for read in generation_reads(lines) {
        let context = window(lines, read, 15);
        // The wait side (`notified()`), not the wake side: unrelated
        // `notify_one` calls sit near other counter reads.
        if context.to_lowercase().contains("notified") {
            mechanism.notify = true;
        }
        if context.contains("jitter")
            || context.contains("Jitter")
            || context.contains("backoff")
            || context.contains("Backoff")
            || context.contains("thread_rng")
            || context.contains("rand::")
            || context.contains("SmallRng")
            || context.contains("StdRng")
        {
            mechanism.backoff = true;
        }
    }
    for changed in matches_in(lines, ".changed(") {
        if window(lines, changed, 15).contains("generation") {
            mechanism.watch = true;
        }
    }
    mechanism
}

#[test]
fn generation_wait_has_a_mechanism() {
    let lines = code_lines(GATEWAY_SOURCE);
    let mechanism = wait_mechanism(&lines);
    assert!(
        mechanism.notify || mechanism.watch || mechanism.backoff,
        "generation wait must be notification-driven (Notify/watch) or backed off with jitter"
    );
}

#[test]
fn generation_wait_is_complete_and_latency_bounded() {
    let lines = code_lines(GATEWAY_SOURCE);
    let mechanism = wait_mechanism(&lines);
    assert!(
        mechanism.notify || mechanism.watch || mechanism.backoff,
        "no generation wait mechanism to complete (see generation_wait_has_a_mechanism)"
    );
    if mechanism.notify || mechanism.watch {
        let bump = matches_in(&lines, "async fn transport_disconnected");
        assert_eq!(
            bump.len(),
            1,
            "the single generation bump site must stay in transport_disconnected"
        );
        let context = window(&lines, bump[0], 40).to_lowercase();
        assert!(
            context.contains("notif") || context.contains("send"),
            "every generation bump must wake the wait (notify the Notify or send on the watch)"
        );
    } else {
        for read in generation_reads(&lines) {
            let context = window(&lines, read, 15);
            assert!(
                !context.contains("from_secs"),
                "backed-off generation wait must tick below one second near line {}",
                read + 1
            );
            for millis in millis_literals(&context) {
                assert!(
                    millis < 10,
                    "backed-off generation wait must tick below the old 10ms poll near line {}",
                    read + 1
                );
            }
        }
    }
}

#[test]
fn eventsub_join_template_stays_intact() {
    let code = code_only(EVENTSUB_SOURCE);
    assert!(
        code.contains("spawn_if_enabled"),
        "eventsub template must keep spawn_if_enabled"
    );
    assert!(
        code.contains("pub async fn join"),
        "eventsub template must keep join"
    );
}

#[tokio::test(start_paused = true)]
async fn notify_reference_wake_latency() {
    // Executable latency bound for the generation wait contract: a flip 5 ms
    // after wait-start resolves in under 10 ms off a notification.
    let generation = Arc::new(AtomicU64::new(7));
    let notify = Arc::new(Notify::new());
    let flipped = Arc::clone(&generation);
    let waker = Arc::clone(&notify);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(5)).await;
        flipped.store(8, Ordering::Release);
        waker.notify_waiters();
    });
    let start = tokio::time::Instant::now();
    loop {
        if generation.load(Ordering::Acquire) != 7 {
            break;
        }
        notify.notified().await;
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(10),
        "notification wake must beat the old 10ms poll: {elapsed:?}"
    );
    assert_eq!(
        elapsed,
        Duration::from_millis(5),
        "paused-time flip resolves exactly when notified"
    );
}
