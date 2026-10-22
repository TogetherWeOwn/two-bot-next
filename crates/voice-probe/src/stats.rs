//! Loss and latency math for probe runs.
//!
//! Pure functions over counts and samples: the runner collects, these
//! summarize. Percentiles use the nearest-rank method.

use serde::Serialize;

/// RTT summary in milliseconds. Every field is `None` when the run
/// collected no samples, so the evidence object keeps its keys on a
/// failed run instead of inventing zeros.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct RttSummary {
    /// Smallest sample, if any.
    pub min: Option<f64>,
    /// Nearest-rank 50th percentile, if any.
    pub p50: Option<f64>,
    /// Nearest-rank 95th percentile, if any.
    pub p95: Option<f64>,
    /// Largest sample, if any.
    pub max: Option<f64>,
}

/// Percentage of sent packets never received, rounded to two decimals.
/// Zero sent means nothing was lost, and overcounted receipts clamp to
/// zero rather than reporting a negative loss.
#[must_use]
pub fn loss_pct(sent: u64, received: u64) -> f64 {
    if sent == 0 {
        return 0.0;
    }
    let lost = sent.saturating_sub(received);
    let raw = (lost as f64) / (sent as f64) * 100.0;
    (raw * 100.0).round() / 100.0
}

/// Summarize RTT samples (milliseconds) without mutating the input.
#[must_use]
pub fn summarize_rtt(samples_ms: &[f64]) -> RttSummary {
    if samples_ms.is_empty() {
        return RttSummary {
            min: None,
            p50: None,
            p95: None,
            max: None,
        };
    }
    let mut sorted = samples_ms.to_vec();
    sorted.sort_by(f64::total_cmp);
    RttSummary {
        min: Some(sorted[0]),
        p50: Some(nearest_rank(&sorted, 50.0)),
        p95: Some(nearest_rank(&sorted, 95.0)),
        max: Some(sorted[sorted.len() - 1]),
    }
}

/// Nearest-rank percentile of a sorted, non-empty sample list.
fn nearest_rank(sorted: &[f64], pct: f64) -> f64 {
    let rank = ((pct / 100.0 * sorted.len() as f64).ceil() as usize).max(1);
    sorted[rank - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loss_is_zero_when_nothing_sent() {
        assert_eq!(loss_pct(0, 0), 0.0);
    }

    #[test]
    fn loss_math() {
        assert_eq!(loss_pct(100, 100), 0.0);
        assert_eq!(loss_pct(100, 95), 5.0);
        assert_eq!(loss_pct(100, 0), 100.0);
        assert_eq!(loss_pct(3, 2), 33.33);
        assert_eq!(loss_pct(1000, 999), 0.1);
    }

    #[test]
    fn loss_clamps_overcounted_receipts() {
        assert_eq!(loss_pct(10, 12), 0.0);
    }

    #[test]
    fn rtt_summary_empty_has_no_samples() {
        let summary = summarize_rtt(&[]);

        assert_eq!(
            summary,
            RttSummary {
                min: None,
                p50: None,
                p95: None,
                max: None,
            }
        );
    }

    #[test]
    fn rtt_summary_singleton_repeats_the_sample() {
        let summary = summarize_rtt(&[2.5]);

        assert_eq!(
            summary,
            RttSummary {
                min: Some(2.5),
                p50: Some(2.5),
                p95: Some(2.5),
                max: Some(2.5),
            }
        );
    }

    #[test]
    fn rtt_summary_nearest_rank() {
        let samples: Vec<f64> = (1..=10).map(f64::from).collect();
        let summary = summarize_rtt(&samples);

        assert_eq!(summary.min, Some(1.0));
        assert_eq!(summary.max, Some(10.0));
        assert_eq!(summary.p50, Some(5.0));
        assert_eq!(summary.p95, Some(10.0));
    }

    #[test]
    fn rtt_summary_sorts_unsorted_input() {
        let summary = summarize_rtt(&[9.0, 1.0, 5.0, 3.0, 7.0]);

        assert_eq!(summary.min, Some(1.0));
        assert_eq!(summary.max, Some(9.0));
        assert_eq!(summary.p50, Some(5.0));
    }
}
