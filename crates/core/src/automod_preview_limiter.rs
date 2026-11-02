//! Bounded in-memory rate limiter for `/automod-preview` (spec §4.3).
//!
//! Per (`actor`, `guild`) rolling budgets — 1 per 5 s, 30 per hour, 120 per
//! day — plus a 300-per-day guild budget shared by the guild's actors. The
//! map borrows only the capacity/refuse-when-full discipline of the existing
//! actor cooldowns: expired entries are pruned and reusable, and a full map
//! refuses new actors rather than evicting live counters (fail closed).
//!
//! Time is an explicit `now_ms` (wall-clock millis) so tests pin boundaries
//! on a paused clock. Charges are idempotent by interaction id for the
//! limiter's in-memory lifetime: a redelivered interaction replays without
//! charging again. Refusals never charge.

use std::collections::{HashMap, VecDeque};

/// Minimum gap between two admitted previews of one actor in one guild.
pub const PREVIEW_MIN_INTERVAL_MS: u64 = 5_000;
/// Rolling-hour budget per (actor, guild).
pub const PREVIEW_PER_HOUR: usize = 30;
/// Rolling-day budget per (actor, guild).
pub const PREVIEW_PER_DAY: usize = 120;
/// Rolling-day budget shared by every actor in one guild.
pub const PREVIEW_GUILD_PER_DAY: usize = 300;
/// Default actor-bucket capacity (mirrors the member-runtime cooldown map).
pub const PREVIEW_LIMITER_CAPACITY: usize = 1024;

const HOUR_MS: u64 = 3_600_000;
const DAY_MS: u64 = 86_400_000;

/// Outcome of a limiter check. `ReplayDuplicate` is a redelivery of an
/// already-admitted interaction: replay without charging or evaluating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewRateDecision {
    Admit,
    RateLimited,
    ReplayDuplicate,
}

#[derive(Debug, Default)]
struct ActorBucket {
    last_admit_ms: Option<u64>,
    hour: VecDeque<u64>,
    day: VecDeque<u64>,
    seen: HashMap<u64, u64>,
}

impl ActorBucket {
    fn prune(&mut self, now_ms: u64) {
        while self
            .hour
            .front()
            .is_some_and(|t| now_ms.saturating_sub(*t) >= HOUR_MS)
        {
            self.hour.pop_front();
        }
        while self
            .day
            .front()
            .is_some_and(|t| now_ms.saturating_sub(*t) >= DAY_MS)
        {
            self.day.pop_front();
        }
        self.seen.retain(|_, t| now_ms.saturating_sub(*t) < DAY_MS);
        if self.hour.is_empty() && self.day.is_empty() && self.seen.is_empty() {
            self.last_admit_ms = None;
        }
    }

    fn is_live(&self) -> bool {
        self.last_admit_ms.is_some() || !self.hour.is_empty() || !self.seen.is_empty()
    }
}

/// Bounded per-(actor, guild) preview budgets plus the shared guild-day cap.
#[derive(Debug)]
pub struct PreviewRateLimiter {
    buckets: HashMap<(String, String), ActorBucket>,
    capacity: usize,
}

impl PreviewRateLimiter {
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(PREVIEW_LIMITER_CAPACITY)
    }

    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buckets: HashMap::new(),
            capacity,
        }
    }

    fn prune_all(&mut self, now_ms: u64) {
        for bucket in self.buckets.values_mut() {
            bucket.prune(now_ms);
        }
        self.buckets.retain(|_, bucket| bucket.is_live());
    }

    fn guild_day_count(&self, guild: &str) -> usize {
        self.buckets
            .iter()
            .filter(|((_, g), _)| g == guild)
            .map(|(_, bucket)| bucket.day.len())
            .sum()
    }

    /// Check the budgets and, on admit, charge all of them exactly once.
    /// Strict-decode refusals never reach this call, so every `Admit` here
    /// is an evaluated preview (including `no_match` and `exempt`); every
    /// `RateLimited` evaluated nothing and charged nothing.
    pub fn check_and_admit(
        &mut self,
        actor: &str,
        guild: &str,
        interaction: u64,
        now_ms: u64,
    ) -> PreviewRateDecision {
        self.prune_all(now_ms);
        let key = (actor.to_owned(), guild.to_owned());
        if let Some(bucket) = self.buckets.get(&key) {
            if bucket.seen.contains_key(&interaction) {
                return PreviewRateDecision::ReplayDuplicate;
            }
            if bucket
                .last_admit_ms
                .is_some_and(|last| now_ms.saturating_sub(last) < PREVIEW_MIN_INTERVAL_MS)
            {
                return PreviewRateDecision::RateLimited;
            }
            if bucket.hour.len() >= PREVIEW_PER_HOUR || bucket.day.len() >= PREVIEW_PER_DAY {
                return PreviewRateDecision::RateLimited;
            }
            if self.guild_day_count(guild) >= PREVIEW_GUILD_PER_DAY {
                return PreviewRateDecision::RateLimited;
            }
        } else {
            if self.buckets.len() >= self.capacity {
                return PreviewRateDecision::RateLimited;
            }
            if self.guild_day_count(guild) >= PREVIEW_GUILD_PER_DAY {
                return PreviewRateDecision::RateLimited;
            }
        }
        let bucket = self.buckets.entry(key).or_default();
        bucket.last_admit_ms = Some(now_ms);
        bucket.hour.push_back(now_ms);
        bucket.day.push_back(now_ms);
        bucket.seen.insert(interaction, now_ms);
        PreviewRateDecision::Admit
    }
}

