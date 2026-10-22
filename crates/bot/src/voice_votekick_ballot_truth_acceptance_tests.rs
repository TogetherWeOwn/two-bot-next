//! Protected acceptance suite: vote-kick ballots that tell the enforcement truth.
//!
//! A vote-kick ballot is a member-visible promise. Today three paths break
//! that promise: timer-expired/cancelled votes never re-render the ballot
//! ([`GuildRoomWorker::kick_refresh`] returns the terminal updates but the
//! actor discards them), a passed-then-skipped enforcement (protected or gone
//! target) leaves the "will be disconnected" ballot standing, and a
//! budget-exhausted unknown outcome audits `gave_up` while the ballot still
//! reads "passed". Members see live-looking buttons on settled votes and
//! removal promises Discord never carried out.
//!
//! This file pins the observable contract at the worker seams. Transport
//! observations come from the dispatch queue (a queued re-render/correction
//! is a decision the worker made), the fake REST trace, and the persisted
//! kick-audit rows; lifecycle driving uses [`LiveGuild`] exactly as gateway
//! frames would land it.
//!
//! Builder contract (this file is preserved verbatim; see the manifest on the
//! acceptance card):
//!
//! - Timer-path terminal settles (expired, cancelled, passed) leave a ballot
//!   re-render behind that a single [`GuildRoomWorker::dispatch_one`] drains.
//!   Cast-path settles must NOT queue one: the interaction layer already
//!   renders those in place, and the existing cast-settle pins
//!   (`promoted_target_mid_vote_cancels_and_enforces_nothing`,
//!   `unavailable_authority_mid_vote_cancels_and_enforces_nothing`) forbid it.
//! - Skipped (`skipped_target_protected`, `skipped_room_gone`) and `gave_up`
//!   enforcement completions leave a ballot correction behind that a single
//!   `dispatch_one` drains. Retries stay idempotent and budgeted: no
//!   correction before the terminal attempt, exactly one enforcement row.
//! - Re-render/correction dispatches write no enforcement audit rows, emit no
//!   deny/disconnect writes, and terminate; an unknown ballot ref completes
//!   quietly. The success path needs no correction (unpinned either way).
//! - Terminal ballot payloads stay an in-place update with both buttons
//!   disabled; the active ballot render is unchanged.
//! - The builder owns, with their own tests: the correction copy (it must
//!   name what happened — protected/gone target, unknown outcome — plus a
//!   bounded next action, and must never claim passed/disconnected), the
//!   ballot message-ref plumbing, and the edit transport.
//! - Updating two existing pins is part of the build:
//!   `target_leaving_cancels_the_vote_without_enforcement` and
//!   `vote_expires_after_two_minutes_without_enforcement` assert an empty
//!   queue after a timer settle, which contradicts the re-render. Narrow
//!   them to drain the re-render while keeping the no-enforcement-write
//!   assertions.
//!
//! The `ballot_truth_control_` tests use only today's behavior and pass on
//! the baseline; the six `ballot_truth_` pins fail there because no
//! re-render/correction decision exists yet.
//!
//! Skills used: test-driven-development.

use super::*;
use two_bot_core::voice_vote_kick::{VoteBallot, VoteCancellation, VOTE_KICK_TTL_MS};

type Worker = GuildRoomWorker<Store, Http>;

const OWNER: u64 = MEMBER;
const VOTER_A: u64 = 301;
const VOTER_B: u64 = 302;
const TARGET: u64 = 303;
const VOTER_C: u64 = 304;
const ROOM: u64 = 500;
const VOTE: u64 = 7_000;

fn occupant(member_id: u64) -> VoiceMember {
    VoiceMember {
        member_id,
        channel_id: ROOM,
        bot: Some(false),
    }
}

fn roster() -> Vec<VoiceMember> {
    [OWNER, VOTER_A, VOTER_B, TARGET, VOTER_C]
        .into_iter()
        .map(occupant)
        .collect()
}

async fn setup() -> (Worker, Trace) {
    let trace = Trace::default();
    let live = LiveGuild::new(GUILD);
    live.publish(snapshot(&[ROOM], roster()));
    let store = Store::new(trace.clone());
    store.rooms.lock().unwrap().insert(ROOM, room(ROOM));
    let worker = GuildRoomWorker::load(live, store, Http::new(trace.clone()))
        .await
        .unwrap();
    (worker, trace)
}

fn start(worker: &mut Worker, initiator: u64, target: u64) -> Result<VoteKickUpdate, KickRefusal> {
    worker.kick_start(VOTE, ROOM, initiator, target, 0)
}

