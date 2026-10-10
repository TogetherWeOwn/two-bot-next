//! Hermetic V4 cooldown and spam-guard acceptance against the public
//! `VoteKickCore` API. Two guards stack: the 120 s active-vote window (one per
//! guild/target) and the post-terminal cooldown plus per-initiator limit
//! (VK-02); see `docs/voice-vote-cooldown-acceptance.md` for the criterion
//! mapping.

use std::cell::Cell;

use two_bot_core::{
    RoomKickDecision, VoteBallot, VoteCancellation, VoteClock, VoteKickCore, VoteKickError,
    VoteKickRef, VoteKickStatus, VoteProgress, VoteRoomFacts, VOTE_KICK_COOLDOWN_MS,
    VOTE_KICK_INITIATOR_LIMIT, VOTE_KICK_INITIATOR_WINDOW_MS, VOTE_KICK_TTL_MS,
};

const GUILD: u64 = 1;
const ROOM_A: u64 = 10;
const ROOM_B: u64 = 11;
const TARGET: u64 = 9;
const START_MS: u64 = 1_000;
const DEADLINE_MS: u64 = START_MS + VOTE_KICK_TTL_MS;
/// End of the post-terminal cooldown for a vote that settles at `DEADLINE_MS`.
const COOLDOWN_END_MS: u64 = DEADLINE_MS + VOTE_KICK_COOLDOWN_MS;

/// Processing time advanced by hand; no sleep or wall clock.
struct TestClock(Cell<u64>);

impl TestClock {
    fn new() -> Self {
        Self(Cell::new(START_MS))
    }

    fn set(&self, now_ms: u64) {
        self.0.set(now_ms);
    }
}

impl VoteClock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0.get()
    }
}

/// Room A: owner 2, original creator 3; members 4 and 5; target 9.
fn room_a(occupants: &[u64]) -> VoteRoomFacts<'_> {
    VoteRoomFacts {
        guild_id: GUILD,
        room_id: ROOM_A,
        owner_id: 2,
        original_creator_id: 3,
        occupants,
        target_privileged: Some(false),
    }
}

/// Room B in the same guild: owner 20, original creator 21; member 22.
fn room_b(occupants: &[u64]) -> VoteRoomFacts<'_> {
    VoteRoomFacts {
        guild_id: GUILD,
        room_id: ROOM_B,
        owner_id: 20,
        original_creator_id: 21,
        occupants,
        target_privileged: Some(false),
    }
}

const A_OCCUPANTS: [u64; 5] = [2, 3, 4, 5, TARGET];

fn progress(yes: usize, required: usize, total: usize) -> VoteProgress {
    VoteProgress {
        yes,
        required,
        total,
    }
}

fn unknown(reference: VoteKickRef, id: u64) -> VoteKickRef {
    VoteKickRef { id, ..reference }
}

// ---- (1) a failed vote cannot be immediately re-raised ----

#[test]
fn defeated_vote_cannot_be_reraised_against_the_target_until_its_window_ends() {
    let facts = room_a(&A_OCCUPANTS);
    let clock = TestClock::new();
    let mut core = VoteKickCore::new();
    let vote = core.start(100, facts, 4, TARGET, &clock).unwrap().vote;
    // Every eligible occupant votes No: the vote cannot pass on this roster,
    // but V4 only settles it at expiry, so it keeps the target locked.
    for voter in [2, 3, 4, 5] {
        let update = core
            .cast(vote, facts, voter, VoteBallot::No, &clock)
            .unwrap();
        assert_eq!(update.status, VoteKickStatus::Active);
        assert_eq!(update.kick, None);
    }
    for now_ms in [START_MS, START_MS + 1, DEADLINE_MS - 1] {
        clock.set(now_ms);
        for (id, initiator) in [(101, 4), (102, 5), (103, 2), (104, 3)] {
            assert_eq!(
                core.start(id, facts, initiator, TARGET, &clock),
                Err(VoteKickError::ActiveVoteExists),
                "re-raise by {initiator} at {now_ms}"
            );
        }
        // The defeated vote's own interaction ID can never be replayed.
        assert_eq!(
            core.start(100, facts, 4, TARGET, &clock),
            Err(VoteKickError::ReusedVoteId)
        );
    }
    let still = core.refresh(vote, facts, &clock).unwrap();
    assert_eq!(still.status, VoteKickStatus::Active);
    assert_eq!(still.progress, progress(0, 3, 4));
}

