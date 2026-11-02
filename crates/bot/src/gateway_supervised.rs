//! Supervised gateway reconnect runner: retries with published state.
//!
//! The retry schedule itself lives in the library (`two_bot::gateway_backoff`,
//! re-exported below so the contract tests keep one import path) and the
//! gateway receive loop already paces `Reconnect` failures on it. This module
//! adds the supervised episode runner: retries wait on the schedule, readyz
//! reports [`GatewayState::Armed`] (`starting`) while a retry is pending, and
//! the runner gives up after [`ReconnectPolicy`] attempts instead of spinning
//! forever.
//!
//! The acceptance tests (`gateway_tests::supervised_reconnect`) pin this
//! module's contract: the schedule and [`run_supervised`]'s retry loop.
//! The runner is still test-only until the gateway loop adopts it: no runtime
//! caller yet, so it is gated to test builds and ships no code to the
//! container.
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

pub use two_bot::gateway_backoff::{reconnect_delay, ReconnectPolicy};

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
    /// A shutdown request arrived, or draining began, before or during the
    /// episode. [`GatewayState::Draining`] is sticky: once observed the
    /// runner never overwrites it and makes no further attempts.
    ShutdownRequested,
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
/// closed. Shutdown wins immediately: it is checked at the top of each
/// iteration and raced (biased, shutdown first) against every `connect` and
/// backoff sleep, so a SIGTERM during a sleep or a hanging connect ends the
/// episode without waiting out the delay — the same "SIGTERM never waits"
/// shape as `publish_gateway_failure` in `main.rs`. Draining is sticky, as at
/// every other publish site: if an outside owner publishes
/// [`GatewayState::Draining`] during the unlocked `connect` or backoff sleep,
/// neither publish overwrites it (readyz is never resurrected to ready or
/// starting after draining begins) and the episode ends with
/// [`SupervisedExit::ShutdownRequested`] instead of retrying into the drain.
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
        // A shutdown that is already pending wins before any attempt. The
        // `connect` and backoff sleep below are also raced against shutdown
        // (biased, shutdown first): the recorded attempt timestamps still
        // advance by exactly the scheduled delay when shutdown never fires.
        tokio::select! {
            biased;
            () = &mut shutdown => return SupervisedExit::ShutdownRequested,
            () = std::future::ready(()) => {}
        }
        // Draining published during the previous backoff sleep (or before
        // the episode started) is terminal: make no further attempts.
        if *state.read().await == GatewayState::Draining {
            return SupervisedExit::ShutdownRequested;
        }
        // The connect future is built lazily inside the raced branch: calling
        // `connect(attempt)` while building the `select!` futures would
        // construct (and drop unpolled) an attempt even when shutdown wins
        // the race. Deferring the call into the branch means a shutdown that
        // is already ready never starts an attempt.
        let outcome = tokio::select! {
            biased;
            () = &mut shutdown => return SupervisedExit::ShutdownRequested,
            outcome = async {
                Box::into_pin(connect(attempt)).await
            } => outcome,
        };
        match outcome {
            AttemptOutcome::Connected => {
                // Guard Draining set during the unlocked connect await, the
                // same stickiness every other Connected publish keeps.
                let mut guard = state.write().await;
                if *guard == GatewayState::Draining {
                    return SupervisedExit::ShutdownRequested;
                }
                *guard = GatewayState::Connected;
                return SupervisedExit::Reconnected;
            }
            AttemptOutcome::Retryable => {
                // Check Draining before either publishing Armed or exhausting
                // the budget. The final attempt still has to honor a drain
                // that arrived during its unlocked connect await.
                let mut guard = state.write().await;
                if *guard == GatewayState::Draining {
                    return SupervisedExit::ShutdownRequested;
                }
                if attempt + 1 >= policy.max_attempts {
                    *guard = GatewayState::Draining;
                    return SupervisedExit::BudgetExhausted;
                }
                *guard = GatewayState::Armed;
                drop(guard);
                // `attempt` is within budget (checked above), so the delay
                // exists; fall back to no wait rather than panic if a future
                // policy rule ever disagrees. Race the sleep against
                // shutdown so a SIGTERM during backoff ends the episode
                // without waiting out the delay.
                let delay = reconnect_delay(&policy, attempt).unwrap_or(Duration::ZERO);
                tokio::select! {
                    biased;
                    () = &mut shutdown => return SupervisedExit::ShutdownRequested,
                    () = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
    *state.write().await = GatewayState::Draining;
    SupervisedExit::BudgetExhausted
}