/// Start a vote on `TARGET` and pass it with three Yes ballots.
fn pass_vote(worker: &mut Worker) {
    start(worker, VOTER_A, TARGET).unwrap();
    for voter in [VOTER_A, VOTER_B] {
        worker.kick_cast(VOTE, voter, VoteBallot::Yes, 1).unwrap();
    }
    let passed = worker.kick_cast(VOTE, VOTER_C, VoteBallot::Yes, 2).unwrap();
    assert_eq!(passed.status, VoteKickStatus::Passed);
}

fn stored_audit(worker: &Worker) -> Vec<KickAuditRow> {
    worker.store.kick_audit.lock().unwrap().clone()
}

fn enforcement_rows(worker: &Worker) -> Vec<KickAuditRow> {
    stored_audit(worker)
        .into_iter()
        .filter(|row| row.event == KickAuditEvent::Enforcement)
        .collect()
}

/// The Discord-visible parts of a ballot follow-up: response kind, content,
/// and each button's disabled flag in row order.
fn ballot_parts(update: &VoteKickUpdate) -> (InteractionResponseType, String, Vec<bool>) {
    let response = vote_update_message(update);
    let data = response.data.as_ref().expect("ballot payload");
    let content = data.content.clone().unwrap_or_default();
    let mut disabled = Vec::new();
    for component in data.components.clone().unwrap_or_default() {
        let Component::ActionRow(action_row) = component else {
            panic!("ballot follow-up must be a single button row");
        };
        for button in action_row.components {
            let Component::Button(button) = button else {
                panic!("ballot row must hold buttons only");
            };
            disabled.push(button.disabled);
        }
    }
    (response.kind, content, disabled)
}

fn base_update(status: VoteKickStatus) -> VoteKickUpdate {
    VoteKickUpdate {
        vote: VoteKickRef {
            id: VOTE,
            guild_id: GUILD,
            room_id: ROOM,
            target_id: TARGET,
        },
        status,
        progress: VoteProgress {
            yes: 0,
            required: 3,
            total: 4,
        },
        kick: None,
    }
}

#[tokio::test]
async fn ballot_truth_control_timer_surfaces_expired_update() {
    // Control: proves the fixture observes timer expiry (GREEN before/after).
    let (mut worker, _) = setup().await;
    start(&mut worker, VOTER_A, TARGET).unwrap();
    worker.kick_cast(VOTE, VOTER_A, VoteBallot::Yes, 1).unwrap();
    assert!(worker.kick_refresh(VOTE_KICK_TTL_MS - 1).is_empty());
    let finished = worker.kick_refresh(VOTE_KICK_TTL_MS);
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].status, VoteKickStatus::Expired);
    assert_eq!(finished[0].kick, None);
}

#[tokio::test]
async fn ballot_truth_control_timer_surfaces_cancelled_update() {
    // Control: proves the fixture observes timer cancellation (GREEN before/after).
    let (mut worker, _) = setup().await;
    start(&mut worker, VOTER_A, TARGET).unwrap();
    worker.kick_cast(VOTE, VOTER_A, VoteBallot::Yes, 1).unwrap();
    worker.live.voice_update(TARGET, None, Some(false));
    let finished = worker.kick_refresh(2);
    assert_eq!(finished.len(), 1);
    assert_eq!(
        finished[0].status,
        VoteKickStatus::Cancelled(VoteCancellation::TargetLeft)
    );
    assert_eq!(finished[0].kick, None);
}

#[test]
fn ballot_truth_control_terminal_ballots_render_disabled() {
    // Control: the re-render payload the queued decision will carry.
    for (status, phrase) in [
        (VoteKickStatus::Expired, "expired"),
        (
            VoteKickStatus::Cancelled(VoteCancellation::TargetLeft),
            "cancelled",
        ),
        (
            VoteKickStatus::Cancelled(VoteCancellation::TargetProtected),
            "cancelled",
        ),
        (VoteKickStatus::Passed, "passed"),
    ] {
        let (kind, content, disabled) = ballot_parts(&base_update(status));
        assert_eq!(kind, InteractionResponseType::UpdateMessage);
        assert!(
            content.to_lowercase().contains(phrase),
            "terminal ballot must name its outcome: {content:?}"
        );
        assert_eq!(disabled, [true, true], "settled ballot: {content:?}");
    }
    let (kind, _, disabled) = ballot_parts(&base_update(VoteKickStatus::Active));
    assert_eq!(kind, InteractionResponseType::UpdateMessage);
    assert_eq!(disabled, [false, false], "an active ballot stays votable");
}

