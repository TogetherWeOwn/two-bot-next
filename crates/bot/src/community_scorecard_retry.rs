//! Pure retry planning; the adapter commits each reservation before doing work.

use two_bot_core::scorecard_tick;

pub(super) const MAX_WEEKLY_ATTEMPTS: i32 = 3;
pub(super) const RETRY_DELAY_MS: i64 = 5 * 60_000;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct AttemptState {
    pub week_key: String,
    pub attempts: i32,
    pub next_attempt_at: i64,
    pub completed: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Decision {
    Attempt(AttemptState),
    Wait,
    Skip,
}

/// Only eligible ticks retry. A new Monday has a fresh budget; failed,
/// cancelled and crashed attempts leave the committed reservation unchanged.
pub(super) fn decide(state: &AttemptState, now_ms: i64) -> Decision {
    let Some(week_key) = scorecard_tick(now_ms, None) else {
        return Decision::Skip;
    };
    let state = if state.week_key == week_key {
        state.clone()
    } else {
        AttemptState::default()
    };
    if state.completed || state.attempts >= MAX_WEEKLY_ATTEMPTS {
        return Decision::Skip;
    }
    if now_ms < state.next_attempt_at {
        return Decision::Wait;
    }
    Decision::Attempt(AttemptState {
        week_key,
        attempts: state.attempts + 1,
        next_attempt_at: now_ms.saturating_add(RETRY_DELAY_MS),
        completed: false,
    })
}
