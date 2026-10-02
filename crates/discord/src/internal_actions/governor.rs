//! One in-memory 429 cooldown governor per bot token.
//!
//! It only refuses: it never queues, sleeps or retries, so repeated 429s
//! cannot keep any operation alive. State is per process and bounded; once
//! `MAX_CHANNEL_HOLDS` live channel holds exist, a further channel hold widens
//! to the token-wide hold rather than evicting a live one.

use super::{CooldownScope, RateLimitCooldown};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};
use tokio::time::Instant;
use twilight_model::id::{marker::ChannelMarker, Id};

/// Fixed ceiling on tracked channel holds.
pub const MAX_CHANNEL_HOLDS: usize = 1024;

/// Monotonic time source. The default reads Tokio's clock, so paused-time
/// tests drive expiry without sleeping.
pub type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

#[derive(Debug, Clone, Copy, Default)]
struct Hold {
    until: Option<Instant>,
    /// Timing was missing or unrepresentable: held until reconciled.
    pending: bool,
}

impl Hold {
    fn active(self, now: Instant) -> bool {
        self.pending || self.until.is_some_and(|until| now < until)
    }

    /// Only ever lengthens: a shorter later cooldown never cuts a hold short.
    fn extend(&mut self, now: Instant, retry_after_ms: Option<u64>) {
        match retry_after_ms.and_then(|ms| now.checked_add(Duration::from_millis(ms))) {
            Some(until) => self.until = self.until.max(Some(until)),
            None => self.pending = true,
        }
    }
}

#[derive(Default)]
struct State {
    global: Hold,
    channels: HashMap<Id<ChannelMarker>, Hold>,
}

impl State {
    /// The channel's hold, or the token-wide one while the ceiling is full of
    /// live holds. Expired holds are reclaimed first.
    fn channel_hold(&mut self, channel: Id<ChannelMarker>, now: Instant) -> &mut Hold {
        if !self.channels.contains_key(&channel) && self.channels.len() >= MAX_CHANNEL_HOLDS {
            self.channels.retain(|_, hold| hold.active(now));
            if self.channels.len() >= MAX_CHANNEL_HOLDS {
                return &mut self.global;
            }
        }
        self.channels.entry(channel).or_default()
    }
}

/// Construct one per bot token and hand clones to every executor and Discord
/// transport using that token; clones share the same holds.
#[derive(Clone)]
pub struct CooldownGovernor {
    state: Arc<Mutex<State>>,
    clock: Clock,
}

impl Default for CooldownGovernor {
    fn default() -> Self {
        Self::new()
    }
}

impl CooldownGovernor {
    #[must_use]
    pub fn new() -> Self {
        Self::with_clock(Arc::new(Instant::now))
    }

    #[must_use]
    pub fn with_clock(clock: Clock) -> Self {
        Self {
            state: Arc::default(),
            clock,
        }
    }

    /// Install a cooldown before any later, independent intent is admitted.
    pub fn record(&self, cooldown: RateLimitCooldown) {
        let now = (self.clock)();
        let mut state = self.lock();
        let hold = match cooldown.scope {
            CooldownScope::Global => &mut state.global,
            CooldownScope::Channel(channel) => state.channel_hold(channel, now),
        };
        hold.extend(now, cooldown.retry_after_ms);
    }

    /// False while a token-wide or matching channel hold is active.
    #[must_use]
    pub fn admits(&self, channel: Id<ChannelMarker>) -> bool {
        let now = (self.clock)();
        let mut state = self.lock();
        if state.global.active(now) {
            return false;
        }
        match state.channels.get(&channel).map(|hold| hold.active(now)) {
            Some(true) => false,
            Some(false) => {
                state.channels.remove(&channel);
                true
            }
            None => true,
        }
    }