#[test]
fn ballot_truth_control_passed_ballot_promises_removal() {
    // Control: documents the claim a skip/gave-up correction must supersede.
    let (_, content, _) = ballot_parts(&base_update(VoteKickStatus::Passed));
    assert!(
        content.contains("disconnected"),
        "passed ballot must promise removal today: {content:?}"
    );
}

#[tokio::test]
async fn ballot_truth_control_successful_enforcement_unchanged() {
    // Control: the success path keeps its writes and audit row (GREEN before/after).
    let (mut worker, trace) = setup().await;
    pass_vote(&mut worker);
    assert!(worker.dispatch_one(3).await);
    assert_eq!(
        *trace.lock().unwrap(),
        [
            format!("deny:{ROOM}:{TARGET}"),
            format!("disconnect:{GUILD}:{TARGET}")
        ]
    );
    assert!(worker.flush_kick_audit(4).await);
    let enforcement = enforcement_rows(&worker);
    assert_eq!(enforcement.len(), 1);
    assert_eq!(enforcement[0].outcome, "connect_denied_and_disconnected");
    assert_eq!(
        (
            enforcement[0].vote_id,
            enforcement[0].initiator_id,
            enforcement[0].target_id
        ),
        (VOTE, VOTER_A, TARGET)
    );
}

#[tokio::test]
async fn ballot_truth_timer_expiry_queues_ballot_rerender() {
    let (mut worker, _) = setup().await;
    start(&mut worker, VOTER_A, TARGET).unwrap();
    worker.kick_cast(VOTE, VOTER_A, VoteBallot::Yes, 1).unwrap();
    let finished = worker.kick_refresh(VOTE_KICK_TTL_MS);
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].status, VoteKickStatus::Expired);
    assert!(worker.flush_kick_audit(VOTE_KICK_TTL_MS + 1).await);
    assert_eq!(
        stored_audit(&worker)
            .iter()
            .map(|row| (row.event, row.outcome))
            .collect::<Vec<_>>(),
        [
            (KickAuditEvent::VoteStarted, "started"),
            (KickAuditEvent::VoteResult, "expired"),
        ]
    );
    assert!(
        enforcement_rows(&worker).is_empty(),
        "expiry never enforces"
    );
    assert!(
        worker.dispatch_one(VOTE_KICK_TTL_MS + 2).await,
        "a timer-expired vote must queue a ballot re-render, or the ballot keeps live-looking buttons"
    );
}

#[tokio::test]
async fn ballot_truth_timer_cancel_queues_ballot_rerender() {
    let (mut worker, _) = setup().await;
    start(&mut worker, VOTER_A, TARGET).unwrap();
    worker.kick_cast(VOTE, VOTER_A, VoteBallot::Yes, 1).unwrap();
    worker.live.voice_update(TARGET, None, Some(false));
    let finished = worker.kick_refresh(2);
    assert_eq!(finished.len(), 1);
    assert_eq!(
        finished[0].status,
        VoteKickStatus::Cancelled(VoteCancellation::TargetLeft)
    );
    assert!(worker.flush_kick_audit(3).await);
    assert_eq!(
        stored_audit(&worker)
            .iter()
            .map(|row| (row.event, row.outcome))
            .collect::<Vec<_>>(),
        [
            (KickAuditEvent::VoteStarted, "started"),
            (KickAuditEvent::VoteResult, "cancelled_target_left"),
        ]
    );
    assert!(
        enforcement_rows(&worker).is_empty(),
        "cancellation never enforces"
    );
    assert!(
        worker.dispatch_one(4).await,
        "a timer-cancelled vote must queue a ballot re-render, or the ballot keeps live-looking buttons"
    );
}