// ---- (2) repeated attempts from the same initiator inside the window ----

#[test]
fn initiator_spam_inside_the_window_is_refused_and_reserves_nothing() {
    let facts = room_a(&A_OCCUPANTS);
    let clock = TestClock::new();
    let mut core = VoteKickCore::new();
    let vote = core.start(100, facts, 4, TARGET, &clock).unwrap().vote;
    for (offset, id) in (101..=110).enumerate() {
        clock.set(START_MS + offset as u64 * 1_000);
        assert_eq!(
            core.start(id, facts, 4, TARGET, &clock),
            Err(VoteKickError::ActiveVoteExists)
        );
        // A refused start creates no vote and reserves no button state.
        assert_eq!(
            core.refresh(unknown(vote, id), facts, &clock),
            Err(VoteKickError::UnknownVote)
        );
    }
    // Starting cast no ballot, so the initiator's first button counts once.
    let first = core.cast(vote, facts, 4, VoteBallot::Yes, &clock).unwrap();
    assert_eq!(first.progress, progress(1, 3, 4));
    for ballot in [VoteBallot::Yes, VoteBallot::No, VoteBallot::Yes] {
        assert_eq!(
            core.cast(vote, facts, 4, ballot, &clock),
            Err(VoteKickError::RepeatedVote)
        );
    }
    let after = core.refresh(vote, facts, &clock).unwrap();
    assert_eq!(after.status, VoteKickStatus::Active);
    assert_eq!(after.progress, progress(1, 3, 4));
    assert_eq!(after.kick, None);
}

#[test]
fn spam_cannot_be_laundered_through_another_target_or_room_boundary() {
    let facts = room_a(&A_OCCUPANTS);
    let clock = TestClock::new();
    let mut core = VoteKickCore::new();
    let vote = core.start(100, facts, 4, TARGET, &clock).unwrap().vote;
    core.cast(vote, facts, 4, VoteBallot::Yes, &clock).unwrap();
    // A forged button that names a different target, room or guild cannot be
    // used to record a second ballot for the same member.
    for forged in [
        VoteKickRef {
            target_id: 5,
            ..vote
        },
        VoteKickRef {
            room_id: ROOM_B,
            ..vote
        },
        VoteKickRef {
            guild_id: 7,
            ..vote
        },
    ] {
        assert_eq!(
            core.cast(forged, facts, 4, VoteBallot::Yes, &clock),
            Err(VoteKickError::WrongVoteBoundary)
        );
    }
    assert_eq!(
        core.refresh(vote, facts, &clock).unwrap().progress,
        progress(1, 3, 4)
    );
}

// ---- (3) expiry ends the window; the post-terminal cooldown gates re-entry ----

