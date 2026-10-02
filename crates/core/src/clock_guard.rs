//! Fail-closed wall-clock policy for internal-action freshness decisions (threat-model F8).
//!
//! `NonceCache`'s TTL math assumes the freshness and expiry clocks advance
//! together without rollback: [`crate::internal_actions::within_skew`] compares
//! in whole seconds and [`crate::internal_actions::NonceCache`] drops entries
//! past the TTL, so expiry followed by wall-clock rollback can reopen a signed
//! window whose nonce is already forgotten. Saturating subtraction does not fix
//! that case — it protects entries still retained in memory, not entries the
//! sweep has already dropped.
//!
//! This guard records the highest freshness-clock reading observed so far (the
//! high-water mark) and refuses any freshness decision whose clock reads
//! further below it than the documented tolerance. Time within the tolerance is
//! evaluated against the high-water mark, never against the regressed reading,
//! so a sweep can never reopen a signed window the high-water mark has already
//! passed. Forward jumps behave as before: the first observation at or above
//! the mark advances it.
//!
//! The tolerance exists for one narrow case: two legitimate readers sampling
//! the same logical clock out of order (a lock or pool wait that sampled its
//! freshness instant before an earlier-committed row's instant). Reads more
//! than [`CLOCK_SKEW_TOLERANCE_MS`] behind the mark are indistinguishable from
//! rollback and refuse. Callers that cross process or failover boundaries must
//! persist the mark (or re-derive it from burned nonces) and restore it before
//! evaluating freshness — a fresh process that starts from zero would
//! otherwise accept a regressed clock as new.
//!
//! State is bounded: only the mark and the tolerance-window floor are kept, no
//! per-nonce or per-request history.

/// Tolerance for out-of-order freshness-clock reads, milliseconds.
///
/// A freshness sample up to this far below the high-water mark is evaluated
/// against the mark rather than refused: it covers a lock or pool wait that
/// sampled its clock before an earlier-committed observation advanced the
/// mark, without opening a replay window (the mark, not the regressed read,
/// decides freshness). Anything further below the mark refuses as rollback.
///
/// Sized well under the ±120 s acceptance interval so the tolerance cannot
/// itself reopen an expired window: 5 s against a 241 s TTL.
pub const CLOCK_SKEW_TOLERANCE_MS: u64 = 5_000;

/// Fail-closed refusal when a freshness clock reads below the high-water mark
/// minus [`CLOCK_SKEW_TOLERANCE_MS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockRollback {
    /// The regressed reading that was refused, milliseconds.
    pub observed_ms: u64,
    /// The high-water mark it regressed from, milliseconds.
    pub high_water_ms: u64,
}

impl std::fmt::Display for ClockRollback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "freshness clock rolled back to {}ms below high-water {}ms",
            self.observed_ms, self.high_water_ms
        )
    }
}

impl std::error::Error for ClockRollback {}

/// Monotonic high-water guard over one freshness clock (one unit, one DB).
///
/// Times are explicit milliseconds, matching
/// [`crate::internal_actions::NonceCache`]'s injectable clock so tests move
/// time without sleeping. Each guard instance covers a single clock domain: a
/// process-local wall clock and a database `clock_timestamp()` need separate
/// guards, because their readings are not comparable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockGuard {
    high_water_ms: Option<u64>,
}

impl ClockGuard {
    /// Guard with no observations yet. The first evaluation sets the mark.
    #[must_use]
    pub fn new() -> Self {
        Self {
            high_water_ms: None,
        }
    }

    /// Restore a persisted mark, e.g. after restart or failover. A fresh
    /// process with an earlier clock must refuse old captures against the
    /// persisted mark rather than treating its own first read as new.
    #[must_use]
    pub fn restore(high_water_ms: u64) -> Self {
        Self {
            high_water_ms: Some(high_water_ms),
        }
    }

    /// The current high-water mark, if any observation has been recorded.
    /// Persist this across restart/failover and [`restore`](Self::restore) it
    /// before evaluating freshness.
    #[must_use]
    pub fn high_water_ms(&self) -> Option<u64> {
        self.high_water_ms
    }