    /// Independent reconciliation lifts an untimed hold. Timed holds in the
    /// same scope still run to expiry.
    pub fn reconcile(&self, scope: CooldownScope) {
        let mut state = self.lock();
        match scope {
            CooldownScope::Global => state.global.pending = false,
            CooldownScope::Channel(channel) => {
                if let Some(hold) = state.channels.get_mut(&channel) {
                    hold.pending = false;
                }
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // Each update is a single field write; a poisoned guard is still whole.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn channel(n: u64) -> Id<ChannelMarker> {
        Id::new(100_000_000_000_000_000 + n)
    }

    fn cooldown(scope: CooldownScope, ms: Option<u64>) -> RateLimitCooldown {
        RateLimitCooldown {
            scope,
            retry_after_ms: ms,
        }
    }

    async fn advance_ms(ms: u64) {
        tokio::time::advance(Duration::from_millis(ms)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn channel_hold_refuses_only_that_channel_until_expiry() {
        let governor = CooldownGovernor::new();
        let shared = governor.clone();
        governor.record(cooldown(CooldownScope::Channel(channel(1)), Some(1500)));
        assert!(!shared.admits(channel(1)));
        assert!(shared.admits(channel(2)));
        advance_ms(1499).await;
        assert!(!shared.admits(channel(1)));
        advance_ms(1).await;
        assert!(shared.admits(channel(1)));
        assert!(
            governor.lock().channels.is_empty(),
            "expired hold reclaimed"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn global_hold_refuses_every_channel_until_expiry() {
        let governor = CooldownGovernor::new();
        governor.record(cooldown(CooldownScope::Global, Some(2000)));
        advance_ms(1999).await;
        assert!((0..50).all(|n| !governor.admits(channel(n))));
        advance_ms(1).await;
        assert!((0..50).all(|n| governor.admits(channel(n))));
    }

    #[tokio::test(start_paused = true)]
    async fn later_shorter_cooldowns_never_shorten_a_hold() {
        let governor = CooldownGovernor::new();
        let scope = CooldownScope::Channel(channel(1));
        governor.record(cooldown(scope, Some(5000)));
        advance_ms(1000).await;
        governor.record(cooldown(scope, Some(10)));
        governor.record(cooldown(CooldownScope::Global, Some(0)));
        advance_ms(3999).await;
        assert!(!governor.admits(channel(1)));
        advance_ms(1).await;
        assert!(governor.admits(channel(1)));
    }

    #[tokio::test(start_paused = true)]
    async fn untimed_holds_wait_for_reconciliation_not_a_guessed_delay() {
        let governor = CooldownGovernor::new();
        let scope = CooldownScope::Channel(channel(1));
        governor.record(cooldown(scope, Some(1000)));
        governor.record(cooldown(scope, None));
        advance_ms(86_400_000).await;
        assert!(!governor.admits(channel(1)));
        governor.reconcile(scope);
        assert!(governor.admits(channel(1)));

        governor.record(cooldown(CooldownScope::Global, None));
        governor.record(cooldown(CooldownScope::Global, Some(1000)));
        governor.reconcile(CooldownScope::Global);
        assert!(!governor.admits(channel(3)), "timed part still runs out");
        advance_ms(1000).await;
        assert!(governor.admits(channel(3)));
    }

    #[tokio::test(start_paused = true)]
    async fn key_ceiling_holds_across_many_channels_without_dropping_a_live_hold() {
        let governor = CooldownGovernor::new();
        let total = 3 * MAX_CHANNEL_HOLDS as u64;
        for n in 0..total {
            governor.record(cooldown(CooldownScope::Channel(channel(n)), Some(1000 + n)));
            assert!(governor.lock().channels.len() <= MAX_CHANNEL_HOLDS);
        }
        // Overflow widened to a token-wide hold for the longest overflow wait.
        assert!((0..total + 10).all(|n| !governor.admits(channel(n))));
        advance_ms(1000 + total - 2).await;
        assert!(!governor.admits(channel(total + 1)));
        advance_ms(1).await;
        assert!(governor.admits(channel(total + 1)));

        // Expired holds are reclaimed before overflow widens again.
        governor.record(cooldown(CooldownScope::Channel(channel(total)), Some(10)));
        assert_eq!(governor.lock().channels.len(), 1);
        assert!(governor.admits(channel(0)));
        assert!(!governor.admits(channel(total)));
    }

    #[test]
    fn injected_clock_drives_expiry() {
        let base = Instant::now();
        let elapsed = Arc::new(AtomicU64::new(0));
        let reading = elapsed.clone();
        let governor = CooldownGovernor::with_clock(Arc::new(move || {
            base + Duration::from_millis(reading.load(Ordering::SeqCst))
        }));
        governor.record(cooldown(CooldownScope::Global, Some(250)));
        assert!(!governor.admits(channel(1)));
        elapsed.store(250, Ordering::SeqCst);
        assert!(governor.admits(channel(1)));
    }
}