#[test]
fn window_expiry_re_enables_a_fresh_vote_only_after_the_cooldown() {
    let facts = room_a(&A_OCCUPANTS);
    let clock = TestClock::new();
    let mut core = VoteKickCore::new();
    let old = core.start(100, facts, 4, TARGET, &clock).unwrap().vote;
    core.cast(old, facts, 2, VoteBallot::Yes, &clock).unwrap();
    core.cast(old, facts, 4, VoteBallot::Yes, &clock).unwrap();

    clock.set(DEADLINE_MS - 1);
    assert_eq!(
        core.start(101, facts, 4, TARGET, &clock),
        Err(VoteKickError::ActiveVoteExists)
    );
    // At the deadline no refresh is needed first: starting settles the elapsed
    // vote as Expired without a passing decision, but the expiry starts the
    // post-terminal cooldown, so a fresh ID is still refused.
    clock.set(DEADLINE_MS);
    assert_eq!(
        core.start(101, facts, 4, TARGET, &clock),
        Err(VoteKickError::Cooldown)
    );
    // A refused cooldown start creates no vote and reserves no button state.
    assert_eq!(
        core.refresh(unknown(old, 101), facts, &clock),
        Err(VoteKickError::UnknownVote)
    );
    // Boundary: refused just before the cooldown ends, allowed exactly at it.
    clock.set(COOLDOWN_END_MS - 1);
    assert_eq!(
        core.start(101, facts, 4, TARGET, &clock),
        Err(VoteKickError::Cooldown)
    );
    clock.set(COOLDOWN_END_MS);
    let fresh = core.start(102, facts, 4, TARGET, &clock).unwrap();
    assert_eq!(fresh.status, VoteKickStatus::Active);
    assert_eq!(fresh.progress, progress(0, 3, 4));
    assert_eq!(fresh.kick, None);

    let old_button = core.cast(old, facts, 3, VoteBallot::Yes, &clock).unwrap();
    assert_eq!(old_button.status, VoteKickStatus::Expired);
    assert_eq!(old_button.kick, None);
    assert_eq!(
        core.start(100, facts, 4, TARGET, &clock),
        Err(VoteKickError::ReusedVoteId)
    );

    // The fresh vote inherits no ballots: earlier voters may vote again.
    for voter in [2, 4] {
        core.cast(fresh.vote, facts, voter, VoteBallot::Yes, &clock)
            .unwrap();
    }
    assert_eq!(
        core.refresh(fresh.vote, facts, &clock).unwrap().progress,
        progress(2, 3, 4)
    );

    // The fresh vote opens its own window, measured from its own start; past
    // its deadline the same cooldown applies again.
    let fresh_deadline = COOLDOWN_END_MS + VOTE_KICK_TTL_MS;
    let fresh_cooldown_end = fresh_deadline + VOTE_KICK_COOLDOWN_MS;
    clock.set(fresh_deadline - 1);
    assert_eq!(
        core.start(103, facts, 4, TARGET, &clock),
        Err(VoteKickError::ActiveVoteExists)
    );
    clock.set(fresh_deadline);
    assert_eq!(
        core.start(103, facts, 4, TARGET, &clock),
        Err(VoteKickError::Cooldown)
    );
    clock.set(fresh_cooldown_end);
    assert!(core.start(104, facts, 4, TARGET, &clock).is_ok());
    assert_eq!(
        core.refresh(fresh.vote, facts, &clock).unwrap().status,
        VoteKickStatus::Expired
    );
}

// ---- (4) scoping by target member and room ----

#[test]
fn votes_in_room_a_never_block_other_targets_in_room_a_or_room_b() {
    let a_facts = room_a(&A_OCCUPANTS);
    let b_occupants = [20, 21, 22, 8];
    let b_facts = room_b(&b_occupants);
    let clock = TestClock::new();
    let mut core = VoteKickCore::new();
    let a_vote = core.start(100, a_facts, 4, TARGET, &clock).unwrap().vote;
    // Another target in the same room, and a different target in room B.
    let a_other = core.start(101, a_facts, 4, 5, &clock).unwrap().vote;
    let b_vote = core.start(102, b_facts, 22, 8, &clock).unwrap().vote;
    // The same member IDs in another guild are a separate scope.
    let other_guild = VoteRoomFacts {
        guild_id: 7,
        ..a_facts
    };
    assert!(core.start(103, other_guild, 4, TARGET, &clock).is_ok());

    // Room B passes on its own electorate; its decision names room B only.
    core.cast(b_vote, b_facts, 20, VoteBallot::Yes, &clock)
        .unwrap();
    let passed = core
        .cast(b_vote, b_facts, 21, VoteBallot::Yes, &clock)
        .unwrap();
    assert_eq!(
        passed.kick,
        Some(RoomKickDecision {
            guild_id: GUILD,
            room_id: ROOM_B,
            target_id: 8,
        })
    );
    // Room B buttons and facts cannot reach room A's votes.
    assert_eq!(
        core.cast(a_vote, b_facts, 22, VoteBallot::Yes, &clock),
        Err(VoteKickError::WrongVoteBoundary)
    );
    for reference in [a_vote, a_other] {
        let update = core.refresh(reference, a_facts, &clock).unwrap();
        assert_eq!(update.status, VoteKickStatus::Active);
        assert_eq!(update.progress.yes, 0);
    }
}

