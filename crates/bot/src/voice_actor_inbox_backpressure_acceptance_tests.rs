//! Protected acceptance tests: the per-guild voice actor inbox is bounded
//! and drops fire-and-forget commands past its cap instead of buffering
//! forever.
//!
//! PROTECTED FILE: the paired Builder implements the production bound
//! without editing this file. The only wiring outside it is the
//! `#[path] mod actor_inbox_backpressure_acceptance;` line in
//! `voice_rooms_tests.rs`.
//!
//! Contract under test (all through the real `VoiceRuntime` + `GuildActor`,
//! offline fakes only):
//!
//! - Burst growth is bounded: with the drain unable to run, a burst of
//!   [`BURST`] fire-and-forget commands is refused past [`MAX_ACTOR_INBOX`].
//!   Today the inbox is an unbounded channel, so the burst tests fail on
//!   the refusal assertion (behavioral RED, not a compile failure).
//! - Overflow is a counted drop, never a panic: refused commands bump
//!   ``GuildActor::inbox_overflow_dropped`` and the runtime stays live: a
//!   `worker_status` probe behind a 10k burst is still answered.
//! - Healthy path is unchanged: under the cap every send admits, every
//!   effect lands (display names visible through the naming probe), and the
//!   status probe answers.
//!
//! Builder instructions (additive only; no channel topology change):
//!
//! - Keep `GuildActor.tx` as `mpsc::UnboundedSender<ActorCommand>` and add
//!   inherent `GuildActor::try_send_command(&self, command: ActorCommand)
//!   -> bool` with exactly this signature, permit-gated in the `AckPermit`
//!   shape: a bounded count of outstanding inbox entries, released as the
//!   actor loop receives. Method resolution prefers the inherent method
//!   over the adapter trait below, so these tests exercise the real bound
//!   with no test-file edit. (A bounded `Sender` would change `tx`'s type
//!   and every direct sender; the build slice explicitly excludes new
//!   channel topology, so gate, don't re-plumb.)
//! - Add inherent `GuildActor::inbox_overflow_dropped(&self) -> u64`,
//!   monotonic per actor, counting refused `try_send_command` calls.
//! - Pick any inbox cap in 8..=[`MAX_ACTOR_INBOX`] and document it on the
//!   inherent method. A smaller cap still passes: the burst tests assert
//!   lower bounds on refusals.
//! - Rewire the fire-and-forget send sites (`Reconcile`, `Display`,
//!   `RoomFactsChanged`, `Join`, `CreatorAdded`, `RefreshCreators`,
//!   `AccessChanged`, `NameSettingsChanged`) to `try_send_command`.
//!   Reply-bearing sends (`Status`, `Ownership`, `Limit`, `Privacy`,
//!   `NamingProbe`, `CreatorsProbe`) and `worker_status` stay direct and
//!   are never refused: a dropped reply hangs its caller, and the liveness
//!   test below pins that a status probe is answered after a burst.
//! - A refused send must not touch the channel and must bump the counter
//!   exactly once.
//!
//! Determinism: the burst tests run on the current-thread Tokio flavor and
//! the burst loop is synchronous (no yield), so the actor task cannot drain
//! mid-burst and the fill is exact. The liveness/status awaits yield, which
//! is what lets the drain run.

use super::*;
use std::time::Duration;
use tokio::sync::oneshot;

/// Ceiling on one guild actor's outstanding fire-and-forget inbox entries.
/// The Builder may choose a smaller cap; the burst tests assert lower
/// bounds so any cap in 8..=`MAX_ACTOR_INBOX` passes.
const MAX_ACTOR_INBOX: usize = 256;
/// Burst sized to overflow any compliant cap: 4x the ceiling.
const BURST: usize = 4 * MAX_ACTOR_INBOX;

/// Adapter: today's unbounded inbox admits every send, so the adapter sends
/// for real and reports admitted. After the Builder adds the inherent
/// `try_send_command`, method resolution prefers the inherent method and
/// the burst tests exercise the real bound. This is what makes RED
/// behavioral: the file compiles today and the assertions fail on current
/// behavior.
trait ActorTrySend {
    fn try_send_command(&self, command: ActorCommand) -> bool;
}

impl ActorTrySend for GuildActor {
    fn try_send_command(&self, command: ActorCommand) -> bool {
        self.tx.send(command).is_ok()
    }
}

/// Adapter: today nothing counts inbox overflow, so the adapter reports
/// zero. The inherent `inbox_overflow_dropped` supersedes it after the
/// build.
trait ActorOverflowCount {
    fn inbox_overflow_dropped(&self) -> u64 {
        0
    }
}

impl ActorOverflowCount for GuildActor {}

fn display(member_id: u64) -> ActorCommand {
    ActorCommand::Display {
        member_id,
        display: "x".to_owned(),
    }
}

