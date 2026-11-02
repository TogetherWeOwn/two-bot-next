//! Acceptance tests: the guild voice actor makes dispatch and maintenance
//! progress under continuous inbox traffic.
//!
//! The actor loop uses a biased `select!` with `inbox.recv()` first, ahead of
//! the ready dispatch branch, the timer tick and the maintenance branch. While
//! the inbox stays ready it wins every iteration, so sustained benign
//! voice-state/display churn can starve already-ready create/move dispatch and
//! due timer/maintenance even while command receipt keeps updating the
//! progress stamp. The dispatch budget only decrements when dispatch runs, so
//! it does not bound command-drain turns.
//!
//! Builder contract (this file is preserved verbatim; see the manifest on the
//! acceptance card):
//!
//! - Bound consecutive inbox-drain turns with a deterministic fair scheduling
//!   policy so already-ready dispatch and due timer/maintenance each get a
//!   turn under continuously ready inbox traffic. Do not rely on random
//!   `select!` fairness as the only oracle: these tests use FIFO Status and
//!   creator probes as deterministic turn counters, never timing races.
//! - The bound lives in one source of truth: a single constant in
//!   `voice_rooms.rs` (for example `VOICE_ACTOR_MAX_CONSECUTIVE_INBOX_TURNS`)
//!   with a value from 1 through 32 inclusive. The behaviour tests below pin
//!   at most 32 inbox turns before any ready non-inbox branch runs, and at
//!   most 64 for a chained timer-then-dispatch or timer-then-maintenance
//!   round.
//! - Preserve command arrival order (FIFO), current-live-state and stale-ticket
//!   fencing, and the no-maintenance-between-create-and-priority-move
//!   invariant. Stop/shutdown stays responsive; no in-flight Discord mutation
//!   is dropped or cancelled to force fairness. Dispatch turns still run to
//!   completion (observe-and-recover, never future-dropping).
//! - Expose a compact agent-facing diagnosis without high-cardinality labels
//!   or raw member data: a gauge
//!   `two_bot_voice_actor_inbox_turn_bound` reporting the bound, plus counters
//!   `two_bot_voice_actor_inbox_turns_total` (every processed inbox command)
//!   and `two_bot_voice_actor_dispatch_turns_total` (every dispatch turn that
//!   ran). The metrics test pins these exact series names.
//! - No new public command and no flood-sensitive per-turn log spam. Benign
//!   churn stays silent; humans keep timely room moves and ordinary failure
//!   replies.
//!
//! Oracle design: the inbox is an FIFO unbounded channel, so a Status or
//! creator probe sent after N churn commands replies only after those N
//! commands were processed. The probe reply content itself (tracked rooms,
//! creator ids) proves whether dispatch or maintenance interleaved before the
//! probe, with no timing race. Tests that must read shared REST/store state at
//! probe time hang the inbox for one bounded revalidate timeout behind the
//! probe, so the actor cannot advance past the probe before the check.
//!
//! The `control_` tests use only today's seams and pass on the baseline; the
//! remaining `fair_` and `scheduling_` tests are the failing-first matrix.
//!
//! Skills used: test-driven-development.

use super::*;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The acceptance bound: at most this many consecutive inbox turns before any
/// ready dispatch, timer or maintenance branch runs. Chained rounds
/// (timer-then-dispatch, timer-then-maintenance) complete within twice this.
const MAX_INBOX_TURNS_BEFORE_PROGRESS: usize = 32;

/// Chained bound for timer-plus-dispatch and timer-plus-maintenance rounds.
const MAX_INBOX_TURNS_BEFORE_CHAINED_PROGRESS: usize = 2 * MAX_INBOX_TURNS_BEFORE_PROGRESS;

/// Flood member ids start here so they never collide with the fixed `MEMBER`
/// (300), `CREATOR` (200) or probe targets used below.
const CHURN_BASE: u64 = 10_000;

/// Target member for the latest-state Display pair.
const DIRECTORY_TARGET: u64 = 5_555;

/// A channel id the creator map never holds, for the inbox-hang Join.
const UNKNOWN_CHANNEL: u64 = 999;

/// Send `count` benign Display churn commands starting at `base`.
fn send_churn(tx: &mpsc::UnboundedSender<ActorCommand>, base: u64, count: usize) {
    for offset in 0..count {
        let member = base + offset as u64;
        tx.send(ActorCommand::Display {
            member_id: member,
            display: format!("churn {member}"),
        })
        .expect("inbox must accept churn");
    }
}