#[test]
fn target_moving_to_room_b_is_released_by_room_a_departure_refresh() {
    let clock = TestClock::new();
    let mut core = VoteKickCore::new();
    let a_vote = core
        .start(100, room_a(&A_OCCUPANTS), 4, TARGET, &clock)
        .unwrap()
        .vote;
    // The target leaves room A for room B (a member is in one voice channel).
    let a_after = [2, 3, 4, 5];
    let b_occupants = [20, 21, 22, TARGET];
    // Unchanged guard: one active vote per target per guild until the parent
    // delivers the departure, so room B is refused before room A refreshes.
    assert_eq!(
        core.start(101, room_b(&b_occupants), 22, TARGET, &clock),
        Err(VoteKickError::ActiveVoteExists)
    );
    let cancelled = core.refresh(a_vote, room_a(&a_after), &clock).unwrap();
    assert_eq!(
        cancelled.status,
        VoteKickStatus::Cancelled(VoteCancellation::TargetLeft)
    );
    assert_eq!(cancelled.kick, None);
    // The departure releases the active-vote window at once, but the
    // cancellation starts the post-terminal cooldown keyed by guild + target,
    // so room B cannot be used to evade it: refused (not ActiveVoteExists)
    // inside the cooldown, allowed exactly at its end.
    assert_eq!(
        core.start(101, room_b(&b_occupants), 22, TARGET, &clock),
        Err(VoteKickError::Cooldown)
    );
    assert_eq!(
        core.refresh(unknown(a_vote, 101), room_b(&b_occupants), &clock),
        Err(VoteKickError::UnknownVote)
    );
    clock.set(START_MS + VOTE_KICK_COOLDOWN_MS - 1);
    assert_eq!(
        core.start(101, room_b(&b_occupants), 22, TARGET, &clock),
        Err(VoteKickError::Cooldown)
    );
    clock.set(START_MS + VOTE_KICK_COOLDOWN_MS);
    let b_vote = core
        .start(102, room_b(&b_occupants), 22, TARGET, &clock)
        .unwrap();
    assert_eq!(b_vote.vote.room_id, ROOM_B);
    assert_eq!(b_vote.progress, progress(0, 2, 3));
    // Room A's cancelled vote cannot be revived by the target returning.
    assert_eq!(
        core.refresh(a_vote, room_a(&A_OCCUPANTS), &clock)
            .unwrap()
            .status,
        VoteKickStatus::Cancelled(VoteCancellation::TargetLeft)
    );
}

// ---- (5) eligible-voter counting and threshold are unchanged ----

#[test]
fn strict_majority_threshold_for_each_electorate_size() {
    // (eligible total, required Yes) for V4's strict majority.
    let expected = [
        (1, 1),
        (2, 2),
        (3, 2),
        (4, 3),
        (5, 3),
        (6, 4),
        (7, 4),
        (8, 5),
    ];
    for (total, required) in expected {
        let mut occupants: Vec<u64> = (40..40 + total as u64).collect();
        occupants.push(TARGET);
        let facts = room_a(&occupants);
        let clock = TestClock::new();
        let mut core = VoteKickCore::new();
        let started = core.start(100, facts, 40, TARGET, &clock).unwrap();
        assert_eq!(started.progress, progress(0, required, total));
        assert_eq!(
            started.progress.required_total_text(),
            format!("{required}/{total}")
        );
        for (index, voter) in occupants.iter().take(required).enumerate() {
            let update = core
                .cast(started.vote, facts, *voter, VoteBallot::Yes, &clock)
                .unwrap();
            let passes = index + 1 == required;
            assert_eq!(update.progress.yes, index + 1);
            assert_eq!(update.status == VoteKickStatus::Passed, passes);
            assert_eq!(update.kick.is_some(), passes);
        }
    }
}

