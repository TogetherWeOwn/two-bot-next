//! Protected acceptance tests: bounded admission for voice interaction
//! responder tasks.
//!
//! PROTECTED FILE: the paired builder implements the production bound
//! without editing this file. The only wiring outside it is the
//! `#[path] mod responder_admission_acceptance;` line in
//! `voice_rooms_tests.rs`.
//!
//! Contract under test (all through the real `VoiceResponder` gateway sink,
//! `handle` / `handle_received`):
//!
//! - Admission is non-waiting and happens before a responder task is
//!   spawned: with every admitted reply future held pending, an arbitrarily
//!   larger burst of recognized voice interactions starts at most
//!   [`MAX_RESPONDER_IN_FLIGHT`] responder executions. Rejected interactions
//!   are dropped, never queued: after the held replies are released, no
//!   rejected interaction starts, acknowledges, or mutates.
//! - Any overload/busy acknowledgement the implementation sends is itself
//!   bounded ([`MAX_BUSY_IN_FLIGHT`] concurrent `respond` calls and
//!   [`MAX_TOTAL_REPLY_IN_FLIGHT`] concurrent reply calls overall), never a
//!   second detached-task flood.
//! - The bound is shared by every recognized kind routed to the responder:
//!   slash commands, recognized component interactions, and `/import`.
//! - Permits return on normal completion, on a failed/ambiguous
//!   acknowledgement, and when an admitted task is cancelled, so later
//!   interactions can always run.
//! - `handle_received` never blocks the gateway event handler, saturated
//!   or not.
//! - Existing ownership and gates are preserved: `/kick` stays with the
//!   shared router (the responder sends no callback for it), unrecognized
//!   interactions are ignored, a disabled voice runtime acknowledges
//!   nothing, and a failed/ambiguous acknowledgement still means no
//!   mutation.
//!
//! The builder may pick any execution bound up to
//! [`MAX_RESPONDER_IN_FLIGHT`] (the shared router's interaction lane is 16)
//! and must document the chosen bound and overload behavior in the
//! production code. Dropping overload silently (sampled log) and sending a
//! bounded busy acknowledgement both satisfy these tests.

use super::*;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::time::{Duration, Instant as StdInstant};
use tokio::sync::watch;
use twilight_model::gateway::payload::incoming::InteractionCreate;
use two_bot_core::voice_custom_id::name_restore_custom_id;

/// Ceiling on concurrently admitted responder executions.
const MAX_RESPONDER_IN_FLIGHT: usize = 16;
/// Ceiling on concurrently in-flight overload/busy acknowledgements.
const MAX_BUSY_IN_FLIGHT: usize = 16;
/// Ceiling on concurrent reply-trait calls of any kind.
const MAX_TOTAL_REPLY_IN_FLIGHT: usize = 32;

const MODE_HOLD: u8 = 0;
const MODE_PASS: u8 = 1;
const MODE_FAIL_DEFER: u8 = 2;

/// Interaction replies whose futures can be held pending, failed, or
/// released on demand, with deterministic concurrency counters. A held
/// `defer` parks each admitted responder execution at its first reply call;
/// a held `respond` parks busy acknowledgements the same way, so any
/// detached-task flood shows up in `peak` instead of completing invisibly.
struct GateReplies {
    mode: AtomicU8,
    gate: watch::Sender<bool>,
    defer_started: AtomicUsize,
    respond_started: AtomicUsize,
    complete_started: AtomicUsize,
    in_flight: AtomicUsize,
    peak: AtomicUsize,
}

impl GateReplies {
    fn new(mode: u8) -> Arc<Self> {
        Arc::new(Self {
            mode: AtomicU8::new(mode),
            gate: watch::Sender::new(false),
            defer_started: AtomicUsize::new(0),
            respond_started: AtomicUsize::new(0),
            complete_started: AtomicUsize::new(0),
            in_flight: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        })
    }

    fn release(&self) {
        self.mode.store(MODE_PASS, Ordering::SeqCst);
        let _ = self.gate.send(true);
    }

    fn set_mode(&self, mode: u8) {
        self.mode.store(mode, Ordering::SeqCst);
    }

    fn mode(&self) -> u8 {
        self.mode.load(Ordering::SeqCst)
    }

