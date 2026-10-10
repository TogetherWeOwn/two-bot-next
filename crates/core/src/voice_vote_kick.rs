//! Pure V4 decisions, written from `docs/voice-rooms.md` §V4 only.
//!
//! The caller supplies authoritative room facts and a clock, serializes events,
//! and refreshes on member/ownership changes and expiry. No Discord action or
//! persistence lives here. A passing transition emits a room-scoped decision
//! once; replay returns the terminal status without emitting it again.

use std::collections::{BTreeMap, BTreeSet};

use crate::Snowflake;

pub const VOTE_KICK_TTL_MS: u64 = 120_000;

/// Post-terminal cooldown per guild + target: after a vote passes, expires or
/// is cancelled, a fresh vote against the same member in the same guild is
/// refused until this long after the terminal transition. Room changes do not
/// evade it. Pinned by `docs/voice-rooms.md` §V4 (VK-02).
pub const VOTE_KICK_COOLDOWN_MS: u64 = 300_000;

/// Per-initiator limit: at most this many successful starts per guild +
/// initiator inside [`VOTE_KICK_INITIATOR_WINDOW_MS`], across targets and
/// rooms. Pinned by `docs/voice-rooms.md` §V4 (VK-02).
pub const VOTE_KICK_INITIATOR_LIMIT: usize = 3;

/// Sliding window for [`VOTE_KICK_INITIATOR_LIMIT`].
pub const VOTE_KICK_INITIATOR_WINDOW_MS: u64 = 600_000;

/// Processing time, not a timestamp supplied by a button payload.
pub trait VoteClock {
    fn now_ms(&self) -> u64;
}

/// Current facts for one managed room. Occupant IDs are deduplicated here;
/// neither roles nor administrator status grant an outsider a vote.
///
/// `target_privileged` is the target's effective Kick Members or Administrator
/// state in `guild_id`, resolved by the parent in the interaction's guild:
/// `Some(true)` blocks the vote, `Some(false)` allows it, and `None` means the
/// guild authority lookup was unavailable and the vote must fail closed.
#[derive(Debug, Clone, Copy)]
pub struct VoteRoomFacts<'a> {
    pub guild_id: Snowflake,
    pub room_id: Snowflake,
    pub owner_id: Snowflake,
    pub original_creator_id: Snowflake,
    pub occupants: &'a [Snowflake],
    pub target_privileged: Option<bool>,
}

impl VoteRoomFacts<'_> {
    fn protected(&self, member_id: Snowflake) -> bool {
        member_id == self.owner_id || member_id == self.original_creator_id
    }

    fn privileged(&self) -> bool {
        self.target_privileged == Some(true)
    }

    fn authority_unknown(&self) -> bool {
        self.target_privileged.is_none()
    }

    /// Owner, original creator, or a Kick Members / Administrator holder. The
    /// parent re-supplies current facts on every transition, so a promotion
    /// granted mid-vote is observed before enforcement.
    fn target_protected(&self, target_id: Snowflake) -> bool {
        self.protected(target_id) || self.privileged()
    }

    fn eligible(&self, target_id: Snowflake) -> BTreeSet<Snowflake> {
        self.occupants
            .iter()
            .copied()
            .filter(|id| *id != target_id)
            .collect()
    }
}

