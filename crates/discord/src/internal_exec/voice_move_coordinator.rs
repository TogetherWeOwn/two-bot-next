//! Ordered gateway evidence around a single governed voice PATCH.
use std::sync::{Arc, Mutex};

use tokio::time::Instant;
use twilight_model::id::Id;
use two_bot_core::{
    internal_actions::VoiceMoveOutcome as Outcome, voice_move_decision::VoiceMoveDecision,
    voice_move_holds::VoiceMoveHolds, Snowflake,
};

use super::{ActionExecutor, VoiceMovePatch};
use crate::{pipeline::SharedCache, DiscordError};

/// Read evidence from the same cache and hold ledger that observe gateway transitions.
#[derive(Clone, Copy)]
pub struct VoiceMoveRoster<'a> {
    cache: &'a SharedCache,
    holds: &'a Mutex<VoiceMoveHolds>,
}

struct VoiceMoveProof {
    from: Snowflake,
    limit: u32,
    epoch: Arc<()>,
}

/// Terminal decision and scalar evidence. `DiscordUnavailable` may mean the
/// provider acted but a successful receipt could not be confirmed.
#[derive(Debug, Clone, Copy)]
pub struct VoiceMoveAttempt {
    pub outcome: Outcome,
    pub from: Option<Snowflake>,
    pub retry_after_secs: Option<u64>,
}

impl VoiceMoveAttempt {
    fn new(outcome: Outcome, from: Option<Snowflake>) -> Self {
        Self {
            outcome,
            from,
            retry_after_secs: None,
        }
    }
}

impl From<DiscordError> for VoiceMoveAttempt {
    fn from(error: DiscordError) -> Self {
        Self::new(
            match error {
                DiscordError::RateLimited => Outcome::RateLimited,
                _ => Outcome::DiscordUnavailable,
            },
            None,
        )
    }
}

impl<'a> VoiceMoveRoster<'a> {
    #[must_use]
    pub const fn new(cache: &'a SharedCache, holds: &'a Mutex<VoiceMoveHolds>) -> Self {
        Self { cache, holds }
    }

    fn policy_epoch(self, guild: Snowflake) -> Option<Arc<()>> {
        self.cache
            .with_guild_policy_snapshot(guild, |_, epoch| Arc::clone(epoch))
    }

    fn read<T>(
        self,
        [guild, member, target]: [Snowflake; 3],
        expected_epoch: &Arc<()>,
        use_roster: impl FnOnce(Option<Snowflake>, u32, &mut VoiceMoveHolds) -> Option<T>,
    ) -> Option<T> {
        let mut holds = self.holds.lock().ok()?;
        self.cache
            .with_guild_policy_snapshot(guild, |cache, epoch| {
                Arc::ptr_eq(epoch, expected_epoch).then_some(())?;
                let guild = Id::new(guild);
                (cache.guild(guild)?.unavailable() != Some(true)).then_some(())?;
                // The rebuilt roster, not a possibly retained voice state, owns membership.
                let (mut source, mut count) = (None, 0u32);
                let roster = cache.guild_voice_states(guild);
                for user in roster.as_deref().into_iter().flatten() {
                    let room = cache.voice_state(*user, guild)?.channel_id().get();
                    if user.get() == member {
                        source = Some(room);
                    }
                    count = count.checked_add(u32::from(room == target))?;
                }
                use_roster(source, count, &mut holds)
            })?
    }
}

