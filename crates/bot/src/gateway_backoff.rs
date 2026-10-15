//! Bounded gateway reconnect backoff: the gateway retry contract.
//!
//! A dead gateway process exits and the container supervisor restarts it, but
//! a restart hot loop is unbounded work (the October crash-loop week). The
//! paired build card supervises reconnects in-process instead: retries wait on
//! this schedule, readyz reports [`GatewayState::Armed`] (`starting`) while a
//! retry is pending, and the runner gives up after [`ReconnectPolicy`]
//! attempts instead of spinning forever.
//!
//! The acceptance tests (`gateway_tests::supervised_reconnect`) pin this
//! module's contract: the schedule and [`run_supervised`]'s retry loop.
//! Test-only until the gateway loop adopts it: no runtime caller yet, so the
//! module is gated to test builds and ships no code to the container.
//!
//! The loop (keep the polling exact so paused-time tests observe it):
//!
//! ```text
//! for attempt in 0..policy.max_attempts:
//!     match connect(attempt).await:
//!         Connected => publish Connected, return Reconnected
//!         Retryable => publish Armed, sleep reconnect_delay(policy, attempt)
//!                      (skipped after the final budgeted attempt)
//! publish Draining, return BudgetExhausted
//! ```
//!
//! The sleep must be `tokio::time::sleep` (not a wall-clock wait) so the
//! attempt timestamps the tests record advance by exactly the scheduled delay
//! under paused time. Never hold the state lock across `connect` or the
//! sleep; the connector reads the state to record what readyz served.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;

use super::gateway::GatewayState;

/// First retry waits this long; each later attempt doubles, capped below.
pub const DEFAULT_BASE_DELAY: Duration = Duration::from_secs(1);
/// No single retry ever waits longer than this.
pub const DEFAULT_MAX_DELAY: Duration = Duration::from_secs(30);
/// The runner stops retrying after this many attempts: no unbounded hot loop.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 10;
/// Default jitter seed. Tests pass explicit seeds; production reseeds per run.
pub const DEFAULT_SEED: u64 = 0x9E3779B97F4A7C15;

/// Bounded retry budget for one supervised reconnect episode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectPolicy {
    /// Delay before the first retry; doubled per attempt, capped at
    /// [`Self::max_delay`].
    pub base_delay: Duration,
    /// Ceiling for any single retry wait.
    pub max_delay: Duration,
    /// Attempts before the runner reports exhaustion. [`reconnect_delay`]
    /// returns `None` at and past this bound.
    pub max_attempts: u32,
    /// Deterministic jitter seed: the same seed replays the same delays, so
    /// tests pin exact timestamps without a clock or a global RNG.
    pub seed: u64,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            base_delay: DEFAULT_BASE_DELAY,
            max_delay: DEFAULT_MAX_DELAY,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            seed: DEFAULT_SEED,
        }
    }
}

/// Outcome of one connect attempt, as reported by the injected connector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptOutcome {
    /// The gateway is back; publish [`GatewayState::Connected`].
    Connected,
    /// The attempt failed transiently; back off and retry within budget.
    Retryable,
}

/// How a supervised reconnect episode ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisedExit {
    /// A retry connected; the gateway is serving again.
    Reconnected,
    /// Every attempt failed; the runner published [`GatewayState::Draining`]
    /// so readyz fails closed and the container supervisor takes over.
    BudgetExhausted,
    /// A shutdown request arrived before or during the episode.
    ShutdownRequested,
}