#[test]
fn electorate_excludes_target_dedupes_roster_and_counts_abstention_as_no() {
    let duplicated = [2, 2, 3, 4, 4, 5, TARGET, TARGET];
    let facts = room_a(&duplicated);
    let clock = TestClock::new();
    let mut core = VoteKickCore::new();
    let started = core.start(100, facts, 4, TARGET, &clock).unwrap();
    assert_eq!(started.progress, progress(0, 3, 4));
    assert_eq!(
        core.cast(started.vote, facts, TARGET, VoteBallot::No, &clock),
        Err(VoteKickError::IneligibleVoter)
    );
    core.cast(started.vote, facts, 2, VoteBallot::Yes, &clock)
        .unwrap();
    core.cast(started.vote, facts, 3, VoteBallot::No, &clock)
        .unwrap();
    // A refused re-raise leaves the count and the denominator untouched.
    assert_eq!(
        core.start(101, facts, 4, TARGET, &clock),
        Err(VoteKickError::ActiveVoteExists)
    );
    let tie = core
        .cast(started.vote, facts, 4, VoteBallot::Yes, &clock)
        .unwrap();
    assert_eq!(tie.status, VoteKickStatus::Active);
    assert_eq!(tie.progress, progress(2, 3, 4));
    clock.set(DEADLINE_MS);
    let expired = core.refresh(started.vote, facts, &clock).unwrap();
    assert_eq!(expired.status, VoteKickStatus::Expired);
    assert_eq!(expired.kick, None);
}

#[test]
fn fresh_vote_after_the_window_counts_the_current_roster() {
    let clock = TestClock::new();
    let mut core = VoteKickCore::new();
    let old = core
        .start(100, room_a(&A_OCCUPANTS), 4, TARGET, &clock)
        .unwrap();
    assert_eq!(old.progress, progress(0, 3, 4));
    // Past the window the expiry starts the cooldown; the fresh vote waits for
    // the cooldown, then counts the current roster.
    clock.set(COOLDOWN_END_MS);
    // Two members joined while the first vote ran; the new electorate is 6.
    let grown = [2, 3, 4, 5, 6, 7, TARGET];
    let fresh = core.start(101, room_a(&grown), 6, TARGET, &clock).unwrap();
    assert_eq!(fresh.progress, progress(0, 4, 6));
    assert_eq!(fresh.progress.required_total_text(), "4/6");
    for voter in [4, 5, 6] {
        let update = core
            .cast(fresh.vote, room_a(&grown), voter, VoteBallot::Yes, &clock)
            .unwrap();
        assert_eq!(update.status, VoteKickStatus::Active);
    }
    let passed = core
        .cast(fresh.vote, room_a(&grown), 7, VoteBallot::Yes, &clock)
        .unwrap();
    assert_eq!(passed.status, VoteKickStatus::Passed);
    assert_eq!(
        passed.kick,
        Some(RoomKickDecision {
            guild_id: GUILD,
            room_id: ROOM_A,
            target_id: TARGET,
        })
    );
}

// ---- (6) post-terminal cooldown after pass and cancellation ----

