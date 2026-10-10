//! V4 vote-kick audit vocabulary: the bounded events and outcomes the voice
//! runtime appends to `voice_vote_kick_audit`, and the row it appends.
//!
//! A vote-kick is a room-scoped decision: the target loses Connect on one
//! temporary room and is disconnected from voice. It is never a guild-member
//! kick, so no event or outcome here may borrow that word (or "ban"); the
//! moderation `/kick` has its own audit row (`moderation_audit`). Rows carry
//! snowflakes and fixed codes only: no interaction token, no message text, no
//! free-form reason.
//!
//! Pure data: the worker builds rows, the store appends them. A row is keyed
//! by (guild, vote, event), where `vote_id` is the initiating interaction ID
//! the vote core already uses to bind ballots, so replaying a row is a no-op.

use crate::voice_vote_kick::{VoteCancellation, VoteKickError, VoteKickStatus, VoteProgress};
use crate::Snowflake;

/// What the row records. At most one row per (vote, event).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KickAuditEvent {
    /// A vote began; the outcome is always `started`.
    VoteStarted,
    /// A `/kick` start reached the voice runtime and was refused.
    VoteRefused,
    /// The vote finished: passed, expired or cancelled.
    VoteResult,
    /// The room-scoped enforcement of a passed vote reached a terminal state.
    Enforcement,
}

impl KickAuditEvent {
    pub const ALL: [Self; 4] = [
        Self::VoteStarted,
        Self::VoteRefused,
        Self::VoteResult,
        Self::Enforcement,
    ];

    /// The stored code; the table's `CHECK` lists exactly these.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::VoteStarted => "vote_started",
            Self::VoteRefused => "vote_refused",
            Self::VoteResult => "vote_result",
            Self::Enforcement => "enforcement",
        }
    }
}

/// Outcome of a `vote_started` row.
pub const OUTCOME_STARTED: &str = "started";
/// Outcome of a `vote_refused` row when live voice evidence was not
/// authoritative (disconnected or not yet published).
pub const OUTCOME_EVIDENCE_UNAVAILABLE: &str = "evidence_unavailable";
/// Outcome of a `vote_refused` row when the channel is not a tracked room.
pub const OUTCOME_NOT_A_ROOM: &str = "not_a_room";

/// Outcome code for a refused start. Exhaustive on purpose: a new vote error
/// must pick a code here before it compiles.
#[must_use]
pub const fn refusal_outcome(error: VoteKickError) -> &'static str {
    match error {
        VoteKickError::InitiatorNotOccupant => "initiator_not_occupant",
        VoteKickError::TargetNotOccupant => "target_not_occupant",
        VoteKickError::SelfTarget => "self_target",
        VoteKickError::ProtectedTarget => "protected_target",
        VoteKickError::ActiveVoteExists => "active_vote_exists",
        VoteKickError::Cooldown => "cooldown",
        VoteKickError::InitiatorLimited => "initiator_limited",
        VoteKickError::ReusedVoteId => "reused_vote_id",
        VoteKickError::UnknownVote => "unknown_vote",
        VoteKickError::WrongVoteBoundary => "wrong_vote_boundary",
        VoteKickError::IneligibleVoter => "ineligible_voter",
        VoteKickError::RepeatedVote => "repeated_vote",
        VoteKickError::InvalidTime => "invalid_time",
    }
}

/// Outcome code for a finished vote; `None` while the vote is still active.
#[must_use]
pub const fn result_outcome(status: VoteKickStatus) -> Option<&'static str> {
    match status {
        VoteKickStatus::Active => None,
        VoteKickStatus::Passed => Some("passed"),
        VoteKickStatus::Expired => Some("expired"),
        VoteKickStatus::Cancelled(VoteCancellation::TargetLeft) => Some("cancelled_target_left"),
        VoteKickStatus::Cancelled(VoteCancellation::TargetProtected) => {
            Some("cancelled_target_protected")
        }
    }
}

/// Terminal outcome of the room-scoped enforcement of a passed vote. The
/// names say what Discord was asked to do (deny Connect on the room, then
/// disconnect from voice), never "kick".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnforcementOutcome {
    /// Connect was denied on the room and the member was disconnected.
    ConnectDeniedAndDisconnected,
    /// Connect was denied; the member had already left, so nothing to disconnect.
    ConnectDeniedTargetAbsent,
    /// The room was gone (or its guard cancelled the write): nothing written.
    SkippedRoomGone,
    /// Ownership changed after the vote passed and the target is now the owner
    /// or original creator: nothing written.
    SkippedTargetProtected,
    /// The bot lacks the permissions the writes need (checked, or refused by
    /// Discord).
    PermissionMissing,
    /// Discord rejected a write for another terminal reason.
    DiscordError,
    /// A retryable failure exhausted the queue's attempt budget.
    GaveUp,
}