/// A Join ticket for `member` into `CREATOR` on the worker's live view.
fn creator_ticket(worker: &GuildRoomWorker<Store, Http>, member: u64) -> JoinTicket {
    worker
        .live
        .voice_update(member, Some(CREATOR), Some(false))
        .expect("ticket for a joined creator channel")
}

/// Send one accepted Join for `member` with `display` through the inbox.
fn send_join(
    tx: &mpsc::UnboundedSender<ActorCommand>,
    ticket: JoinTicket,
    display: &str,
    seed: u64,
) {
    tx.send(ActorCommand::Join {
        ticket,
        display: display.to_owned(),
        seed,
        created_at: NOW.to_owned(),
        received: tokio::time::Instant::now(),
    })
    .expect("inbox must accept the join");
}

/// Await a Status probe with a generous bound so a wedged actor fails fast
/// instead of hanging the suite.
async fn await_status(rx: oneshot::Receiver<WorkerStatus>, label: &str) -> WorkerStatus {
    tokio::time::timeout(Duration::from_secs(10), rx)
        .await
        .unwrap_or_else(|_| panic!("{label} must reply within 10 s"))
        .expect("status oneshot must deliver")
}

/// Await a creator probe with the same wedge guard.
async fn await_creators(rx: oneshot::Receiver<Vec<Snowflake>>, label: &str) -> Vec<Snowflake> {
    tokio::time::timeout(Duration::from_secs(10), rx)
        .await
        .unwrap_or_else(|_| panic!("{label} must reply within 10 s"))
        .expect("creators oneshot must deliver")
}

/// Await a naming probe (settings plus a directory snapshot) with the same
/// wedge guard. Only the directory is pinned; the settings travel along.
async fn await_naming(
    rx: oneshot::Receiver<(NameSettings, NameDirectory)>,
    label: &str,
) -> NameDirectory {
    tokio::time::timeout(Duration::from_secs(10), rx)
        .await
        .unwrap_or_else(|_| panic!("{label} must reply within 10 s"))
        .expect("naming oneshot must deliver")
        .1
}

/// Spawn the actor loop on `worker` with the standard 100 ms tick. The first
/// timer tick is immediately ready, so timer/maintenance tests need no clock
/// advance on live time.
fn spawn_actor(
    worker: GuildRoomWorker<Store, Http>,
    rx: mpsc::UnboundedReceiver<ActorCommand>,
) -> tokio::task::JoinHandle<GuildRoomWorker<Store, Http>> {
    tokio::spawn(async move {
        let mut worker = worker;
        // These tests drive a raw channel (no permits acquired), so the
        // loop's releases saturate against this throwaway ledger.
        let backpressure = ActorInboxBackpressure::default();
        run_actor_with_limits(
            &mut worker,
            rx,
            Duration::from_millis(100),
            ActorLimits::default(),
            &backpressure,
        )
        .await;
        worker
    })
}

// --- failing-first: bounded dispatch progress -------------------------------

#[tokio::test]
async fn fair_create_and_move_progress_within_bounded_inbox_turns() {
    // A queued create/move pair must dispatch within the bound despite
    // continuously ready Display churn. S1 (after 32 churn turns) already sees
    // the created room; S2 (after 32 more) already sees the move on the REST
    // trace. The trailing unknown Join hangs the inbox for one bounded
    // revalidate timeout behind S2, so the actor cannot advance past S2
    // before the trace check.
    let (live, store, http, trace) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let ticket = creator_ticket(&worker, MEMBER);
    // Known creators skip the store read, so only the trailing unknown Join
    // pays this delay (and resolves at the 1 s revalidate bound).
    worker.store.creator_for_delay = Some(Duration::from_secs(30));
    let (tx, rx) = mpsc::unbounded_channel();
    send_join(&tx, ticket, "new room", 7);
    send_churn(&tx, CHURN_BASE, MAX_INBOX_TURNS_BEFORE_PROGRESS);
    let (s1_tx, s1_rx) = oneshot::channel();
    tx.send(ActorCommand::Status(s1_tx)).unwrap();
    send_churn(&tx, CHURN_BASE + 1_000, MAX_INBOX_TURNS_BEFORE_PROGRESS);
    let (s2_tx, s2_rx) = oneshot::channel();
    tx.send(ActorCommand::Status(s2_tx)).unwrap();
    let blocking = JoinTicket {
        creator_id: UNKNOWN_CHANNEL,
        ..ticket
    };
    send_join(&tx, blocking, "blocked", 8);

    let handle = spawn_actor(worker, rx);
    let first = await_status(s1_rx, "S1").await;
    assert_eq!(
        first.tracked_rooms, 1,
        "the create must dispatch within {MAX_INBOX_TURNS_BEFORE_PROGRESS} inbox turns"
    );
    await_status(s2_rx, "S2").await;
    let calls = trace.lock().unwrap().clone();
    assert!(
        calls.iter().any(|call| call == &format!("move:{MEMBER}:500")),
        "the priority move must dispatch within {MAX_INBOX_TURNS_BEFORE_PROGRESS} more inbox turns: {calls:?}"
    );
    drop(tx);
    let worker = tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("actor must exit after inbox close")
        .unwrap();
    assert_eq!(worker.tracked().len(), 1);
}