fn spawned_runtime(trace: Trace) -> VoiceRuntime<Store, Http> {
    let runtime = test_runtime(trace);
    assert!(
        runtime.publish_snapshot(GUILD, snapshot(&[], vec![])),
        "snapshot must spawn the guild actor on an empty inbox"
    );
    runtime
}

#[tokio::test(flavor = "current_thread")]
async fn actor_inbox_burst_refuses_beyond_cap() {
    let trace = Trace::default();
    let runtime = spawned_runtime(trace);
    let actor = runtime.ensure_actor(GUILD).expect("live actor");
    let mut refused = 0usize;
    for _ in 0..BURST {
        if !actor.try_send_command(ActorCommand::Reconcile) {
            refused += 1;
        }
    }
    let minimum = BURST - MAX_ACTOR_INBOX;
    assert!(
        refused >= minimum,
        "burst of {BURST} past a cap of at most {MAX_ACTOR_INBOX} must refuse at least {minimum} commands, refused {refused}"
    );
    runtime.remove_guild(GUILD);
}

#[tokio::test(flavor = "current_thread")]
async fn actor_inbox_overflow_is_counted() {
    let trace = Trace::default();
    let runtime = spawned_runtime(trace);
    let actor = runtime.ensure_actor(GUILD).expect("live actor");
    for _ in 0..BURST {
        let _ = actor.try_send_command(ActorCommand::Reconcile);
    }
    let dropped = actor.inbox_overflow_dropped();
    let minimum = (BURST - MAX_ACTOR_INBOX) as u64;
    assert!(
        dropped >= minimum,
        "burst of {BURST} past a cap of at most {MAX_ACTOR_INBOX} must count at least {minimum} drops, counted {dropped}"
    );
    runtime.remove_guild(GUILD);
}

#[tokio::test(flavor = "current_thread")]
async fn actor_inbox_burst_never_panics_and_stays_live() {
    // Overflow pin: a 10k burst (drops or buffers) must neither panic nor
    // wedge the actor. The status probe travels the never-refused
    // reply-bearing path, so it is answered once the burst drains.
    let trace = Trace::default();
    let runtime = spawned_runtime(trace);
    let actor = runtime.ensure_actor(GUILD).expect("live actor");
    for member in 0..10_000u64 {
        let _ = actor.try_send_command(display(900_000 + member));
    }
    let status = tokio::time::timeout(Duration::from_secs(30), runtime.worker_status(GUILD))
        .await
        .expect("status probe answered after a 10k burst")
        .expect("live actor");
    assert!(
        !status.halted,
        "a burst must not halt the worker: {:?}",
        status.failures
    );
    runtime.remove_guild(GUILD);
}

#[tokio::test(flavor = "current_thread")]
async fn actor_inbox_healthy_path_admits_and_drains() {
    // Control: under the cap every send admits, every display effect lands
    // (visible through the naming probe in arrival order), and status
    // answers. Pins the cap floor (well above this load) and low-load
    // behavior.
    let trace = Trace::default();
    let runtime = spawned_runtime(trace);
    let actor = runtime.ensure_actor(GUILD).expect("live actor");
    for member in [MEMBER, MEMBER + 1, MEMBER + 2] {
        assert!(
            actor.try_send_command(display(member)),
            "fire-and-forget send under the cap must admit"
        );
    }
    let (probe, inbox) = oneshot::channel();
    assert!(
        actor.try_send_command(ActorCommand::NamingProbe(probe)),
        "naming probe under the cap must admit"
    );
    let (_, directory) = tokio::time::timeout(Duration::from_secs(5), inbox)
        .await
        .expect("naming probe answered")
        .expect("probe reply");
    for member in [MEMBER, MEMBER + 1, MEMBER + 2] {
        assert!(
            directory.knows(member),
            "display effect for {member} must land at low load"
        );
    }
    let status = tokio::time::timeout(Duration::from_secs(5), runtime.worker_status(GUILD))
        .await
        .expect("status probe answered")
        .expect("live actor");
    assert!(!status.halted);
    runtime.remove_guild(GUILD);
}

#[tokio::test(flavor = "current_thread")]
async fn actor_inbox_legacy_adapter_path_stays_available() {
    // Control pinning the legacy adapter path: qualified calls bypass any
    // inherent inbox gate by design, so this passes both before and after the
    // Builder adds inherent `try_send_command` / `inbox_overflow_dropped`. It
    // also keeps the adapter traits referenced once inherent methods shadow
    // unqualified calls, avoiding never-used warnings under `-D warnings`.
    let trace = Trace::default();
    let runtime = spawned_runtime(trace);
    let actor = runtime.ensure_actor(GUILD).expect("live actor");
    assert!(
        <GuildActor as ActorTrySend>::try_send_command(&actor, ActorCommand::Reconcile),
        "legacy adapter path must admit on a fresh actor"
    );
    assert_eq!(
        <GuildActor as ActorOverflowCount>::inbox_overflow_dropped(&actor),
        0,
        "legacy adapter reports zero overflow on a fresh actor"
    );
    runtime.remove_guild(GUILD);
}