#[test]
fn cooldown_after_a_pass_blocks_the_same_target_but_not_other_targets() {
    assert_eq!(VOTE_KICK_COOLDOWN_MS, 300_000);
    let facts = room_a(&A_OCCUPANTS);
    let clock = TestClock::new();
    let mut core = VoteKickCore::new();
    let vote = core.start(100, facts, 4, TARGET, &clock).unwrap().vote;
    for voter in [2, 3, 4] {
        core.cast(vote, facts, voter, VoteBallot::Yes, &clock)
            .unwrap();
    }
    assert_eq!(
        core.refresh(vote, facts, &clock).unwrap().status,
        VoteKickStatus::Passed
    );
    // Fresh IDs against the same target are refused inside the cooldown and
    // create nothing; replaying a passed vote emits no new enforcement.
    for id in [101, 102] {
        assert_eq!(
            core.start(id, facts, 4, TARGET, &clock),
            Err(VoteKickError::Cooldown)
        );
        assert_eq!(
            core.refresh(unknown(vote, id), facts, &clock),
            Err(VoteKickError::UnknownVote)
        );
    }
    let replay = core.cast(vote, facts, 5, VoteBallot::Yes, &clock).unwrap();
    assert_eq!(replay.status, VoteKickStatus::Passed);
    assert_eq!(replay.kick, None);
    // The cooldown does not leak across targets: another member can be voted
    // on at once (same initiator still has cap room: 1 start of 3 used).
    let other = core.start(103, facts, 4, 5, &clock).unwrap();
    assert_eq!(other.status, VoteKickStatus::Active);
    // Boundary: refused just before the cooldown ends, allowed exactly at it.
    clock.set(START_MS + VOTE_KICK_COOLDOWN_MS - 1);
    assert_eq!(
        core.start(104, facts, 4, TARGET, &clock),
        Err(VoteKickError::Cooldown)
    );
    clock.set(START_MS + VOTE_KICK_COOLDOWN_MS);
    let fresh = core.start(104, facts, 4, TARGET, &clock).unwrap();
    assert_eq!(fresh.status, VoteKickStatus::Active);
    assert_eq!(fresh.progress, progress(0, 3, 4));
    assert_eq!(fresh.kick, None);
}

#[test]
fn cooldown_after_a_protective_cancellation_with_validation_precedence() {
    let facts = room_a(&A_OCCUPANTS);
    let clock = TestClock::new();
    let mut core = VoteKickCore::new();
    let vote = core.start(100, facts, 4, TARGET, &clock).unwrap().vote;
    // The target becomes the owner mid-vote: the pending vote cancels.
    let owned = VoteRoomFacts {
        owner_id: 9,
        ..facts
    };
    let cancelled = core.cast(vote, owned, 2, VoteBallot::Yes, &clock).unwrap();
    assert_eq!(
        cancelled.status,
        VoteKickStatus::Cancelled(VoteCancellation::TargetProtected)
    );
    assert_eq!(cancelled.kick, None);
    // Target validation still precedes the cooldown: a protected target gets
    // ProtectedTarget even inside the cooldown window.
    assert_eq!(
        core.start(101, owned, 4, TARGET, &clock),
        Err(VoteKickError::ProtectedTarget)
    );
    // With ordinary facts the cooldown refuses until exactly its end.
    assert_eq!(
        core.start(101, facts, 4, TARGET, &clock),
        Err(VoteKickError::Cooldown)
    );
    clock.set(START_MS + VOTE_KICK_COOLDOWN_MS - 1);
    assert_eq!(
        core.start(101, facts, 4, TARGET, &clock),
        Err(VoteKickError::Cooldown)
    );
    clock.set(START_MS + VOTE_KICK_COOLDOWN_MS);
    let fresh = core.start(101, facts, 4, TARGET, &clock).unwrap();
    assert_eq!(fresh.status, VoteKickStatus::Active);
    assert_eq!(fresh.kick, None);
}

// ---- (7) per-initiator limit across targets, rooms and guilds ----