// --- failing-first: bounded maintenance progress ----------------------------

#[tokio::test]
async fn fair_maintenance_creator_merge_within_bounded_inbox_turns() {
    // A flagged creator merge (timer schedules it, maintenance runs it) must
    // land within the chained bound despite continuously ready churn. The
    // creator probe after 64 churn turns already sees the new row.
    const NEW_CREATOR: u64 = 201;
    let (live, store, http, _) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker
        .store
        .creators
        .lock()
        .unwrap()
        .push(CreatorChannel::new(GUILD, NEW_CREATOR));
    worker.creators_refresh_due = true;
    let (tx, rx) = mpsc::unbounded_channel();
    send_churn(&tx, CHURN_BASE, MAX_INBOX_TURNS_BEFORE_CHAINED_PROGRESS);
    let (probe_tx, probe_rx) = oneshot::channel();
    tx.send(ActorCommand::CreatorsProbe(probe_tx)).unwrap();
    // Keep the inbox ready past the probe so a drain-then-maintain loop
    // cannot pass by servicing maintenance right after the probe.
    send_churn(&tx, CHURN_BASE + 5_000, 32);
    let (status_tx, status_rx) = oneshot::channel();
    tx.send(ActorCommand::Status(status_tx)).unwrap();

    let handle = spawn_actor(worker, rx);
    let ids = await_creators(probe_rx, "creator probe").await;
    assert!(
        ids.contains(&NEW_CREATOR),
        "the flagged creator merge must land within {MAX_INBOX_TURNS_BEFORE_CHAINED_PROGRESS} inbox turns: {ids:?}"
    );
    await_status(status_rx, "trailing status").await;
    drop(tx);
    tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("actor must exit after inbox close")
        .unwrap();
}

#[tokio::test]
async fn fair_empty_room_delete_within_bounded_inbox_turns() {
    // Timer reconcile queues the empty-room delete and dispatch runs it; both
    // must happen within the chained bound under churn. S1 (after 64 churn
    // turns) already sees zero tracked rooms.
    let (live, store, http, _) = owned_room();
    let worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.live.voice_update(MEMBER, None, Some(false));
    worker.live.voice_update(301, None, Some(false));
    worker.live.set_empty_grace(Duration::ZERO);
    let (tx, rx) = mpsc::unbounded_channel();
    send_churn(&tx, CHURN_BASE, MAX_INBOX_TURNS_BEFORE_CHAINED_PROGRESS);
    let (s1_tx, s1_rx) = oneshot::channel();
    tx.send(ActorCommand::Status(s1_tx)).unwrap();
    send_churn(&tx, CHURN_BASE + 5_000, 32);
    let (s2_tx, s2_rx) = oneshot::channel();
    tx.send(ActorCommand::Status(s2_tx)).unwrap();

    let handle = spawn_actor(worker, rx);
    let first = await_status(s1_rx, "S1").await;
    assert_eq!(
        first.tracked_rooms, 0,
        "the empty-room delete must reconcile and dispatch within {MAX_INBOX_TURNS_BEFORE_CHAINED_PROGRESS} inbox turns"
    );
    await_status(s2_rx, "S2").await;
    drop(tx);
    tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("actor must exit after inbox close")
        .unwrap();
}

