//! Dark arithmetic for a notification member budget; not a delivery limiter.
//!
//! A future store must select one state per canonical member across both verbs
//! and all callers, and atomically commit an admission with its idempotency
//! receipt. Replays and other non-claims must not call `admit`. This module
//! alone provides neither serialization nor durable enforcement.

const DAY_MS: u64 = 86_400_000;
const TOKEN_MS: u64 = DAY_MS / 10;
const BURST_MS: u64 = 3 * TOKEN_MS;
const DAY_LIMIT: usize = 10;

/// Persist all fields together. Times and credits are integer milliseconds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationBudget {
    credit_ms: u64,
    updated_ms: u64,
    admissions: Vec<u64>,
}

impl NotificationBudget {
    /// Create a full budget for a member with no prior admissions.
    #[must_use]
    pub fn new(now_ms: u64) -> Self {
        Self {
            credit_ms: BURST_MS,
            updated_ms: now_ms,
            admissions: Vec::new(),
        }
    }

    /// Restore a trusted storage snapshot, rejecting malformed state.
    #[must_use]
    pub fn restore(credit_ms: u64, updated_ms: u64, admissions: Vec<u64>) -> Option<Self> {
        if credit_ms > BURST_MS
            || admissions.len() > DAY_LIMIT
            || admissions.windows(2).any(|pair| pair[0] > pair[1])
            || admissions.last().is_some_and(|&time| time > updated_ms)
        {
            return None;
        }
        Some(Self {
            credit_ms,
            updated_ms,
            admissions,
        })
    }

    /// Return the exact state fields for durable storage.
    #[must_use]
    pub fn snapshot(&self) -> (u64, u64, &[u64]) {
        (self.credit_ms, self.updated_ms, &self.admissions)
    }

    /// Return the next state for a new claim, or the retry delay in seconds.
    /// Denial never changes the stored state. The caller must commit the
    /// returned state only when the associated receipt claim also commits.
    pub fn admit(&self, now_ms: u64) -> Result<Self, u64> {
        let mut next = self.clone();
        next.credit_ms = next
            .credit_ms
            .saturating_add(now_ms.saturating_sub(next.updated_ms))
            .min(BURST_MS);
        next.updated_ms = next.updated_ms.max(now_ms);
        next.admissions
            .retain(|&time| next.updated_ms - time < DAY_MS);
        let recovery = next.updated_ms - now_ms;
        let burst_wait = if next.credit_ms < TOKEN_MS {
            recovery.saturating_add(TOKEN_MS - next.credit_ms)
        } else {
            0
        };
        let daily_wait = if next.admissions.len() >= DAY_LIMIT {
            recovery.saturating_add(DAY_MS - (next.updated_ms - next.admissions[0]))
        } else {
            0
        };
        let wait = burst_wait.max(daily_wait);
        if wait > 0 {
            return Err(wait.div_ceil(1000));
        }
        next.credit_ms -= TOKEN_MS;
        next.admissions.push(next.updated_ms);
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_refill_and_denial_do_not_spend() {
        let mut state = NotificationBudget::new(0);
        for _ in 0..3 {
            state = state.admit(0).unwrap();
        }
        assert_eq!(state.admit(0), Err(8640));
        assert_eq!(state.admit(TOKEN_MS - 1), Err(1));
        state = state.admit(TOKEN_MS).unwrap();
        assert_eq!(state.admit(TOKEN_MS), Err(8640));
    }

    #[test]
    fn rolling_day_includes_burst_and_denials() {
        let mut state = NotificationBudget::new(0);
        for _ in 0..3 {
            state = state.admit(0).unwrap();
        }
        for step in 1..=7 {
            state = state.admit(step * TOKEN_MS).unwrap();
        }
        assert_eq!(state.admit(8 * TOKEN_MS), Err(17_280));
        assert_eq!(state.admit(DAY_MS - 1), Err(1));
        for _ in 0..3 {
            state = state.admit(DAY_MS).unwrap();
        }
        assert_eq!(state.admit(DAY_MS), Err(8640));
        assert_eq!(state.snapshot().2.len(), 10);
    }

    #[test]
    fn rollback_delays_refill_without_double_credit() {
        let mut state = NotificationBudget::new(1000);
        for _ in 0..3 {
            state = state.admit(1000).unwrap();
        }
        assert_eq!(state.admit(0), Err(8641));
        assert_eq!(state.admit(1000), Err(8640));
        state = state.admit(1000 + TOKEN_MS).unwrap();
        assert_eq!(state.admit(1000), Err(17_280));
    }

    #[test]
    fn snapshots_round_trip_and_invalid_states_are_rejected() {
        let state = NotificationBudget::new(10).admit(10).unwrap();
        let (credit, updated, times) = state.snapshot();
        assert_eq!(
            NotificationBudget::restore(credit, updated, times.to_vec()),
            Some(state)
        );
        assert!(NotificationBudget::restore(BURST_MS + 1, 10, vec![]).is_none());
        assert!(NotificationBudget::restore(0, 10, vec![0; 11]).is_none());
        assert!(NotificationBudget::restore(0, 10, vec![9, 8]).is_none());
        assert!(NotificationBudget::restore(0, 10, vec![11]).is_none());
    }
}