impl ActionExecutor {
    /// Recheck local evidence after durable admission, then issue at most one PATCH.
    /// The caller supplies a readiness guard held across each cache/hold read, or
    /// `None` if gateway evidence is unavailable. The observer must share `roster`'s
    /// hold ledger and fence transitions before their cache publication.
    /// `deadline` bounds wire operations; await the result to let transport settle.
    pub async fn move_with_voice_evidence_until<R>(
        &self,
        ids: [&str; 3],
        roster: VoiceMoveRoster<'_>,
        deadline: Instant,
        ready: impl Fn() -> Option<R> + Sync,
    ) -> VoiceMoveAttempt {
        use VoiceMovePatch::{Denied, Limited, Moved, NotSent, Stale};
        let no_evidence = VoiceMoveAttempt::new(Outcome::DiscordUnavailable, None);
        let parse = |id: &str| {
            id.parse::<Snowflake>()
                .ok()
                .filter(|n| *n != 0 && n.to_string() == id)
        };
        let [Some(guild), Some(member), Some(target)] = ids.map(parse) else {
            return no_evidence;
        };
        let ids_numeric = [guild, member, target];
        let VoiceMoveProof { from, limit, epoch } = match self
            .check_move(roster, ids_numeric, deadline, &ready, true)
            .await
        {
            Ok(proof) => proof,
            Err(refusal) => return refusal,
        };
        let revalidate = async {
            let Some(_ready) = ready() else { return false };
            roster.read(ids_numeric, &epoch, |source, cached, holds| {
                let seats = holds.reserved_occupancy(member, target, cached)?;
                Some(source == Some(from) && (limit == 0 || seats <= limit))
            }) == Some(true)
        };
        let [guild, user, room] = ids;
        let patch = self
            .move_voice_member_until(guild, user, room, deadline, revalidate)
            .await;
        {
            let Ok(mut holds) = roster.holds.lock() else {
                return no_evidence;
            };
            if matches!(patch, Ok(Denied | Stale | NotSent(_) | Limited { .. })) {
                holds.release_no_effect(member);
            } else {
                holds.complete(member);
            }
        }
        let from = Some(from);
        match patch {
            Ok(Moved) => VoiceMoveAttempt::new(Outcome::Moved, from),
            Ok(Denied) => VoiceMoveAttempt::new(Outcome::BotNotPermitted, from),
            Ok(Limited { retry_after_ms, .. }) => VoiceMoveAttempt {
                retry_after_secs: retry_after_ms.map(|ms| ms.div_ceil(1_000).max(1)),
                ..VoiceMoveAttempt::new(Outcome::RateLimited, from)
            },
            // Re-read once after a stale refusal, but never issue a second PATCH.
            Ok(Stale) => match self
                .check_move(roster, ids_numeric, deadline, &ready, false)
                .await
            {
                Err(refusal) => VoiceMoveAttempt {
                    from: refusal.from.or(from),
                    ..refusal
                },
                Ok(_) => VoiceMoveAttempt::new(Outcome::DiscordUnavailable, from),
            },
            _ => VoiceMoveAttempt::new(Outcome::DiscordUnavailable, from),
        }
    }

    async fn check_move<R>(
        &self,
        roster: VoiceMoveRoster<'_>,
        ids @ [guild, member, target]: [Snowflake; 3],
        deadline: Instant,
        ready: &impl Fn() -> Option<R>,
        reserve: bool,
    ) -> Result<VoiceMoveProof, VoiceMoveAttempt> {
        let no_evidence = VoiceMoveAttempt::new(Outcome::DiscordUnavailable, None);
        // Capture before the first REST read, not after collecting mixed facts.
        let epoch = {
            let _ready = ready().ok_or(no_evidence)?;
            roster.policy_epoch(guild).ok_or(no_evidence)?
        };
        let room = self
            .voice_move_target_until(guild, target, deadline)
            .await?;
        let source = {
            let _ready = ready().ok_or(no_evidence)?;
            roster
                .read(ids, &epoch, |source, _, _| Some(source))
                .ok_or(no_evidence)?
        };
        let room = room.ok_or(VoiceMoveAttempt::new(Outcome::RoomGone, None))?;
        let from = source.ok_or(VoiceMoveAttempt::new(Outcome::NotConnected, None))?;
        if from == target {
            return Err(VoiceMoveAttempt::new(Outcome::AlreadyInRoom, Some(from)));
        }
        let limit = room.user_limit();
        let evidence = self
            .voice_move_evidence_until(room, member, from, deadline)
            .await?;
        let _ready = ready().ok_or(no_evidence)?;
        roster
            .read(ids, &epoch, |latest, cached, holds| {
                (latest == Some(from)).then_some(())?;
                let count = holds.occupancy(member, target, cached).ok()?;
                match evidence.decide(count) {
                    VoiceMoveDecision::Refuse(outcome) => {
                        Some(Err(VoiceMoveAttempt::new(outcome, Some(from))))
                    }
                    VoiceMoveDecision::Proceed { .. } => {
                        if reserve {
                            holds.reserve(member, target).ok()?;
                        }
                        Some(Ok(VoiceMoveProof {
                            from,
                            limit,
                            epoch: Arc::clone(&epoch),
                        }))
                    }
                    VoiceMoveDecision::Incomplete => None,
                }
            })
            .ok_or(no_evidence)?
    }
}