impl EnforcementOutcome {
    pub const ALL: [Self; 7] = [
        Self::ConnectDeniedAndDisconnected,
        Self::ConnectDeniedTargetAbsent,
        Self::SkippedRoomGone,
        Self::SkippedTargetProtected,
        Self::PermissionMissing,
        Self::DiscordError,
        Self::GaveUp,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConnectDeniedAndDisconnected => "connect_denied_and_disconnected",
            Self::ConnectDeniedTargetAbsent => "connect_denied_target_absent",
            Self::SkippedRoomGone => "skipped_room_gone",
            Self::SkippedTargetProtected => "skipped_target_protected",
            Self::PermissionMissing => "permission_missing",
            Self::DiscordError => "discord_error",
            Self::GaveUp => "gave_up",
        }
    }
}

/// One audit row. `vote_id` is the initiating interaction ID (a snowflake;
/// never the interaction token). `progress` rides `vote_started` and
/// `vote_result` rows only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KickAuditRow {
    pub guild_id: Snowflake,
    pub room_id: Snowflake,
    pub vote_id: Snowflake,
    pub initiator_id: Snowflake,
    pub target_id: Snowflake,
    pub event: KickAuditEvent,
    pub outcome: &'static str,
    pub progress: Option<VoteProgress>,
    /// ISO-8601 milliseconds, the worker's wall clock when the event happened.
    pub occurred_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    const REFUSALS: [VoteKickError; 13] = [
        VoteKickError::InitiatorNotOccupant,
        VoteKickError::TargetNotOccupant,
        VoteKickError::SelfTarget,
        VoteKickError::ProtectedTarget,
        VoteKickError::ActiveVoteExists,
        VoteKickError::Cooldown,
        VoteKickError::InitiatorLimited,
        VoteKickError::ReusedVoteId,
        VoteKickError::UnknownVote,
        VoteKickError::WrongVoteBoundary,
        VoteKickError::IneligibleVoter,
        VoteKickError::RepeatedVote,
        VoteKickError::InvalidTime,
    ];

    const STATUSES: [VoteKickStatus; 4] = [
        VoteKickStatus::Passed,
        VoteKickStatus::Expired,
        VoteKickStatus::Cancelled(VoteCancellation::TargetLeft),
        VoteKickStatus::Cancelled(VoteCancellation::TargetProtected),
    ];

    fn every_code() -> Vec<&'static str> {
        let mut codes = vec![
            OUTCOME_STARTED,
            OUTCOME_EVIDENCE_UNAVAILABLE,
            OUTCOME_NOT_A_ROOM,
        ];
        codes.extend(KickAuditEvent::ALL.map(KickAuditEvent::as_str));
        codes.extend(REFUSALS.map(refusal_outcome));
        codes.extend(STATUSES.iter().filter_map(|s| result_outcome(*s)));
        codes.extend(EnforcementOutcome::ALL.map(EnforcementOutcome::as_str));
        codes
    }

    #[test]
    fn codes_match_the_table_check_and_never_name_a_guild_kick_or_ban() {
        for code in every_code() {
            assert!(
                !code.is_empty()
                    && code.len() <= 40
                    && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{code} violates the outcome CHECK shape ^[a-z_]{{1,40}}$"
            );
            for banned in ["kick", "ban", "token"] {
                assert!(!code.contains(banned), "{code} names {banned}");
            }
        }
    }

    #[test]
    fn refusal_result_and_enforcement_codes_are_distinct_per_family() {
        let refusals: std::collections::BTreeSet<_> =
            REFUSALS.iter().map(|e| refusal_outcome(*e)).collect();
        assert_eq!(refusals.len(), REFUSALS.len());
        let results: std::collections::BTreeSet<_> =
            STATUSES.iter().filter_map(|s| result_outcome(*s)).collect();
        assert_eq!(results.len(), STATUSES.len());
        let enforcement: std::collections::BTreeSet<_> =
            EnforcementOutcome::ALL.iter().map(|o| o.as_str()).collect();
        assert_eq!(enforcement.len(), EnforcementOutcome::ALL.len());
        // The two start-time refusals the worker adds do not collide with a
        // vote-core refusal.
        assert!(!refusals.contains(OUTCOME_EVIDENCE_UNAVAILABLE));
        assert!(!refusals.contains(OUTCOME_NOT_A_ROOM));
    }

    #[test]
    fn an_active_vote_has_no_result_code() {
        assert_eq!(result_outcome(VoteKickStatus::Active), None);
    }
}