impl Default for PreviewRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACTOR: &str = "actor-1";
    const GUILD: &str = "guild-1";

    #[test]
    fn five_second_boundary() {
        let mut limiter = PreviewRateLimiter::new();
        assert_eq!(
            limiter.check_and_admit(ACTOR, GUILD, 1, 0),
            PreviewRateDecision::Admit
        );
        assert_eq!(
            limiter.check_and_admit(ACTOR, GUILD, 2, PREVIEW_MIN_INTERVAL_MS - 1),
            PreviewRateDecision::RateLimited
        );
        // A refusal charges nothing: admission opens exactly 5 s after the
        // admitted attempt, not after the refusal.
        assert_eq!(
            limiter.check_and_admit(ACTOR, GUILD, 3, PREVIEW_MIN_INTERVAL_MS),
            PreviewRateDecision::Admit
        );
    }

    #[test]
    fn hourly_cap_binds_and_releases() {
        let mut limiter = PreviewRateLimiter::new();
        let step = PREVIEW_MIN_INTERVAL_MS + 1;
        for i in 0..PREVIEW_PER_HOUR {
            let at = i as u64 * step;
            assert_eq!(
                limiter.check_and_admit(ACTOR, GUILD, 100 + i as u64, at),
                PreviewRateDecision::Admit,
                "hour budget {i}"
            );
        }
        let full_at = PREVIEW_PER_HOUR as u64 * step;
        assert_eq!(
            limiter.check_and_admit(ACTOR, GUILD, 999, full_at),
            PreviewRateDecision::RateLimited
        );
        assert_eq!(
            limiter.check_and_admit(ACTOR, GUILD, 1000, HOUR_MS + 1),
            PreviewRateDecision::Admit
        );
    }

    #[test]
    fn daily_cap_binds_before_hourly_pace() {
        let mut limiter = PreviewRateLimiter::new();
        // Four hourly budgets spaced past the rolling-hour window reach
        // exactly the daily cap.
        let stride = HOUR_MS + 200_000;
        for chunk in 0..4 {
            for i in 0..PREVIEW_PER_HOUR {
                let at = chunk * stride + i as u64 * (PREVIEW_MIN_INTERVAL_MS + 1) + 1;
                assert_eq!(
                    limiter.check_and_admit(ACTOR, GUILD, chunk * 100 + i as u64, at),
                    PreviewRateDecision::Admit,
                    "chunk {chunk} item {i}"
                );
            }
        }
        assert_eq!(
            limiter.check_and_admit(ACTOR, GUILD, 9999, 4 * stride),
            PreviewRateDecision::RateLimited
        );
        assert_eq!(
            limiter.check_and_admit(ACTOR, GUILD, 10000, DAY_MS + 1),
            PreviewRateDecision::Admit
        );
    }

    #[test]
    fn guild_cap_is_shared_and_new_actor_capped() {
        let mut limiter = PreviewRateLimiter::new();
        let per_actor = PREVIEW_GUILD_PER_DAY / 10;
        for actor in 0..10 {
            let name = format!("actor-{actor}");
            for i in 0..per_actor {
                // Each actor stays inside its own 5 s and hourly budgets.
                let at = i as u64 * (PREVIEW_MIN_INTERVAL_MS + 1) + 1;
                assert_eq!(
                    limiter.check_and_admit(&name, GUILD, i as u64, at),
                    PreviewRateDecision::Admit,
                    "{name} item {i}"
                );
            }
        }
        assert_eq!(
            limiter.check_and_admit("actor-new", GUILD, 1, 5 * HOUR_MS),
            PreviewRateDecision::RateLimited
        );
    }

    #[test]
    fn redelivery_replays_without_charging() {
        let mut limiter = PreviewRateLimiter::new();
        assert_eq!(
            limiter.check_and_admit(ACTOR, GUILD, 7, 0),
            PreviewRateDecision::Admit
        );
        assert_eq!(
            limiter.check_and_admit(ACTOR, GUILD, 7, 1_000),
            PreviewRateDecision::ReplayDuplicate
        );
        // The replay consumed no 5 s budget: a fresh interaction is refused.
        assert_eq!(
            limiter.check_and_admit(ACTOR, GUILD, 8, 1_000),
            PreviewRateDecision::RateLimited
        );
        assert_eq!(
            limiter.check_and_admit(ACTOR, GUILD, 8, PREVIEW_MIN_INTERVAL_MS),
            PreviewRateDecision::Admit
        );
    }

    #[test]
    fn full_map_refuses_new_actor_but_reuses_expired() {
        let mut limiter = PreviewRateLimiter::with_capacity(2);
        assert_eq!(
            limiter.check_and_admit("a", GUILD, 1, 0),
            PreviewRateDecision::Admit
        );
        assert_eq!(
            limiter.check_and_admit("b", GUILD, 1, 0),
            PreviewRateDecision::Admit
        );
        assert_eq!(
            limiter.check_and_admit("c", GUILD, 1, 0),
            PreviewRateDecision::RateLimited
        );
        // After a day both entries expire and the slots are reusable.
        assert_eq!(
            limiter.check_and_admit("c", GUILD, 2, DAY_MS + 1),
            PreviewRateDecision::Admit
        );
        // A known actor's redelivery still replays even at capacity.
        assert_eq!(
            limiter.check_and_admit("c", GUILD, 2, DAY_MS + 2),
            PreviewRateDecision::ReplayDuplicate
        );
    }
}