/// Room B facts that still contain initiator 4, so a cross-room retry by the
/// same initiator reaches the initiator cap instead of InitiatorNotOccupant.
fn room_b_with_initiator(occupants: &[u64]) -> VoteRoomFacts<'_> {
    VoteRoomFacts {
        guild_id: GUILD,
        room_id: ROOM_B,
        owner_id: 20,
        original_creator_id: 21,
        occupants,
        target_privileged: Some(false),
    }
}

#[test]
fn initiator_cap_spans_targets_and_rooms_but_not_guilds() {
    assert_eq!(VOTE_KICK_INITIATOR_LIMIT, 3);
    assert_eq!(VOTE_KICK_INITIATOR_WINDOW_MS, 600_000);
    // Owner 2 and original creator 3 are protected; 5, 7, 8 and TARGET are
    // all votable, giving the initiator four distinct targets.
    let occupants = [2, 3, 4, 5, 7, 8, TARGET];
    let facts = room_a(&occupants);
    let clock = TestClock::new();
    let mut core = VoteKickCore::new();
    let first = core.start(100, facts, 4, TARGET, &clock).unwrap().vote;
    for (id, target) in [(101, 5), (102, 8)] {
        let started = core.start(id, facts, 4, target, &clock).unwrap();
        assert_eq!(started.status, VoteKickStatus::Active);
    }
    // A fourth target cannot evade the cap, neither in this room nor in room B.
    assert_eq!(
        core.start(103, facts, 4, 7, &clock),
        Err(VoteKickError::InitiatorLimited)
    );
    assert_eq!(
        core.start(104, room_b_with_initiator(&[4, 20, 21, 30]), 4, 30, &clock),
        Err(VoteKickError::InitiatorLimited)
    );
    // Refused cap attempts create no vote and reserve no button state.
    assert_eq!(
        core.refresh(unknown(first, 103), facts, &clock),
        Err(VoteKickError::UnknownVote)
    );
    // The cap is per guild: the same initiator can still start in another guild.
    let other_guild = VoteRoomFacts {
        guild_id: 7,
        ..facts
    };
    assert!(core.start(105, other_guild, 4, TARGET, &clock).is_ok());
    // Boundary: the window slides, so the oldest start drops off exactly at
    // START + WINDOW and the fourth target is allowed again.
    clock.set(START_MS + VOTE_KICK_INITIATOR_WINDOW_MS - 1);
    assert_eq!(
        core.start(106, facts, 4, 7, &clock),
        Err(VoteKickError::InitiatorLimited)
    );
    clock.set(START_MS + VOTE_KICK_INITIATOR_WINDOW_MS);
    let fresh = core.start(106, facts, 4, 7, &clock).unwrap();
    assert_eq!(fresh.status, VoteKickStatus::Active);
    assert_eq!(fresh.kick, None);
}

#[test]
fn target_cooldown_precedes_the_initiator_cap() {
    // Initiator 4 reaches the cap while target 9 sits in a post-pass cooldown:
    // a retry against 9 reports Cooldown (the target refusal wins), while a
    // retry against a fresh target reports InitiatorLimited.
    let occupants = [2, 3, 4, 5, 7, 8, TARGET];
    let facts = room_a(&occupants);
    let clock = TestClock::new();
    let mut core = VoteKickCore::new();
    let vote = core.start(100, facts, 4, TARGET, &clock).unwrap().vote;
    core.start(101, facts, 4, 5, &clock).unwrap();
    core.start(102, facts, 4, 8, &clock).unwrap();
    for voter in [2, 3, 4, 5] {
        core.cast(vote, facts, voter, VoteBallot::Yes, &clock)
            .unwrap();
    }
    assert_eq!(
        core.refresh(vote, facts, &clock).unwrap().status,
        VoteKickStatus::Passed
    );
    assert_eq!(
        core.start(103, facts, 4, TARGET, &clock),
        Err(VoteKickError::Cooldown)
    );
    assert_eq!(
        core.start(104, facts, 4, 7, &clock),
        Err(VoteKickError::InitiatorLimited)
    );
}