    fn defer_started(&self) -> usize {
        self.defer_started.load(Ordering::SeqCst)
    }

    fn respond_started(&self) -> usize {
        self.respond_started.load(Ordering::SeqCst)
    }

    fn complete_started(&self) -> usize {
        self.complete_started.load(Ordering::SeqCst)
    }

    fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }

    fn counters(&self) -> (usize, usize, usize, usize) {
        (
            self.defer_started(),
            self.respond_started(),
            self.complete_started(),
            self.in_flight.load(Ordering::SeqCst),
        )
    }

    fn enter(&self) {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
    }

    fn exit(&self) {
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }

    async fn wait_gate(&self) {
        if self.mode() != MODE_HOLD {
            return;
        }
        let mut watch = self.gate.subscribe();
        while !*watch.borrow() {
            if watch.changed().await.is_err() {
                break;
            }
        }
    }
}

impl InteractionReplies for GateReplies {
    async fn defer(&self, _: &Interaction) -> Result<(), RoomHttpError> {
        self.defer_started.fetch_add(1, Ordering::SeqCst);
        self.enter();
        if self.mode() == MODE_FAIL_DEFER {
            self.exit();
            return Err(RoomHttpError::UnknownOutcome);
        }
        self.wait_gate().await;
        self.exit();
        Ok(())
    }

    async fn complete(&self, _: &Interaction, _: InteractionResponse) -> Result<(), RoomHttpError> {
        self.complete_started.fetch_add(1, Ordering::SeqCst);
        self.enter();
        self.exit();
        Ok(())
    }

    async fn respond(&self, _: &Interaction, _: InteractionResponse) -> Result<(), RoomHttpError> {
        self.respond_started.fetch_add(1, Ordering::SeqCst);
        self.enter();
        self.wait_gate().await;
        self.exit();
        Ok(())
    }
}

type GateSink = VoiceResponder<Store, Http, GateReplies>;

fn gate_sink(
    trace: Trace,
    mode: u8,
) -> (
    Arc<GateSink>,
    Arc<GateReplies>,
    Arc<VoiceRuntime<Store, Http>>,
) {
    let runtime = Arc::new(test_runtime(trace));
    let replies = GateReplies::new(mode);
    let sink = Arc::new(VoiceResponder::new(
        Arc::clone(&runtime),
        Arc::clone(&replies),
    ));
    (sink, replies, runtime)
}

fn send(sink: &GateSink, cache: &DefaultInMemoryCache, interaction: Interaction) {
    sink.handle(
        &Event::InteractionCreate(Box::new(InteractionCreate(interaction))),
        cache,
    );
}

/// Distinct interaction id and author per burst entry, so neither an
/// interaction-id dedupe nor a per-member fairness cap can masquerade as
/// the global execution bound.
fn burst_create(index: usize) -> Interaction {
    let mut interaction = voice_interaction_as(
        Some(command_data(
            "create",
            vec![command_option("name", "lobby")],
        )),
        Some(Permissions::MANAGE_CHANNELS),
        20_000 + index as u64,
    );
    interaction.id = Id::new(30_000 + index as u64);
    interaction
}

fn burst_setup(index: usize) -> Interaction {
    let mut interaction = voice_interaction(Some(command_data("setup", Vec::new())), None, true);
    interaction.id = Id::new(40_000 + index as u64);
    interaction
}

fn creates(trace: &Trace) -> usize {
    trace
        .lock()
        .unwrap()
        .iter()
        .filter(|entry| entry.as_str() == "create")
        .count()
}