#[tokio::test]
async fn fair_notice_maintenance_within_bounded_inbox_turns() {
    // A queued failure notice (maintenance sends it) must go out within the
    // chained bound under churn. The shared REST handle already holds the
    // notice when S1 replies; the trailing unknown Join hangs the inbox for
    // one bounded revalidate timeout so the actor cannot advance past S1
    // before the check.
    let mut worker = failed_worker(LoggingSettings::default(), Some(SYSTEM_CHANNEL)).await;
    assert_eq!(worker.failures().len(), 1);
    worker.store.creator_for_delay = Some(Duration::from_secs(30));
    let http_probe = Arc::clone(&worker.http);
    // MEMBER already joined during the failed-worker setup, so a second
    // ticket for MEMBER would be a same-channel update (None). The blocking
    // join only needs a valid ticket shape with an unknown creator id.
    let ticket = creator_ticket(&worker, MEMBER + 1);
    let (tx, rx) = mpsc::unbounded_channel();
    send_churn(&tx, CHURN_BASE, MAX_INBOX_TURNS_BEFORE_CHAINED_PROGRESS);
    let (s1_tx, s1_rx) = oneshot::channel();
    tx.send(ActorCommand::Status(s1_tx)).unwrap();
    let blocking = JoinTicket {
        creator_id: UNKNOWN_CHANNEL,
        ..ticket
    };
    send_join(&tx, blocking, "blocked", 9);

    let handle = spawn_actor(worker, rx);
    await_status(s1_rx, "S1").await;
    let notices = http_probe.notices.lock().unwrap().clone();
    assert!(
        !notices.is_empty(),
        "the queued failure notice must send within {MAX_INBOX_TURNS_BEFORE_CHAINED_PROGRESS} inbox turns"
    );
    drop(tx);
    tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("actor must exit after inbox close")
        .unwrap();
}

// --- failing-first: bound source of truth and diagnosis ---------------------

#[tokio::test]
async fn scheduling_bound_and_diagnosis_exposed() {
    // The bound is one gauge and inbox activity is distinguishable from
    // successful dispatch progress, with no high-cardinality labels or member
    // data. Counters use lower bounds so parallel suites cannot flake them.
    let inbox_before = global_series("two_bot_voice_actor_inbox_turns_total");
    let dispatch_before = global_series("two_bot_voice_actor_dispatch_turns_total");
    let (live, store, http, trace) = fixture();
    let worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let ticket = creator_ticket(&worker, MEMBER);
    let (tx, rx) = mpsc::unbounded_channel();
    send_join(&tx, ticket, "new room", 7);
    send_churn(&tx, CHURN_BASE, 50);
    let handle = spawn_actor(worker, rx);
    // The inbox stays open while the create dispatches: closing it first
    // would break the actor loop before any dispatch turn runs.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if trace.lock().unwrap().iter().any(|call| call == "create") {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            panic!(
                "the create must dispatch without continuous churn: {:?}",
                trace.lock().unwrap()
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    drop(tx);
    let worker = tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("actor must exit after inbox close")
        .unwrap();
    assert_eq!(
        worker.tracked().len(),
        1,
        "the drained run must still create the room"
    );

    let inbox_after = global_series("two_bot_voice_actor_inbox_turns_total");
    assert!(
        inbox_after >= inbox_before + 50,
        "inbox turns must count processed commands ({inbox_before} -> {inbox_after})"
    );
    let dispatch_after = global_series("two_bot_voice_actor_dispatch_turns_total");
    assert!(
        dispatch_after >= dispatch_before + 2,
        "dispatch turns must count the create and the move ({dispatch_before} -> {dispatch_after})"
    );
    let render = metrics::global().render(None);
    let bound_line = render
        .lines()
        .find(|line| line.starts_with("two_bot_voice_actor_inbox_turn_bound "));
    let bound: u64 = bound_line
        .unwrap_or_else(|| panic!("the inbox-turn bound gauge must exist:\n{render}"))
        .split_whitespace()
        .last()
        .expect("gauge line carries a value")
        .parse()
        .expect("gauge value parses");
    assert!(
        (1..=MAX_INBOX_TURNS_BEFORE_PROGRESS as u64).contains(&bound),
        "the bound gauge must report the single source of truth (1..=32), got {bound}"
    );
    assert!(
        !render.contains("two_bot_voice_actor_inbox_turns_total{"),
        "inbox turns carry no high-cardinality labels"
    );
    assert!(
        !render.contains("two_bot_voice_actor_dispatch_turns_total{"),
        "dispatch turns carry no high-cardinality labels"
    );
}