/// Bind each button to the original vote, guild, room and target. `id` must be
/// the unique initiating interaction ID, never just the target member ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoteKickRef {
    pub id: Snowflake,
    pub guild_id: Snowflake,
    pub room_id: Snowflake,
    pub target_id: Snowflake,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoteBallot {
    Yes,
    No,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoteCancellation {
    TargetLeft,
    TargetProtected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoteKickStatus {
    Active,
    Passed,
    Expired,
    Cancelled(VoteCancellation),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoteProgress {
    pub yes: usize,
    pub required: usize,
    pub total: usize,
}

impl VoteProgress {
    /// V4's required/total progress display (not Yes/ballots cast).
    #[must_use]
    pub fn required_total_text(self) -> String {
        format!("{}/{}", self.required, self.total)
    }
}

/// A decision only: disconnect and deny Connect on this room, not a guild kick
/// or ban. The parent must perform effective-permission checks and delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoomKickDecision {
    pub guild_id: Snowflake,
    pub room_id: Snowflake,
    pub target_id: Snowflake,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoteKickUpdate {
    pub vote: VoteKickRef,
    pub status: VoteKickStatus,
    pub progress: VoteProgress,
    /// Present only on the first Active -> Passed transition. Reading a Passed
    /// status is not permission to repeat a permission-bearing action.
    pub kick: Option<RoomKickDecision>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum VoteKickError {
    #[error("only an occupant can start a vote")]
    InitiatorNotOccupant,
    #[error("the target must be in the room")]
    TargetNotOccupant,
    #[error("members cannot target themselves")]
    SelfTarget,
    #[error("the owner and original creator cannot be targeted")]
    ProtectedTarget,
    #[error("the target has moderation privilege and cannot be voted out")]
    PrivilegedTarget,
    #[error("target authority is unavailable")]
    AuthorityUnavailable,
    #[error("a vote is already active for this target in this guild")]
    ActiveVoteExists,
    #[error("a recent vote against this member is in cooldown")]
    Cooldown,
    #[error("the initiator has started too many votes recently")]
    InitiatorLimited,
    #[error("the initiating interaction ID has already been used")]
    ReusedVoteId,
    #[error("unknown vote")]
    UnknownVote,
    #[error("vote, guild, room or target does not match")]
    WrongVoteBoundary,
    #[error("only current occupants other than the target can vote")]
    IneligibleVoter,
    #[error("this member has already voted")]
    RepeatedVote,
    #[error("clock is before the vote start or the deadline would overflow")]
    InvalidTime,
}

#[derive(Debug)]
struct Vote {
    reference: VoteKickRef,
    started_at_ms: u64,
    expires_at_ms: u64,
    ballots: BTreeMap<Snowflake, VoteBallot>,
    status: VoteKickStatus,
    progress: VoteProgress,
    /// Processing time of the Active -> terminal transition. `None` while
    /// active; drives the post-terminal cooldown keyed by guild + target.
    terminal_at_ms: Option<u64>,
}

impl Vote {
    fn update(&self, kick: Option<RoomKickDecision>) -> VoteKickUpdate {
        VoteKickUpdate {
            vote: self.reference,
            status: self.status,
            progress: self.progress,
            kick,
        }
    }

    fn advance(&mut self, facts: VoteRoomFacts<'_>, now_ms: u64) -> Option<RoomKickDecision> {
        if self.status != VoteKickStatus::Active {
            return None;
        }
        let eligible = facts.eligible(self.reference.target_id);
        self.progress = VoteProgress {
            yes: eligible
                .iter()
                .filter(|id| self.ballots.get(id) == Some(&VoteBallot::Yes))
                .count(),
            required: eligible.len() / 2 + 1,
            total: eligible.len(),
        };
        if !facts.occupants.contains(&self.reference.target_id) {
            self.status = VoteKickStatus::Cancelled(VoteCancellation::TargetLeft);
            self.terminal_at_ms = Some(now_ms);
        } else if facts.authority_unknown() || facts.target_protected(self.reference.target_id) {
            // Fail closed on an unavailable guild-authority lookup, and cancel
            // when the target gained owner/creator status or Kick Members /
            // Administrator after the vote started. No kick is emitted.
            // Either way this is a terminal transition and starts the
            // post-terminal cooldown (VK-02).
            self.status = VoteKickStatus::Cancelled(VoteCancellation::TargetProtected);
            self.terminal_at_ms = Some(now_ms);
        } else if now_ms >= self.expires_at_ms {
            self.status = VoteKickStatus::Expired;
            // Expiry happened at the deadline even if observed late: backdate
            // so a late refresh or lazy sweep does not extend the cooldown.
            self.terminal_at_ms = Some(self.expires_at_ms);
        } else if self.progress.yes >= self.progress.required {
            self.status = VoteKickStatus::Passed;
            self.terminal_at_ms = Some(now_ms);
            return Some(RoomKickDecision {
                guild_id: self.reference.guild_id,
                room_id: self.reference.room_id,
                target_id: self.reference.target_id,
            });
        }
        None
    }
}

/// In-memory decision state, including finished IDs to reject command replay.
/// Retain this core for the managed session; dropping it loses the replay ledger.
/// Durable storage, restart reconciliation and retention belong to the parent.
///
/// The post-terminal cooldown is derived from terminal votes in `votes`; the
/// per-initiator history keeps successful start times per guild + initiator and
/// is pruned to [`VOTE_KICK_INITIATOR_WINDOW_MS`] on every start. Coordinated
/// bounding of all retained state is VK-03 and out of scope here.
#[derive(Debug, Default)]
pub struct VoteKickCore {
    votes: BTreeMap<Snowflake, Vote>,
    initiator_starts: BTreeMap<(Snowflake, Snowflake), Vec<u64>>,
}

impl VoteKickCore {
    /// A start timestamp still counts toward the initiator limit at `now_ms`.
    fn initiator_start_counts(started_at_ms: u64, now_ms: u64) -> bool {
        now_ms < started_at_ms.saturating_add(VOTE_KICK_INITIATOR_WINDOW_MS)
    }

    /// A terminal vote still holds the guild + target cooldown at `now_ms`.
    fn cooldown_holds(terminal_at_ms: u64, now_ms: u64) -> bool {
        now_ms < terminal_at_ms.saturating_add(VOTE_KICK_COOLDOWN_MS)
    }
}

impl VoteKickCore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Starting does not cast a ballot: V4 says votes are cast with buttons.
    pub fn start(
        &mut self,
        id: Snowflake,
        facts: VoteRoomFacts<'_>,
        initiator_id: Snowflake,
        target_id: Snowflake,
        clock: &impl VoteClock,
    ) -> Result<VoteKickUpdate, VoteKickError> {
        if self.votes.contains_key(&id) {
            return Err(VoteKickError::ReusedVoteId);
        }
        if !facts.occupants.contains(&initiator_id) {
            return Err(VoteKickError::InitiatorNotOccupant);
        }
        if initiator_id == target_id {
            return Err(VoteKickError::SelfTarget);
        }
        if !facts.occupants.contains(&target_id) {
            return Err(VoteKickError::TargetNotOccupant);
        }
        // Fail closed in the interaction's guild: without authoritative
        // privilege evidence no vote, ballot, or enforcement effect follows.
        if facts.authority_unknown() {
            return Err(VoteKickError::AuthorityUnavailable);
        }
        if facts.protected(target_id) {
            return Err(VoteKickError::ProtectedTarget);
        }
        if facts.privileged() {
            return Err(VoteKickError::PrivilegedTarget);
        }
        let now_ms = clock.now_ms();
        let expires_at_ms = now_ms
            .checked_add(VOTE_KICK_TTL_MS)
            .ok_or(VoteKickError::InvalidTime)?;
        // A new command must not be blocked by an elapsed vote when the timer
        // has not refreshed it yet. No passing decision is made on this path,
        // but the lazy expiry is a terminal transition and starts the cooldown.
        for vote in self.votes.values_mut() {
            if vote.status == VoteKickStatus::Active && now_ms >= vote.expires_at_ms {
                vote.status = VoteKickStatus::Expired;
                vote.terminal_at_ms = Some(vote.expires_at_ms);
            }
        }
        if self.votes.values().any(|vote| {
            vote.status == VoteKickStatus::Active
                && vote.reference.guild_id == facts.guild_id
                && vote.reference.target_id == target_id
        }) {
            return Err(VoteKickError::ActiveVoteExists);
        }
        // Refusal order after the active guard: target cooldown, then the
        // initiator cap. Both refuse without creating a vote, ballot,
        // enforcement effect or initiator-history entry.
        let in_cooldown = self.votes.values().any(|vote| {
            vote.status != VoteKickStatus::Active
                && vote.reference.guild_id == facts.guild_id
                && vote.reference.target_id == target_id
                && vote
                    .terminal_at_ms
                    .is_some_and(|terminal| Self::cooldown_holds(terminal, now_ms))
        });
        if in_cooldown {
            return Err(VoteKickError::Cooldown);
        }
        let key = (facts.guild_id, initiator_id);
        let starts = self.initiator_starts.entry(key).or_default();
        starts.retain(|started| Self::initiator_start_counts(*started, now_ms));
        if starts.len() >= VOTE_KICK_INITIATOR_LIMIT {
            return Err(VoteKickError::InitiatorLimited);
        }
        let total = facts.eligible(target_id).len();
        let vote = Vote {
            reference: VoteKickRef {
                id,
                guild_id: facts.guild_id,
                room_id: facts.room_id,
                target_id,
            },
            started_at_ms: now_ms,
            expires_at_ms,
            ballots: BTreeMap::new(),
            status: VoteKickStatus::Active,
            progress: VoteProgress {
                yes: 0,
                required: total / 2 + 1,
                total,
            },
            terminal_at_ms: None,
        };
        let update = vote.update(None);
        self.votes.insert(id, vote);
        self.initiator_starts.entry(key).or_default().push(now_ms);
        Ok(update)
    }

    fn vote_mut(
        &mut self,
        reference: VoteKickRef,
        facts: VoteRoomFacts<'_>,
        now_ms: u64,
    ) -> Result<&mut Vote, VoteKickError> {
        let vote = self
            .votes
            .get_mut(&reference.id)
            .ok_or(VoteKickError::UnknownVote)?;
        if vote.reference != reference
            || reference.guild_id != facts.guild_id
            || reference.room_id != facts.room_id
        {
            return Err(VoteKickError::WrongVoteBoundary);
        }
        if now_ms < vote.started_at_ms {
            return Err(VoteKickError::InvalidTime);
        }
        Ok(vote)
    }

    pub fn cast(
        &mut self,
        reference: VoteKickRef,
        facts: VoteRoomFacts<'_>,
        voter_id: Snowflake,
        ballot: VoteBallot,
        clock: &impl VoteClock,
    ) -> Result<VoteKickUpdate, VoteKickError> {
        let now_ms = clock.now_ms();
        let vote = self.vote_mut(reference, facts, now_ms)?;
        if vote.status != VoteKickStatus::Active {
            return Ok(vote.update(None));
        }
        if voter_id == reference.target_id || !facts.occupants.contains(&voter_id) {
            return Err(VoteKickError::IneligibleVoter);
        }
        // Expiry/departure/protection precede a new ballot. Roster changes may
        // also settle an existing majority without counting this ballot.
        let kick = vote.advance(facts, now_ms);
        if vote.status != VoteKickStatus::Active {
            return Ok(vote.update(kick));
        }
        if vote.ballots.contains_key(&voter_id) {
            return Err(VoteKickError::RepeatedVote);
        }
        vote.ballots.insert(voter_id, ballot);
        let kick = vote.advance(facts, now_ms);
        Ok(vote.update(kick))
    }

    /// Call on a timer and every roster/ownership change, especially a target
    /// departure. Once cancelled, a target rejoining cannot revive the vote.
    pub fn refresh(
        &mut self,
        reference: VoteKickRef,
        facts: VoteRoomFacts<'_>,
        clock: &impl VoteClock,
    ) -> Result<VoteKickUpdate, VoteKickError> {
        let now_ms = clock.now_ms();
        let vote = self.vote_mut(reference, facts, now_ms)?;
        let kick = vote.advance(facts, now_ms);
        Ok(vote.update(kick))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    struct Clock(Cell<u64>);

    impl Clock {
        fn new() -> Self {
            Self(Cell::new(1_000))
        }

        fn set(&self, now_ms: u64) {
            self.0.set(now_ms);
        }
    }

    impl VoteClock for Clock {
        fn now_ms(&self) -> u64 {
            self.0.get()
        }
    }

    fn room(occupants: &[Snowflake]) -> VoteRoomFacts<'_> {
        VoteRoomFacts {
            guild_id: 1,
            room_id: 10,
            owner_id: 2,
            original_creator_id: 3,
            occupants,
            target_privileged: Some(false),
        }
    }

    fn privileged_room(occupants: &[Snowflake]) -> VoteRoomFacts<'_> {
        VoteRoomFacts {
            target_privileged: Some(true),
            ..room(occupants)
        }
    }

    fn unknown_authority_room(occupants: &[Snowflake]) -> VoteRoomFacts<'_> {
        VoteRoomFacts {
            target_privileged: None,
            ..room(occupants)
        }
    }

    fn start(core: &mut VoteKickCore, facts: VoteRoomFacts<'_>, clock: &Clock) -> VoteKickRef {
        core.start(100, facts, 2, 9, clock).unwrap().vote
    }

    #[test]
    fn any_occupant_can_start_but_no_implicit_yes_vote() {
        let facts = room(&[2, 3, 4, 9]);
        let clock = Clock::new();
        let mut core = VoteKickCore::new();
        let update = core.start(100, facts, 4, 9, &clock).unwrap();
        assert_eq!(update.status, VoteKickStatus::Active);
        assert_eq!(
            update.progress,
            VoteProgress {
                yes: 0,
                required: 2,
                total: 3
            }
        );
        assert_eq!(update.progress.required_total_text(), "2/3");
        assert_eq!(update.kick, None);
    }

    #[test]
    fn initiation_authorization_and_target_boundaries() {
        let facts = room(&[2, 3, 4, 9]);
        let clock = Clock::new();
        let cases = [
            (99, 9, VoteKickError::InitiatorNotOccupant),
            (4, 99, VoteKickError::TargetNotOccupant),
            (4, 4, VoteKickError::SelfTarget),
            (4, 2, VoteKickError::ProtectedTarget),
            (4, 3, VoteKickError::ProtectedTarget),
        ];
        for (actor, target, error) in cases {
            let mut core = VoteKickCore::new();
            assert_eq!(core.start(100, facts, actor, target, &clock), Err(error));
            // Refusal has not reserved the ID or target.
            assert!(core.start(100, facts, 4, 9, &clock).is_ok());
        }
        // Each privileged class is denied separately at start with no
        // vote, ballot, or enforcement effect.
        {
            let privileged = privileged_room(&[2, 3, 4, 9]);
            let mut core = VoteKickCore::new();
            assert_eq!(
                core.start(100, privileged, 4, 9, &clock),
                Err(VoteKickError::PrivilegedTarget)
            );
            assert!(core.start(100, facts, 4, 9, &clock).is_ok());
        }
        // An unavailable guild-authority lookup fails closed.
        {
            let unknown = unknown_authority_room(&[2, 3, 4, 9]);
            let mut core = VoteKickCore::new();
            assert_eq!(
                core.start(100, unknown, 4, 9, &clock),
                Err(VoteKickError::AuthorityUnavailable)
            );
            assert!(core.start(100, facts, 4, 9, &clock).is_ok());
        }
    }

    #[test]
    fn one_active_vote_per_target_with_guild_isolation() {
        let facts = room(&[2, 3, 4, 8, 9]);
        let clock = Clock::new();
        let mut core = VoteKickCore::new();
        start(&mut core, facts, &clock);
        assert_eq!(
            core.start(101, facts, 4, 9, &clock),
            Err(VoteKickError::ActiveVoteExists)
        );
        let other_room = VoteRoomFacts {
            room_id: 11,
            ..facts
        };
        assert_eq!(
            core.start(101, other_room, 4, 9, &clock),
            Err(VoteKickError::ActiveVoteExists)
        );
        assert!(core.start(102, facts, 4, 8, &clock).is_ok());
        let other_guild = VoteRoomFacts {
            guild_id: 5,
            ..facts
        };
        assert!(core.start(103, other_guild, 4, 9, &clock).is_ok());
    }

    #[test]
    fn strict_majority_for_odd_and_even_electorates() {
        let clock = Clock::new();
        for total in 1..=8 {
            let mut occupants: Vec<_> = (2..2 + total).collect();
            occupants.push(99);
            let facts = room(&occupants);
            let mut core = VoteKickCore::new();
            let update = core.start(100, facts, 2, 99, &clock).unwrap();
            assert_eq!(update.progress.total, total as usize);
            assert_eq!(update.progress.required, total as usize / 2 + 1);
            for (index, voter_id) in occupants.iter().take(update.progress.required).enumerate() {
                let cast = core
                    .cast(update.vote, facts, *voter_id, VoteBallot::Yes, &clock)
                    .unwrap();
                let passes = index + 1 == update.progress.required;
                assert_eq!(cast.status == VoteKickStatus::Passed, passes);
                assert_eq!(cast.kick.is_some(), passes);
            }
        }
    }

    #[test]
    fn deduplicated_roster_ineligible_voters_and_repeated_ballots() {
        let facts = room(&[2, 2, 3, 3, 4, 9, 9]);
        let clock = Clock::new();
        let mut core = VoteKickCore::new();
        let reference = start(&mut core, facts, &clock);
        assert_eq!(
            core.refresh(reference, facts, &clock)
                .unwrap()
                .progress
                .total,
            3
        );
        for outsider in [9, 99] {
            assert_eq!(
                core.cast(reference, facts, outsider, VoteBallot::Yes, &clock),
                Err(VoteKickError::IneligibleVoter)
            );
        }
        let update = core
            .cast(reference, facts, 2, VoteBallot::Yes, &clock)
            .unwrap();
        assert_eq!(update.progress.yes, 1);
        for ballot in [VoteBallot::Yes, VoteBallot::No] {
            assert_eq!(
                core.cast(reference, facts, 2, ballot, &clock),
                Err(VoteKickError::RepeatedVote)
            );
        }
        core.cast(reference, facts, 3, VoteBallot::No, &clock)
            .unwrap();
        assert_eq!(
            core.cast(reference, facts, 3, VoteBallot::Yes, &clock),
            Err(VoteKickError::RepeatedVote)
        );
        assert_eq!(
            core.refresh(reference, facts, &clock).unwrap().progress.yes,
            1
        );
    }

    #[test]
    fn abstentions_and_no_votes_never_reduce_the_denominator() {
        let facts = room(&[2, 3, 4, 5, 9]);
        let clock = Clock::new();
        let mut core = VoteKickCore::new();
        let reference = start(&mut core, facts, &clock);
        core.cast(reference, facts, 2, VoteBallot::Yes, &clock)
            .unwrap();
        core.cast(reference, facts, 3, VoteBallot::No, &clock)
            .unwrap();
        let tie = core
            .cast(reference, facts, 4, VoteBallot::Yes, &clock)
            .unwrap();
        assert_eq!(tie.status, VoteKickStatus::Active);
        assert_eq!(
            tie.progress,
            VoteProgress {
                yes: 2,
                required: 3,
                total: 4
            }
        );
        clock.set(121_000);
        let expired = core.refresh(reference, facts, &clock).unwrap();
        assert_eq!(expired.status, VoteKickStatus::Expired);
        assert_eq!(expired.kick, None);
    }

    #[test]
    fn buttons_and_facts_cannot_cross_vote_room_guild_or_target() {
        let facts = room(&[2, 3, 4, 9]);
        let clock = Clock::new();
        let mut core = VoteKickCore::new();
        let reference = start(&mut core, facts, &clock);
        for forged in [
            VoteKickRef {
                guild_id: 8,
                ..reference
            },
            VoteKickRef {
                room_id: 11,
                ..reference
            },
            VoteKickRef {
                target_id: 4,
                ..reference
            },
        ] {
            assert_eq!(
                core.cast(forged, facts, 2, VoteBallot::Yes, &clock),
                Err(VoteKickError::WrongVoteBoundary)
            );
        }
        for wrong_facts in [
            VoteRoomFacts {
                guild_id: 8,
                ..facts
            },
            VoteRoomFacts {
                room_id: 11,
                ..facts
            },
        ] {
            assert_eq!(
                core.refresh(reference, wrong_facts, &clock),
                Err(VoteKickError::WrongVoteBoundary)
            );
            assert_eq!(
                core.cast(reference, wrong_facts, 2, VoteBallot::Yes, &clock),
                Err(VoteKickError::WrongVoteBoundary)
            );
        }
        let unknown = VoteKickRef {
            id: 101,
            ..reference
        };
        assert_eq!(
            core.cast(unknown, facts, 2, VoteBallot::Yes, &clock),
            Err(VoteKickError::UnknownVote)
        );
        assert_eq!(
            core.refresh(reference, facts, &clock).unwrap().progress.yes,
            0
        );
    }

    #[test]
    fn expiry_boundary_precedes_a_winning_ballot() {
        let facts = room(&[2, 3, 4, 9]);
        for (now_ms, expected) in [
            (120_999, VoteKickStatus::Passed),
            (121_000, VoteKickStatus::Expired),
            (121_001, VoteKickStatus::Expired),
        ] {
            let clock = Clock::new();
            let mut core = VoteKickCore::new();
            let reference = start(&mut core, facts, &clock);
            core.cast(reference, facts, 2, VoteBallot::Yes, &clock)
                .unwrap();
            clock.set(now_ms);
            let update = core
                .cast(reference, facts, 3, VoteBallot::Yes, &clock)
                .unwrap();
            assert_eq!(update.status, expected);
            assert_eq!(update.kick.is_some(), expected == VoteKickStatus::Passed);
        }
    }

    #[test]
    fn target_departure_cancels_and_rejoin_cannot_revive_it() {
        let facts = room(&[2, 3, 4, 9]);
        let clock = Clock::new();
        let mut core = VoteKickCore::new();
        let reference = start(&mut core, facts, &clock);
        core.cast(reference, facts, 2, VoteBallot::Yes, &clock)
            .unwrap();
        let left = room(&[2, 3, 4]);
        let update = core.refresh(reference, left, &clock).unwrap();
        assert_eq!(
            update.status,
            VoteKickStatus::Cancelled(VoteCancellation::TargetLeft)
        );
        assert_eq!(update.kick, None);
        assert_eq!(
            core.cast(reference, facts, 3, VoteBallot::Yes, &clock)
                .unwrap(),
            update
        );
        // The cancellation starts the post-terminal cooldown: a fresh ID is
        // refused inside it and creates nothing, then succeeds at its end.
        assert_eq!(
            core.start(101, facts, 2, 9, &clock),
            Err(VoteKickError::Cooldown)
        );
        assert_eq!(
            core.refresh(reference, facts, &clock).unwrap().status,
            VoteKickStatus::Cancelled(VoteCancellation::TargetLeft)
        );
        clock.set(1_000 + VOTE_KICK_COOLDOWN_MS);
        assert!(core.start(101, facts, 2, 9, &clock).is_ok());
        assert_eq!(
            core.start(100, facts, 2, 9, &clock),
            Err(VoteKickError::ReusedVoteId)
        );
    }

    #[test]
    fn new_owner_or_original_creator_is_protected_before_a_pass() {
        let facts = room(&[2, 3, 4, 9]);
        for new_facts in [
            VoteRoomFacts {
                owner_id: 9,
                ..facts
            },
            VoteRoomFacts {
                original_creator_id: 9,
                ..facts
            },
        ] {
            let clock = Clock::new();
            let mut core = VoteKickCore::new();
            let reference = start(&mut core, facts, &clock);
            core.cast(reference, facts, 2, VoteBallot::Yes, &clock)
                .unwrap();
            let update = core
                .cast(reference, new_facts, 3, VoteBallot::Yes, &clock)
                .unwrap();
            assert_eq!(
                update.status,
                VoteKickStatus::Cancelled(VoteCancellation::TargetProtected)
            );
            assert_eq!(update.kick, None);
        }
    }

    #[test]
    fn privileged_promotion_mid_vote_cancels_before_enforcement() {
        // VK-01 recheck: a promotion granted mid-vote must not be bypassed. A
        // passed vote for a promoted target produces no kick effect.
        let facts = room(&[2, 3, 4, 9]);
        let promoted = privileged_room(&[2, 3, 4, 9]);
        let clock = Clock::new();
        let mut core = VoteKickCore::new();
        let reference = start(&mut core, facts, &clock);
        core.cast(reference, facts, 2, VoteBallot::Yes, &clock)
            .unwrap();
        // The second Yes would pass on the old roster; the recheck sees the
        // privilege first and cancels instead.
        let update = core
            .cast(reference, promoted, 3, VoteBallot::Yes, &clock)
            .unwrap();
        assert_eq!(
            update.status,
            VoteKickStatus::Cancelled(VoteCancellation::TargetProtected)
        );
        assert_eq!(update.kick, None);
        // A refresh on the promoted facts stays cancelled and emits nothing.
        let again = core.refresh(reference, promoted, &clock).unwrap();
        assert_eq!(
            again.status,
            VoteKickStatus::Cancelled(VoteCancellation::TargetProtected)
        );
        assert_eq!(again.kick, None);
    }

    #[test]
    fn unavailable_lookup_mid_vote_cancels_without_enforcement() {
        // Failing closed mid-vote: losing guild-authority evidence cancels the
        // vote rather than letting a later ballot pass it.
        let facts = room(&[2, 3, 4, 9]);
        let unknown = unknown_authority_room(&[2, 3, 4, 9]);
        let clock = Clock::new();
        let mut core = VoteKickCore::new();
        let reference = start(&mut core, facts, &clock);
        core.cast(reference, facts, 2, VoteBallot::Yes, &clock)
            .unwrap();
        let update = core
            .cast(reference, unknown, 3, VoteBallot::Yes, &clock)
            .unwrap();
        assert_eq!(
            update.status,
            VoteKickStatus::Cancelled(VoteCancellation::TargetProtected)
        );
        assert_eq!(update.kick, None);
    }

    #[test]
    fn protected_boundary_owner_admin_and_ordinary_member() {
        // Owner and original creator stay ProtectedTarget; a privileged
        // (Kick Members / Administrator) target is PrivilegedTarget; an
        // ordinary occupant with no privilege remains votable end to end.
        let facts = room(&[2, 3, 4, 9]);
        let clock = Clock::new();
        for (target, error) in [
            (2, VoteKickError::ProtectedTarget),
            (3, VoteKickError::ProtectedTarget),
        ] {
            let mut core = VoteKickCore::new();
            assert_eq!(core.start(100, facts, 4, target, &clock), Err(error));
        }
        let mut core = VoteKickCore::new();
        assert_eq!(
            core.start(100, privileged_room(&[2, 3, 4, 9]), 4, 9, &clock),
            Err(VoteKickError::PrivilegedTarget)
        );
        let mut core = VoteKickCore::new();
        assert_eq!(
            core.start(100, unknown_authority_room(&[2, 3, 4, 9]), 4, 9, &clock),
            Err(VoteKickError::AuthorityUnavailable)
        );
        // Ordinary target: start, two Yes ballots, pass with a decision.
        let mut core = VoteKickCore::new();
        let reference = start(&mut core, facts, &clock);
        core.cast(reference, facts, 2, VoteBallot::Yes, &clock)
            .unwrap();
        let passed = core
            .cast(reference, facts, 3, VoteBallot::Yes, &clock)
            .unwrap();
        assert_eq!(passed.status, VoteKickStatus::Passed);
        assert!(passed.kick.is_some());
    }

    #[test]
    fn current_roster_controls_eligibility_and_majority() {
        let facts = room(&[2, 3, 4, 5, 9]);
        let clock = Clock::new();
        let mut core = VoteKickCore::new();
        let reference = start(&mut core, facts, &clock);
        core.cast(reference, facts, 2, VoteBallot::Yes, &clock)
            .unwrap();
        core.cast(reference, facts, 3, VoteBallot::Yes, &clock)
            .unwrap();
        let changed = room(&[2, 4, 5, 6, 9]);
        let update = core.refresh(reference, changed, &clock).unwrap();
        assert_eq!(
            update.progress,
            VoteProgress {
                yes: 1,
                required: 3,
                total: 4
            }
        );
        assert_eq!(
            core.cast(reference, changed, 3, VoteBallot::Yes, &clock),
            Err(VoteKickError::IneligibleVoter)
        );
        core.cast(reference, changed, 6, VoteBallot::Yes, &clock)
            .unwrap();
        let reduced = room(&[2, 4, 6, 9]);
        assert_eq!(
            core.refresh(reference, reduced, &clock).unwrap().status,
            VoteKickStatus::Passed
        );
    }

    #[test]
    fn passed_decision_is_room_scoped_and_never_emitted_on_replay() {
        let facts = room(&[2, 3, 4, 9]);
        let clock = Clock::new();
        let mut core = VoteKickCore::new();
        let reference = start(&mut core, facts, &clock);
        core.cast(reference, facts, 2, VoteBallot::Yes, &clock)
            .unwrap();
        let passed = core
            .cast(reference, facts, 3, VoteBallot::Yes, &clock)
            .unwrap();
        assert_eq!(
            passed.kick,
            Some(RoomKickDecision {
                guild_id: 1,
                room_id: 10,
                target_id: 9
            })
        );
        for _ in 0..3 {
            let replay = core
                .cast(reference, facts, 3, VoteBallot::Yes, &clock)
                .unwrap();
            assert_eq!(replay.status, VoteKickStatus::Passed);
            assert_eq!(replay.kick, None);
            assert_eq!(core.refresh(reference, facts, &clock).unwrap().kick, None);
            assert_eq!(
                core.start(100, facts, 2, 9, &clock),
                Err(VoteKickError::ReusedVoteId)
            );
        }
        // The pass starts the post-terminal cooldown: a fresh vote is refused
        // inside it, then succeeds exactly at its end with no inherited ballots.
        assert_eq!(
            core.start(101, facts, 2, 9, &clock),
            Err(VoteKickError::Cooldown)
        );
        clock.set(1_000 + VOTE_KICK_COOLDOWN_MS - 1);
        assert_eq!(
            core.start(101, facts, 2, 9, &clock),
            Err(VoteKickError::Cooldown)
        );
        clock.set(1_000 + VOTE_KICK_COOLDOWN_MS);
        let new_vote = core.start(101, facts, 2, 9, &clock).unwrap();
        assert_eq!(new_vote.progress.yes, 0);
        core.cast(reference, facts, 4, VoteBallot::Yes, &clock)
            .unwrap();
        assert_eq!(
            core.refresh(new_vote.vote, facts, &clock)
                .unwrap()
                .progress
                .yes,
            0
        );
    }

    #[test]
    fn elapsed_vote_does_not_block_new_vote_and_old_buttons_stay_expired() {
        let facts = room(&[2, 3, 4, 9]);
        let clock = Clock::new();
        let mut core = VoteKickCore::new();
        let old = start(&mut core, facts, &clock);
        // At the active-window deadline the elapsed vote has expired, but its
        // post-terminal cooldown still refuses a fresh vote.
        clock.set(121_000);
        assert_eq!(
            core.start(101, facts, 2, 9, &clock),
            Err(VoteKickError::Cooldown)
        );
        clock.set(121_000 + VOTE_KICK_COOLDOWN_MS);
        let new_vote = core.start(101, facts, 2, 9, &clock).unwrap();
        let old_button = core.cast(old, facts, 2, VoteBallot::Yes, &clock).unwrap();
        assert_eq!(old_button.status, VoteKickStatus::Expired);
        assert_eq!(old_button.kick, None);
        assert_eq!(
            core.refresh(new_vote.vote, facts, &clock)
                .unwrap()
                .progress
                .yes,
            0
        );
    }

    #[test]
    fn invalid_clock_cannot_extend_or_wrap_deadline() {
        let facts = room(&[2, 3, 4, 9]);
        let clock = Clock::new();
        let mut core = VoteKickCore::new();
        let reference = start(&mut core, facts, &clock);
        clock.set(999);
        assert_eq!(
            core.cast(reference, facts, 2, VoteBallot::Yes, &clock),
            Err(VoteKickError::InvalidTime)
        );
        assert_eq!(
            core.refresh(reference, facts, &clock),
            Err(VoteKickError::InvalidTime)
        );
        clock.set(u64::MAX);
        assert_eq!(
            core.start(101, facts, 2, 4, &clock),
            Err(VoteKickError::InvalidTime)
        );
        clock.set(121_000);
        assert_eq!(
            core.refresh(reference, facts, &clock).unwrap().status,
            VoteKickStatus::Expired
        );
    }
}