/// Deterministic 64-bit mixer (splitmix64) for jitter without a global RNG.
///
/// Pure integer arithmetic: the same `(seed, attempt)` pair always yields the
/// same stream, on every platform, with no clock and no `rand` dependency.
fn jitter_stream(seed: u64, attempt: u32) -> u64 {
    let mixed = seed.wrapping_add(u64::from(attempt).wrapping_mul(0x9E3779B97F4A7C15));
    let mut z = mixed.wrapping_add(0x9E3779B97F4A7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

/// Wait before retrying `attempt` (0-based), or `None` past the budget.
///
/// Growth is `base_delay * 2^attempt`, saturated and capped at `max_delay`,
/// with equal jitter: the returned delay lies in `[capped/2, capped]`, where
/// the top half is drawn deterministically from `(seed, attempt)`. Equal
/// jitter (not full jitter) keeps a floor under each wait so a herd of
/// restarting shards cannot collapse onto near-zero synchronized retries,
/// while still decorrelating them. Attempt indices at or past `max_attempts`
/// return `None`: the type itself carries "no unbounded retry".
#[must_use]
pub fn reconnect_delay(policy: &ReconnectPolicy, attempt: u32) -> Option<Duration> {
    if attempt >= policy.max_attempts {
        return None;
    }
    // The shift is at most 31, so the multiply factor never overflows u32.
    let grown = policy.base_delay.saturating_mul(1_u32 << attempt.min(31));
    let capped = grown.min(policy.max_delay);
    let half = capped / 2;
    // `span` is at least 1, so the remainder below never divides by zero; it
    // is capped at `u64::MAX` so the cast is exact even for absurd policies.
    let span = half.as_nanos().saturating_add(1).min(u128::from(u64::MAX)) as u64;
    Some(half + Duration::from_nanos(jitter_stream(policy.seed, attempt) % span))
}

/// Run one supervised reconnect episode against an injected connector.
///
/// `connect` performs attempt `n` and reports its outcome; `shutdown`
/// resolves when the process must stop. The connector may read `state` (to
/// record what readyz served) but the runner never holds the state lock
/// across `connect` or a backoff sleep.
///
/// The connector returns a boxed attempt future; the runner converts that
/// box into a `Pin<Box<dyn Future>>` before polling (never `Box::pin` on the
/// box — a second box still needs the `dyn` future to be `Unpin`), so callers
/// never have to name the exact future type. Retryable
/// outcomes publish [`GatewayState::Armed`] (readyz serves `starting`) and
/// wait exactly [`reconnect_delay`] before the next attempt, except after the
/// final budgeted attempt, where there is no next attempt to wait for.
/// Exhausting the budget publishes [`GatewayState::Draining`] so readyz fails
/// closed. A shutdown that is already pending wins before any attempt
/// (checked at the top of each iteration, so one arriving during a backoff
/// sleep ends the episode before the next attempt).
pub async fn run_supervised<C, S>(
    state: Arc<RwLock<GatewayState>>,
    policy: ReconnectPolicy,
    mut connect: C,
    shutdown: S,
) -> SupervisedExit
where
    C: FnMut(u32) -> Box<dyn Future<Output = AttemptOutcome> + Send>,
    S: Future<Output = ()>,
{
    tokio::pin!(shutdown);
    for attempt in 0..policy.max_attempts {
        // A shutdown that is already pending wins before any attempt. Poll
        // `connect` and the backoff sleep directly (not inside `select!`):
        // racing them against `shutdown` changes how the paused clock in
        // tests observes the timers, and the recorded attempt timestamps
        // must advance by exactly the scheduled delay.
        tokio::select! {
            biased;
            () = &mut shutdown => return SupervisedExit::ShutdownRequested,
            () = std::future::ready(()) => {}
        }
        let outcome = Box::into_pin(connect(attempt)).await;
        match outcome {
            AttemptOutcome::Connected => {
                *state.write().await = GatewayState::Connected;
                return SupervisedExit::Reconnected;
            }
            AttemptOutcome::Retryable => {
                if attempt + 1 >= policy.max_attempts {
                    break;
                }
                *state.write().await = GatewayState::Armed;
                // `attempt` is within budget (checked above), so the delay
                // exists; fall back to no wait rather than panic if a future
                // policy rule ever disagrees.
                let delay = reconnect_delay(&policy, attempt).unwrap_or(Duration::ZERO);
                tokio::time::sleep(delay).await;
            }
        }
    }
    *state.write().await = GatewayState::Draining;
    SupervisedExit::BudgetExhausted
}