async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "deadline waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Wait until every reply counter has been stable for ~100 ms, so spawned
/// work has had every chance to start (or be refused) before assertions.
async fn settle(replies: &GateReplies) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut stable = 0;
    let mut last = replies.counters();
    while stable < 20 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "reply counters never settled: {:?}",
            replies.counters()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
        let now = replies.counters();
        if now == last {
            stable += 1;
        } else {
            stable = 0;
            last = now;
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn responder_burst_is_bounded_before_spawned_tasks_can_run() {
    let trace = Trace::default();
    let (sink, replies, runtime) = gate_sink(trace.clone(), MODE_HOLD);
    assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
    let cache = DefaultInMemoryCache::new();
    let metrics = tokio::runtime::Handle::current().metrics();
    let tasks_before = metrics.num_alive_tasks();
    let capacity = sink.admission.permits.available_permits();
    assert!((1..=MAX_RESPONDER_IN_FLIGHT).contains(&capacity));
    let drops_before = sink.admission.drops.load(Ordering::SeqCst);

    // No await: a current-thread executor cannot poll any spawned task
    // during this burst. Acquiring inside a task leaves the entire burst
    // queued, even if the reply concurrency looks bounded after a yield.
    for index in 0..256 {
        send(&sink, &cache, burst_create(index));
    }
    assert_eq!(replies.counters(), (0, 0, 0, 0));
    assert_eq!(
        sink.admission.permits.available_permits(),
        0,
        "admission did not reserve capacity before spawning"
    );
    assert_eq!(
        sink.admission.drops.load(Ordering::SeqCst) - drops_before,
        (256 - capacity) as u64,
        "excess interactions were queued instead of synchronously refused"
    );
    let spawned = metrics.num_alive_tasks() - tasks_before;
    assert!(
        (1..=MAX_TOTAL_REPLY_IN_FLIGHT).contains(&spawned),
        "synchronous burst queued {spawned} tasks before admission; even with bounded busy replies the ceiling is {MAX_TOTAL_REPLY_IN_FLIGHT}"
    );
    assert_eq!(creates(&trace), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn responder_saturated_invite_is_refused_before_settings_snapshot() {
    let trace = Trace::default();
    let snapshot_reads = Arc::new(AtomicUsize::new(0));
    let reader = Arc::clone(&snapshot_reads);
    let runtime = Arc::new(test_runtime(trace.clone()).with_live_source(move || {
        reader.fetch_add(1, Ordering::SeqCst);
        None
    }));
    let replies = GateReplies::new(MODE_PASS);
    let sink = VoiceResponder::new(runtime, Arc::clone(&replies));
    let cache = DefaultInMemoryCache::new();
    let capacity = sink.admission.permits.available_permits();
    assert!((1..=MAX_RESPONDER_IN_FLIGHT).contains(&capacity));
    let held = sink
        .admission
        .permits
        .try_acquire_many(capacity as u32)
        .expect("hold every responder permit");
    let mut invite = voice_interaction(Some(command_data("invite", Vec::new())), None, true);
    invite.id = Id::new(70_000);

    // The existing live-settings reader is an observable snapshot boundary.
    // A refused interaction must not cross it, even before any task polls.
    send(&sink, &cache, invite.clone());
    assert_eq!(
        snapshot_reads.load(Ordering::SeqCst),
        0,
        "saturated invite read a settings snapshot before refusal"
    );
    assert_eq!(sink.admission.drops.load(Ordering::SeqCst), 1);
    assert_eq!(replies.counters(), (0, 0, 0, 0));
    assert_eq!(creates(&trace), 0);

    // Positive control: the same reader must be live for an admitted invite,
    // so deleting snapshot capture cannot satisfy the refusal assertion.
    drop(held);
    invite.id = Id::new(70_001);
    send(&sink, &cache, invite);
    wait_until("admitted invite to complete", || {
        replies.complete_started() == 1
    })
    .await;
    assert_eq!(snapshot_reads.load(Ordering::SeqCst), 1);
    assert_eq!(replies.defer_started(), 1);
}

#[tokio::test]
async fn responder_burst_never_exceeds_execution_bound_while_replies_held() {
    let trace = Trace::default();
    let (sink, replies, runtime) = gate_sink(trace.clone(), MODE_HOLD);
    assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
    let cache = DefaultInMemoryCache::new();

    for index in 0..256 {
        send(&sink, &cache, burst_create(index));
    }
    settle(&replies).await;

    let admitted = replies.defer_started();
    assert!(
        (1..=MAX_RESPONDER_IN_FLIGHT).contains(&admitted),
        "held burst admitted {admitted} responder executions, bound is {MAX_RESPONDER_IN_FLIGHT}"
    );
    assert!(
        replies.respond_started() <= MAX_BUSY_IN_FLIGHT,
        "overload acknowledgements started {}, bound is {MAX_BUSY_IN_FLIGHT}",
        replies.respond_started()
    );
    assert!(
        replies.peak() <= MAX_TOTAL_REPLY_IN_FLIGHT,
        "concurrent reply calls peaked at {}, bound is {MAX_TOTAL_REPLY_IN_FLIGHT}",
        replies.peak()
    );
    assert_eq!(
        replies.complete_started(),
        0,
        "held replies cannot complete"
    );
    assert_eq!(creates(&trace), 0, "no mutation runs before its ack lands");

    // Releasing the gate finishes exactly the admitted executions. Rejected
    // interactions were dropped, not queued: none of them starts, completes,
    // or mutates afterwards.
    replies.release();
    wait_until("admitted executions to complete", || {
        replies.complete_started() == admitted
    })
    .await;
    settle(&replies).await;
    assert_eq!(
        replies.defer_started(),
        admitted,
        "rejected interactions started after release: they were queued, not dropped"
    );
    assert_eq!(replies.complete_started(), admitted);
    assert!(
        creates(&trace) <= admitted,
        "{} channel creates from {admitted} admitted executions",
        creates(&trace)
    );
}

#[tokio::test]
async fn responder_mixed_recognized_interactions_share_one_bound() {
    let trace = Trace::default();
    let (sink, replies, runtime) = gate_sink(trace.clone(), MODE_HOLD);
    assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
    let cache = DefaultInMemoryCache::new();

    // Slash, recognized component, and /import interactions all reach the
    // responder through the gateway sink; together they share one bound.
    for index in 0..64 {
        send(&sink, &cache, burst_create(index));
        let mut component =
            component_interaction(&name_restore_custom_id(500), None, 20_000 + index as u64);
        component.id = Id::new(50_000 + index as u64);
        send(&sink, &cache, component);
        let mut import = import_interaction(10, manager(), 20_000 + index as u64);
        import.id = Id::new(60_000 + index as u64);
        send(&sink, &cache, import);
    }
    settle(&replies).await;

    let admitted = replies.defer_started();
    assert!(
        (1..=MAX_RESPONDER_IN_FLIGHT).contains(&admitted),
        "mixed burst admitted {admitted} executions, bound is {MAX_RESPONDER_IN_FLIGHT}"
    );
    assert!(replies.peak() <= MAX_TOTAL_REPLY_IN_FLIGHT);
    assert_eq!(replies.complete_started(), 0);
    assert_eq!(creates(&trace), 0);
}

#[tokio::test]
async fn gateway_handler_returns_promptly_while_responder_saturated() {
    let trace = Trace::default();
    let (sink, replies, runtime) = gate_sink(trace, MODE_HOLD);
    assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
    let cache = DefaultInMemoryCache::new();

    for index in 0..32 {
        send(&sink, &cache, burst_create(index));
    }
    settle(&replies).await;

    // Admission is a non-waiting check: the gateway handler must stay free
    // even when every permit is held by a pending reply future.
    let started = StdInstant::now();
    for index in 32..160 {
        send(&sink, &cache, burst_create(index));
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "handle blocked for {elapsed:?} on a saturated responder"
    );
    settle(&replies).await;
    assert!(
        replies.defer_started() <= MAX_RESPONDER_IN_FLIGHT,
        "saturated handler admitted {} executions",
        replies.defer_started()
    );
}

#[tokio::test]
async fn responder_permits_return_after_normal_completion() {
    let trace = Trace::default();
    let (sink, replies, _runtime) = gate_sink(trace, MODE_PASS);
    let cache = DefaultInMemoryCache::new();

    // More sequential interactions than any allowed bound: each may only
    // complete if the previous execution returned its permit.
    for index in 0..40 {
        send(&sink, &cache, burst_setup(index));
        let expected = index + 1;
        wait_until("sequential interaction to complete", || {
            replies.complete_started() == expected
        })
        .await;
    }
    assert_eq!(replies.defer_started(), 40);
}

#[tokio::test(flavor = "current_thread")]
async fn responder_permits_return_after_failed_ack_and_failed_ack_never_mutates() {
    let trace = Trace::default();
    let (sink, replies, runtime) = gate_sink(trace.clone(), MODE_FAIL_DEFER);
    assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
    let cache = DefaultInMemoryCache::new();
    let capacity = sink.admission.permits.available_permits();
    assert!((1..=MAX_RESPONDER_IN_FLIGHT).contains(&capacity));

    // Submit successive failures, not a synchronous burst: non-waiting
    // pre-spawn admission must drop a burst's excess even when every ack
    // would fail. Each failed execution must return its permit so far more
    // than one bound's worth of interactions can attempt their defer.
    for index in 0..64 {
        send(&sink, &cache, burst_create(index));
        let expected = index + 1;
        wait_until("failed acknowledgement and its permit to return", || {
            replies.defer_started() == expected
                && replies.in_flight.load(Ordering::SeqCst) == 0
                && sink.admission.permits.available_permits() == capacity
        })
        .await;
    }
    settle(&replies).await;
    assert_eq!(replies.defer_started(), 64);
    assert_eq!(
        creates(&trace),
        0,
        "failed/ambiguous ack executed a mutation"
    );
    assert_eq!(replies.complete_started(), 0);

    // Later interactions run normally once acknowledgements succeed again.
    replies.set_mode(MODE_PASS);
    send(&sink, &cache, burst_setup(0));
    wait_until("later interaction to complete", || {
        replies.complete_started() == 1
    })
    .await;
}

#[test]
fn responder_permits_return_after_task_cancellation() {
    let trace = Trace::default();
    let (sink, replies, _runtime) = gate_sink(trace, MODE_HOLD);

    // Fill the bound with held executions on a throwaway runtime, then drop
    // that runtime: its spawned responder tasks are cancelled mid-reply.
    let thread_sink = Arc::clone(&sink);
    let thread_replies = Arc::clone(&replies);
    let cancelled = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("throwaway runtime");
        runtime.block_on(async {
            let cache = DefaultInMemoryCache::new();
            for index in 0..64 {
                send(&thread_sink, &cache, burst_create(index));
            }
            settle(&thread_replies).await;
        });
        drop(runtime);
    });
    cancelled.join().expect("cancellation thread");

    let admitted = replies.defer_started();
    assert!(
        (1..=MAX_RESPONDER_IN_FLIGHT).contains(&admitted),
        "pre-cancellation burst admitted {admitted} executions"
    );

    // Cancellation returned every permit: a fresh interaction on a new
    // runtime is admitted and completes against the same responder.
    replies.release();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("recovery runtime");
    runtime.block_on(async {
        let cache = DefaultInMemoryCache::new();
        send(&sink, &cache, burst_setup(0));
        wait_until("post-cancellation interaction to complete", || {
            replies.complete_started() == 1
        })
        .await;
    });
}

#[tokio::test]
async fn responder_kick_unrecognized_and_disabled_never_acknowledge() {
    let trace = Trace::default();
    let (sink, replies, _runtime) = gate_sink(trace.clone(), MODE_PASS);
    let cache = DefaultInMemoryCache::new();

    // `/kick` belongs to the shared router alone; answering it here would
    // race the router. Unrecognized commands are not voice work at all.
    send(
        &sink,
        &cache,
        voice_interaction(
            Some(command_data("kick", vec![user_option("member", 303)])),
            None,
            true,
        ),
    );
    send(
        &sink,
        &cache,
        voice_interaction(Some(command_data("other", Vec::new())), None, true),
    );
    settle(&replies).await;
    assert_eq!(
        replies.counters(),
        (0, 0, 0, 0),
        "responder acknowledged /kick or an unrecognized interaction"
    );

    // A disabled voice runtime acknowledges nothing, recognized or not.
    let factory_trace = trace.clone();
    let disabled = Arc::new(VoiceRuntime::new(
        move || {
            (
                Store::new(factory_trace.clone()),
                Http::new(factory_trace.clone()),
            )
        },
        Duration::from_millis(10),
        false,
    ));
    let disabled_sink: GateSink = VoiceResponder::new(disabled, Arc::clone(&replies));
    send(&disabled_sink, &cache, burst_create(0));
    settle(&replies).await;
    assert_eq!(replies.counters(), (0, 0, 0, 0));

    // Sanity: the same counters do move for a recognized interaction on the
    // enabled responder, so the zeroes above are routing, not a dead mock.
    send(&sink, &cache, burst_setup(0));
    wait_until("recognized interaction to complete", || {
        replies.complete_started() == 1
    })
    .await;
    assert_eq!(replies.defer_started(), 1);
}