#[tokio::test]
async fn ballot_truth_timer_pass_queues_rerender_beside_enforcement() {
    // Two Yes ballots, then OWNER and VOTER_C leave: the eligible set drops
    // to {A, B}, required drops to 2, and the timer pass carries both the
    // enforcement and the ballot re-render.
    let (mut worker, trace) = setup().await;
    let started = start(&mut worker, VOTER_A, TARGET).unwrap();
    assert_eq!(started.progress.required_total_text(), "3/4");
    for voter in [VOTER_A, VOTER_B] {
        let update = worker.kick_cast(VOTE, voter, VoteBallot::Yes, 1).unwrap();
        assert_eq!(update.status, VoteKickStatus::Active);
    }
    worker.live.voice_update(OWNER, None, Some(false));
    worker.live.voice_update(VOTER_C, None, Some(false));
    let finished = worker.kick_refresh(5);
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].status, VoteKickStatus::Passed);
    assert!(finished[0].kick.is_some());
    assert!(worker.dispatch_one(6).await, "setup: enforcement drains");
    assert!(
        worker.dispatch_one(7).await,
        "a timer-passed vote must queue a ballot re-render beside its enforcement"
    );
    let calls = trace.lock().unwrap().clone();
    assert_eq!(
        calls
            .iter()
            .filter(|call| *call == &format!("deny:{ROOM}:{TARGET}"))
            .count(),
        1,
        "enforcement runs exactly once: {calls:?}"
    );
    assert_eq!(
        calls
            .iter()
            .filter(|call| *call == &format!("disconnect:{GUILD}:{TARGET}"))
            .count(),
        1,
        "enforcement runs exactly once: {calls:?}"
    );
    assert!(worker.flush_kick_audit(8).await);
    let enforcement = enforcement_rows(&worker);
    assert_eq!(enforcement.len(), 1);
    assert_eq!(enforcement[0].outcome, "connect_denied_and_disconnected");
}

#[tokio::test]
async fn ballot_truth_skipped_protected_target_corrects_passed_ballot() {
    let (mut worker, trace) = setup().await;
    pass_vote(&mut worker);
    worker.rooms.get_mut(&ROOM).unwrap().owner_id = TARGET;
    assert!(worker.dispatch_one(3).await, "setup: enforcement drains");
    assert!(
        trace.lock().unwrap().is_empty(),
        "a protected target is not touched"
    );
    assert!(worker.flush_kick_audit(4).await);
    let enforcement = enforcement_rows(&worker);
    assert_eq!(enforcement.len(), 1);
    assert_eq!(enforcement[0].outcome, "skipped_target_protected");
    assert_eq!(
        (
            enforcement[0].vote_id,
            enforcement[0].initiator_id,
            enforcement[0].target_id
        ),
        (VOTE, VOTER_A, TARGET),
        "the skip audit row is preserved"
    );
    assert!(
        worker.dispatch_one(5).await,
        "a skipped enforcement must queue a ballot correction, or the ballot keeps promising a removal that never happened"
    );
}

#[tokio::test]
async fn ballot_truth_skipped_gone_room_corrects_passed_ballot() {
    let (mut worker, trace) = setup().await;
    pass_vote(&mut worker);
    worker.rooms.remove(&ROOM);
    assert!(worker.dispatch_one(3).await, "setup: enforcement drains");
    assert!(
        trace.lock().unwrap().is_empty(),
        "a gone room is not touched"
    );
    assert!(worker.flush_kick_audit(4).await);
    let enforcement = enforcement_rows(&worker);
    assert_eq!(enforcement.len(), 1);
    assert_eq!(enforcement[0].outcome, "skipped_room_gone");
    assert_eq!(
        (
            enforcement[0].vote_id,
            enforcement[0].initiator_id,
            enforcement[0].target_id
        ),
        (VOTE, VOTER_A, TARGET),
        "the skip audit row is preserved"
    );
    assert!(
        worker.dispatch_one(5).await,
        "a skipped enforcement must queue a ballot correction, or the ballot keeps promising a removal that never happened"
    );
}

#[tokio::test]
async fn ballot_truth_gave_up_enforcement_queues_correction() {
    let (mut worker, _trace) = setup().await;
    pass_vote(&mut worker);
    {
        let mut errors = worker.http.kick_errors.lock().unwrap();
        for _ in 0..QUEUE_MAX_ATTEMPTS {
            errors.push_back(RoomHttpError::UnknownOutcome);
        }
    }
    let mut now = 3;
    for attempt in 1..=QUEUE_MAX_ATTEMPTS {
        now += 10_000_000;
        assert!(
            worker.dispatch_one(now).await,
            "setup: retry attempt {attempt} drains"
        );
    }
    assert!(worker.flush_kick_audit(now + 1).await);
    let enforcement = enforcement_rows(&worker);
    assert_eq!(enforcement.len(), 1);
    assert_eq!(enforcement[0].outcome, "gave_up");
    assert!(
        worker.dispatch_one(now + 2).await,
        "a gave-up enforcement must queue a ballot correction, or the ballot keeps reading passed for an unknown outcome"
    );
}