    /// Evaluate a freshness-clock reading for a decision at `now_ms`.
    ///
    /// - At or above the mark: advances the mark, returns `Ok(now_ms)` — the
    ///   reading itself decides, exactly as before.
    /// - Within [`CLOCK_SKEW_TOLERANCE_MS`] below the mark: returns
    ///   `Ok(high_water_ms)` — the caller evaluates freshness against the
    ///   mark, so a slightly stale sample cannot reopen a window the mark has
    ///   already closed.
    /// - Further below the mark: returns `Err(ClockRollback)` — the caller
    ///   must refuse the freshness decision, fail-closed.
    /// - No mark yet: records and returns the reading.
    pub fn evaluate(&mut self, now_ms: u64) -> Result<u64, ClockRollback> {
        match self.high_water_ms {
            None => {
                self.high_water_ms = Some(now_ms);
                Ok(now_ms)
            }
            Some(mark) => {
                if now_ms >= mark {
                    self.high_water_ms = Some(now_ms);
                    Ok(now_ms)
                } else if mark - now_ms <= CLOCK_SKEW_TOLERANCE_MS {
                    Ok(mark)
                } else {
                    Err(ClockRollback {
                        observed_ms: now_ms,
                        high_water_ms: mark,
                    })
                }
            }
        }
    }
}

impl Default for ClockGuard {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_observation_sets_mark_and_decides() {
        let mut guard = ClockGuard::new();
        assert_eq!(guard.high_water_ms(), None);
        assert_eq!(guard.evaluate(1_000), Ok(1_000));
        assert_eq!(guard.high_water_ms(), Some(1_000));
    }

    #[test]
    fn forward_time_advances_mark() {
        let mut guard = ClockGuard::new();
        assert_eq!(guard.evaluate(1_000), Ok(1_000));
        assert_eq!(guard.evaluate(2_000), Ok(2_000));
        assert_eq!(guard.high_water_ms(), Some(2_000));
    }

    #[test]
    fn within_tolerance_evaluates_against_mark() {
        let mut guard = ClockGuard::new();
        assert_eq!(guard.evaluate(10_000), Ok(10_000));
        // A lock-wait sample 1 s behind the mark decides at the mark.
        assert_eq!(guard.evaluate(9_000), Ok(10_000));
        // The mark itself does not regress.
        assert_eq!(guard.high_water_ms(), Some(10_000));
        // Exactly at the tolerance edge still decides at the mark.
        assert_eq!(guard.evaluate(10_000 - CLOCK_SKEW_TOLERANCE_MS), Ok(10_000));
    }

    #[test]
    fn beyond_tolerance_refuses_fail_closed() {
        let mut guard = ClockGuard::new();
        assert_eq!(guard.evaluate(10_000), Ok(10_000));
        let err = guard
            .evaluate(10_000 - CLOCK_SKEW_TOLERANCE_MS - 1)
            .expect_err("rollback past tolerance must refuse");
        assert_eq!(
            err,
            ClockRollback {
                observed_ms: 10_000 - CLOCK_SKEW_TOLERANCE_MS - 1,
                high_water_ms: 10_000,
            }
        );
        // The refusal does not move the mark.
        assert_eq!(guard.high_water_ms(), Some(10_000));
    }

    #[test]
    fn restored_mark_refuses_earlier_clock() {
        // A new process with an earlier clock and the persisted mark refuses
        // old captures instead of treating its first read as new.
        let mut guard = ClockGuard::restore(50_000);
        assert_eq!(guard.high_water_ms(), Some(50_000));
        let err = guard
            .evaluate(1_000)
            .expect_err("earlier clock against restored mark must refuse");
        assert_eq!(
            err,
            ClockRollback {
                observed_ms: 1_000,
                high_water_ms: 50_000,
            }
        );
        // A clock at/above the restored mark resumes normally.
        assert_eq!(guard.evaluate(50_000), Ok(50_000));
        assert_eq!(guard.evaluate(60_000), Ok(60_000));
    }
}
