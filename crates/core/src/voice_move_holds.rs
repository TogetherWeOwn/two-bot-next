//! Inert, process-local voice admission holds; this module performs no effects.
//!
//! Integration must serialize reservation, transition fencing, completion and
//! observation with the same observer mutex. Capture a transition generation
//! before publishing the cache change; arm completion under that mutex after
//! the local PATCH finishes. The caller must also serialize each guild's move
//! attempt from fact gathering through completion, so a late completion cannot
//! act on a replacement hold. Observe both previous and current channels. A
//! cancelled attempt remains pending. This ledger neither proves cache
//! freshness nor provides receiver or executor wiring.

use std::collections::HashMap;

use crate::Snowflake;

/// Bounded pending and successful/uncertain voice move admissions.
///
/// Holds do not expire and reseeding a cache does not clear them. Restarting
/// the process discards this in-memory state, so restart recovery is a separate
/// integration concern.
#[derive(Debug, Default)]
pub struct VoiceMoveHolds {
    targets: HashMap<Snowflake, Hold>,
    generation: u64,
}

#[derive(Debug)]
struct Hold {
    target: Snowflake,
    // None stays pending until local completion, even if an observer arrives.
    completion_generation: Option<u64>,
}

impl VoiceMoveHolds {
    /// Maximum concurrent holds; reaching it refuses new reservations.
    pub const LIMIT: usize = 1024;

    /// Reserve a target seat, or fail closed on duplicate, limit or exhaustion.
    pub fn reserve(&mut self, member: Snowflake, target: Snowflake) -> Result<(), HoldRefused> {
        if self.targets.len() >= Self::LIMIT
            || self.targets.contains_key(&member)
            || self.generation == u64::MAX
        {
            return Err(HoldRefused);
        }
        self.targets.insert(
            member,
            Hold {
                target,
                completion_generation: None,
            },
        );
        Ok(())
    }

    /// Arm a successful or uncertain completion at the current generation.
    /// Repeated completion does not move the original fence.
    pub fn complete(&mut self, member: Snowflake) {
        if let Some(hold) = self.targets.get_mut(&member) {
            hold.completion_generation.get_or_insert(self.generation);
        }
    }

    /// Release only when the caller has proved the operation had no effect.
    pub fn release_no_effect(&mut self, member: Snowflake) {
        self.targets.remove(&member);
    }

    /// Fence a transition before its cache publication, under the observer mutex.
    pub fn begin_transition(&mut self) -> u64 {
        self.generation = self.generation.saturating_add(1);
        self.generation
    }

    /// Settle only the same member and target on a strictly newer observation.
    /// `generation` must be the token captured for this actual transition.
    pub fn observe(&mut self, member: Snowflake, channel: Option<Snowflake>, generation: u64) {
        if self.targets.get(&member).is_some_and(|hold| {
            channel == Some(hold.target)
                && hold
                    .completion_generation
                    .is_some_and(|floor| floor < generation)
        }) {
            self.targets.remove(&member);
        }
    }

    /// Add every held target seat, even when the cache already counts it.
    /// The held member's own admission fails closed. The caller must first
    /// establish that the supplied cache occupancy is complete and trustworthy.
    pub fn occupancy(
        &self,
        member: Snowflake,
        target: Snowflake,
        cached: u32,
    ) -> Result<u32, HoldRefused> {
        if self.targets.contains_key(&member) {
            return Err(HoldRefused);
        }
        Ok(self.targets.values().fold(cached, |count, hold| {
            if hold.target == target {
                count.saturating_add(1)
            } else {
                count
            }
        }))
    }
}

/// Admission cannot safely proceed using the current ledger state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoldRefused;

#[cfg(test)]
mod tests;