// --- controls: order, fencing and lifecycle are preserved -------------------

#[tokio::test]
async fn control_fifo_and_latest_state_preserved_under_churn() {
    // Fair scheduling must not reorder the inbox: two joins separated by
    // churn still create in arrival order, and the later Display for one
    // member still wins. The directory is snapshotted with a NamingProbe in
    // FIFO order behind the churn: the timer naming pass prunes entries for
    // members outside voice, so the post-exit directory depends on whether
    // a timer turn ran before close and only the probe snapshot pins exact
    // contents.
    let (live, store, http, trace) = fixture();
    let worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let first_ticket = creator_ticket(&worker, MEMBER);
    let second_ticket = creator_ticket(&worker, MEMBER + 1);
    let (tx, rx) = mpsc::unbounded_channel();
    send_join(&tx, first_ticket, "first room", 7);
    send_churn(&tx, CHURN_BASE, 50);
    send_join(&tx, second_ticket, "second room", 8);
    tx.send(ActorCommand::Display {
        member_id: DIRECTORY_TARGET,
        display: "first".to_owned(),
    })
    .unwrap();
    tx.send(ActorCommand::Display {
        member_id: DIRECTORY_TARGET,
        display: "second".to_owned(),
    })
    .unwrap();
    let (naming_tx, naming_rx) = oneshot::channel();
    tx.send(ActorCommand::NamingProbe(naming_tx)).unwrap();

    let handle = spawn_actor(worker, rx);
    // FIFO order guarantees every churn Display and both joins were applied
    // before the probe replies, and no dispatch, timer or maintenance turn
    // can interleave while the preloaded inbox drains.
    let probed = await_naming(naming_rx, "naming probe").await;
    let mut expected = NameDirectory::default();
    for offset in 0..50 {
        let member = CHURN_BASE + offset;
        expected.insert(member, format!("churn {member}"));
    }
    expected.insert(DIRECTORY_TARGET, "second".to_owned());
    // Accepted joins also record the join display in the directory.
    expected.insert(MEMBER, "first room".to_owned());
    expected.insert(MEMBER + 1, "second room".to_owned());
    assert_eq!(
        probed, expected,
        "the later Display for one member still wins"
    );
    // The inbox stays open while dispatch drains: closing it first would
    // break the actor loop before any dispatch turn runs.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let creates = trace
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.as_str() == "create")
            .count();
        if creates >= 2 {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            panic!(
                "both joins must dispatch without continuous churn: {:?}",
                trace.lock().unwrap()
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    drop(tx);
    let worker = tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("actor must exit after inbox close")
        .unwrap();
    assert_eq!(worker.tracked().len(), 2);
    // V1 naming renders `<display>'s room`; the pin is the arrival order.
    assert_eq!(
        *worker.http.created_names.lock().unwrap(),
        [
            "first room's room".to_owned(),
            "second room's room".to_owned()
        ],
        "joins separated by churn still create in arrival order"
    );
    // No post-exit directory pin: refresh_template_names prunes non-voice
    // entries on the timer branch, which may or may not run before close.
}

#[tokio::test]
async fn control_stale_ticket_fenced_under_churn() {
    // A ticket for a member who already left is refused against current live
    // state even with churn ahead of it: no room is created.
    let (live, store, http, trace) = fixture();
    let worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let ticket = creator_ticket(&worker, MEMBER);
    worker.live.voice_update(MEMBER, None, Some(false));
    let (tx, rx) = mpsc::unbounded_channel();
    send_churn(&tx, CHURN_BASE, 50);
    send_join(&tx, ticket, "new room", 7);
    let (s1_tx, s1_rx) = oneshot::channel();
    tx.send(ActorCommand::Status(s1_tx)).unwrap();

    let handle = spawn_actor(worker, rx);
    let status = await_status(s1_rx, "S1").await;
    assert_eq!(
        status.tracked_rooms, 0,
        "a stale ticket must not create a room"
    );
    drop(tx);
    tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("actor must exit after inbox close")
        .unwrap();
    assert!(
        !trace.lock().unwrap().iter().any(|call| call == "create"),
        "no Discord create for a stale ticket: {:?}",
        trace.lock().unwrap()
    );
}

#[tokio::test]
async fn control_create_move_adjacency_no_maintenance_between() {
    // Without churn, the priority move still dispatches before any
    // maintenance store read: at move time the settings snapshot count is
    // still zero, and maintenance runs afterwards.
    let (live, mut store, http, _) = fixture();
    store.persist_error = Some(StoreError::Unavailable);
    *store.logging.lock().unwrap() = LoggingSettings::default();
    live.refresh_bot(notice_bot(Some(SYSTEM_CHANNEL)));
    live.upsert_channel(channel(650, 0, None));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    dispatch(&mut worker, 1).await;
    assert_eq!(worker.failures().len(), 1);
    worker.store.persist_error = None;
    join(&mut worker, MEMBER + 1);

    let config_reads = Arc::clone(&worker.store.config_reads);
    let reads_before = config_reads.load(Ordering::Relaxed);
    let move_saw_reads = Arc::new(Mutex::new(None));
    let probe_reads = Arc::clone(&config_reads);
    let probe_slot = Arc::clone(&move_saw_reads);
    Arc::get_mut(&mut worker.http)
        .expect("the worker holds the sole REST handle")
        .before_move = Some(Arc::new(move || {
        *probe_slot.lock().unwrap() = Some(probe_reads.load(Ordering::Relaxed));
    }));

    let (tx, rx) = mpsc::unbounded_channel::<ActorCommand>();
    // The inbox stays open but empty, so the immediately-ready timer, both
    // dispatch turns and then maintenance all run. The sender drops only
    // after maintenance is observed, letting the actor exit on close.
    let handle = spawn_actor(worker, rx);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if config_reads.load(Ordering::Relaxed) > reads_before {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("maintenance must still run after the pair");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    drop(tx);
    let worker = tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("actor must exit after inbox close")
        .unwrap();
    // The persist failure compensated the first room (create then delete), so
    // only the second room is tracked; its create->move pair is the adjacency
    // the probe pins.
    assert_eq!(worker.tracked().len(), 1);
    assert_eq!(
        *move_saw_reads.lock().unwrap(),
        Some(reads_before),
        "no maintenance store read may interpose between create and move"
    );
    assert!(
        config_reads.load(Ordering::Relaxed) > reads_before,
        "maintenance still runs after the pair"
    );
}

#[tokio::test]
async fn control_shutdown_responsive_with_queued_churn() {
    // Closing the inbox with 500 churn commands queued still stops the actor
    // promptly: shutdown never waits on a fairness budget.
    let (live, store, http, _) = fixture();
    let worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    send_churn(&tx, CHURN_BASE, 500);
    let handle = spawn_actor(worker, rx);
    drop(tx);
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("the actor must exit promptly on inbox close")
        .unwrap();
}

#[tokio::test]
async fn control_in_flight_dispatch_not_dropped_for_fairness() {
    // A dispatch hung mid-write still runs to completion when churn arrives
    // while it is held: fairness never cancels an in-flight Discord mutation.
    let (live, store, mut http, trace) = owned_room();
    let gate = Arc::new(LimitGate::default());
    http.limit_gate = Some(Arc::clone(&gate));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let super::limit::LimitReply::Queued { .. } =
        worker.apply_limit(MEMBER, false, LimitCommand::Limit(LimitArg::Value(3)))
    else {
        panic!("the limit must queue");
    };
    worker.queue.enqueue(
        GUILD,
        RoomAction::RenameRoom {
            channel_id: 500,
            name: "den".to_owned(),
        },
    );
    let (tx, rx) = mpsc::unbounded_channel();
    let handle = spawn_actor(worker, rx);
    tokio::time::timeout(Duration::from_secs(5), gate.sent.notified())
        .await
        .expect("the limit write must go live");
    // Churn lands while the write is held.
    send_churn(&tx, CHURN_BASE, 200);
    gate.release.notify_one();
    let mut done = false;
    for _ in 0..200 {
        if trace
            .lock()
            .unwrap()
            .iter()
            .any(|entry| entry == "limit:500:3")
        {
            done = true;
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(done, "the held write must land after release");
    drop(tx);
    tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("actor must exit after inbox close")
        .unwrap();
}
