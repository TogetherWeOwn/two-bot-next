//! Pure voice-heat hourly accumulation; no collection or storage is activated here.

use std::collections::BTreeMap;
use time::OffsetDateTime;

/// One completed UTC hour of sampler output: how many five-minute samples
/// the hour holds and their summed guild-wide occupancy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HourBucket {
    /// Truncated to the UTC hour start before flushing.
    pub hour_start: OffsetDateTime,
    /// Five-minute samples recorded in the hour (at most 12).
    pub samples: i64,
    /// Summed `total_in_voice` across those samples.
    pub occupancy_sum: i64,
    /// Latest actual valid observation included in the hour, not flush time.
    /// Present iff `samples > 0`, including true zero-occupancy observations.
    pub last_observed_at: Option<OffsetDateTime>,
}

/// Truncate a sample timestamp to its UTC hour start. Pure over the clock so
/// bucketing pins without a database.
#[must_use]
pub fn truncate_to_hour(sample_at: OffsetDateTime) -> OffsetDateTime {
    let hour = sample_at.unix_timestamp().div_euclid(3_600) * 3_600;
    OffsetDateTime::from_unix_timestamp(hour).expect("hour truncation stays in range")
}

/// In-memory hourly buckets, keyed by UTC hour start. At most 14 days of
/// hourly buckets (336) survive a long database outage; beyond that the
/// oldest hour drops so memory stays bounded while the database
/// is unreachable.
const MAX_BUFFERED_HOURS: usize = 336;

/// Hourly accumulation of five-minute `total_in_voice` samples. Pure over the
/// injected clock so bucketing pins without a gateway cache or a database.
#[derive(Debug, Default)]
pub struct Accumulator {
    buckets: BTreeMap<time::OffsetDateTime, BufferedHour>,
}

#[derive(Debug, Default)]
struct BufferedHour {
    slots: u16,
    occupancy_sum: i64,
    last_observed_at: Option<time::OffsetDateTime>,
}

impl Accumulator {
    /// Record the first valid observation in each UTC five-minute slot.
    /// Retries in that slot cannot change its count, sum or actual observation
    /// timestamp. Negative totals clamp to zero; a full buffer evicts the
    /// oldest hour, never an unbounded grow.
    pub fn record(&mut self, sample_at: time::OffsetDateTime, total: i64) {
        let hour = truncate_to_hour(sample_at);
        let slot = (sample_at.unix_timestamp() - hour.unix_timestamp()) / 300;
        let mask = 1u16 << slot;
        let entry = self.buckets.entry(hour).or_default();
        if entry.slots & mask != 0 {
            return;
        }
        entry.slots |= mask;
        entry.occupancy_sum = entry.occupancy_sum.saturating_add(total.max(0));
        entry.last_observed_at = Some(
            entry
                .last_observed_at
                .map_or(sample_at, |at| at.max(sample_at)),
        );
        if self.buckets.len() > MAX_BUFFERED_HOURS {
            let oldest = *self.buckets.keys().next().expect("non-empty buffer");
            self.buckets.remove(&oldest);
        }
    }

    /// Snapshot every hour strictly before the current UTC hour. Keep the
    /// rows buffered across every await: the supervisor may cancel a tick.
    pub fn completed(&self, now: time::OffsetDateTime) -> Vec<HourBucket> {
        let current = truncate_to_hour(now);
        self.buckets
            .range(..current)
            .map(|(&hour_start, entry)| HourBucket {
                hour_start,
                samples: i64::from(entry.slots.count_ones()),
                occupancy_sum: entry.occupancy_sum,
                last_observed_at: entry.last_observed_at,
            })
            .collect()
    }

    /// Forget buckets only after their flush succeeds. The caller must ensure
    /// that its storage makes retries safe and does not acknowledge failed writes.
    pub fn acknowledge(&mut self, buckets: &[HourBucket]) {
        for bucket in buckets {
            self.buckets.remove(&bucket.hour_start);
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.buckets.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn truncates_samples_to_the_utc_hour() {
        let hour_start = hour(19);
        assert_eq!(
            truncate_to_hour(
                hour_start + time::Duration::minutes(37) + time::Duration::seconds(42)
            ),
            hour_start
        );
        assert_eq!(truncate_to_hour(hour_start), hour_start);
        assert_eq!(
            truncate_to_hour(time::OffsetDateTime::from_unix_timestamp(-1).expect("fixture")),
            hour(-1)
        );
    }
    fn hour(h: i64) -> time::OffsetDateTime {
        time::OffsetDateTime::from_unix_timestamp(h * 3_600).expect("fixture hour")
    }

    #[test]
    fn completed_hours_acknowledge_and_live_hour_stays() {
        let mut acc = Accumulator::default();
        acc.record(hour(10) + time::Duration::minutes(5), 4);
        acc.record(hour(10) + time::Duration::minutes(10), 6);
        acc.record(hour(11) + time::Duration::minutes(2), 3);
        let drained = acc.completed(hour(11) + time::Duration::minutes(30));
        assert_eq!(drained.len(), 1, "only the completed hour flushes");
        assert_eq!(drained[0].hour_start, hour(10));
        assert_eq!((drained[0].samples, drained[0].occupancy_sum), (2, 10));
        assert_eq!(acc.len(), 2, "in-flight rows stay buffered");
        acc.acknowledge(&drained);
        assert_eq!(acc.len(), 1, "the live hour keeps accumulating");
        // A later tick completes the next hour too.
        let drained = acc.completed(hour(12));
        assert_eq!(drained.len(), 1);
        assert_eq!((drained[0].samples, drained[0].occupancy_sum), (1, 3));
        acc.acknowledge(&drained);
        assert_eq!(acc.len(), 0);
    }

    #[test]
    fn failed_flush_preserves_unsent_counts_without_double_count() {
        let mut acc = Accumulator::default();
        acc.record(hour(9) + time::Duration::minutes(5), 4);
        let pending = acc.completed(hour(10));
        assert_eq!(pending.len(), 1);
        // A failed connection/flush never acknowledges the snapshot.
        let retry = acc.completed(hour(10));
        assert_eq!(retry, pending, "failed flush retries the same counts");
        assert_eq!((retry[0].samples, retry[0].occupancy_sum), (1, 4));
        acc.acknowledge(&retry);
        assert!(acc.completed(hour(10)).is_empty());
    }

    #[tokio::test]
    async fn cancelled_flush_keeps_completed_hours_buffered() {
        let accumulator = Arc::new(Mutex::new(Accumulator::default()));
        accumulator.lock().expect("accumulator").record(hour(9), 4);
        let attempt = async {
            let buckets = accumulator.lock().expect("accumulator").completed(hour(10));
            std::future::pending::<()>().await;
            accumulator
                .lock()
                .expect("accumulator")
                .acknowledge(&buckets);
        };
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1), attempt)
                .await
                .is_err()
        );
        let retry = accumulator.lock().expect("accumulator").completed(hour(10));
        assert_eq!(
            retry,
            vec![HourBucket {
                hour_start: hour(9),
                samples: 1,
                occupancy_sum: 4,
                last_observed_at: Some(hour(9)),
            }]
        );
    }

    #[test]
    fn missing_hours_stay_gaps_across_the_week_boundary() {
        let mut acc = Accumulator::default();
        // Unix day 3 is Sunday; day 4 starts Monday.
        acc.record(hour(95) + time::Duration::minutes(55), 4);
        acc.record(hour(98), 0);
        let pending = acc.completed(hour(99));
        assert_eq!(pending.len(), 2, "no rows invented for missing hours");
        assert_eq!(pending[0].hour_start, hour(95));
        assert_eq!(pending[1].hour_start, hour(98));
        assert_eq!((pending[1].samples, pending[1].occupancy_sum), (1, 0));
    }

    #[test]
    fn retries_count_each_five_minute_slot_only_once() {
        let mut acc = Accumulator::default();
        let first = hour(10) + time::Duration::seconds(3);
        acc.record(first, 4);
        acc.record(first + time::Duration::minutes(4), 99);
        let next = hour(10) + time::Duration::minutes(5);
        acc.record(next, 0);
        acc.record(next + time::Duration::seconds(30), 99);
        let pending = acc.completed(hour(11));
        assert_eq!(pending[0].samples, 2);
        assert_eq!(pending[0].occupancy_sum, 4);
        assert_eq!(pending[0].last_observed_at, Some(next));
    }

    #[test]
    fn hour_has_twelve_slots_and_keeps_actual_latest_observation() {
        let mut acc = Accumulator::default();
        for minute in (0..60).rev() {
            acc.record(hour(10) + time::Duration::minutes(minute), 2);
        }
        let pending = acc.completed(hour(11));
        assert_eq!(pending[0].samples, 12);
        assert_eq!(pending[0].occupancy_sum, 24);
        assert_eq!(
            pending[0].last_observed_at,
            Some(hour(10) + time::Duration::minutes(59))
        );
        // The first observation in the next hour belongs to a new slot.
        acc.record(hour(11), 0);
        let pending = acc.completed(hour(12));
        assert_eq!(pending[1].samples, 1);
        assert_eq!(pending[1].last_observed_at, Some(hour(11)));
    }

    #[test]
    fn equal_counts_and_sums_preserve_different_observation_times() {
        let mut early = Accumulator::default();
        let mut late = Accumulator::default();
        early.record(hour(10), 0);
        late.record(hour(10) + time::Duration::minutes(4), 0);
        let early = early.completed(hour(11))[0];
        let late = late.completed(hour(11))[0];
        assert_eq!(
            (early.samples, early.occupancy_sum),
            (late.samples, late.occupancy_sum)
        );
        assert_ne!(early.last_observed_at, late.last_observed_at);
    }

    #[test]
    fn restart_leaves_unflushed_observations_as_gaps() {
        let mut before_restart = Accumulator::default();
        for minute in (0..60).step_by(5) {
            before_restart.record(hour(9) + time::Duration::minutes(minute), 4);
        }
        assert_eq!(before_restart.completed(hour(10))[0].samples, 12);
        drop(before_restart);
        let mut after_restart = Accumulator::default();
        assert!(after_restart.completed(hour(10)).is_empty());
        after_restart.record(hour(10), 0);
        let pending = after_restart.completed(hour(11));
        assert_eq!(pending.len(), 1, "no synthetic row for the lost hour");
        assert_eq!(pending[0].hour_start, hour(10));
        assert_eq!((pending[0].samples, pending[0].occupancy_sum), (1, 0));
    }

    #[test]
    fn negative_samples_clamp_and_buffer_stays_bounded() {
        let mut acc = Accumulator::default();
        acc.record(hour(3), -7);
        let drained = acc.completed(hour(4));
        assert_eq!((drained[0].samples, drained[0].occupancy_sum), (1, 0));
        for h in 0..(MAX_BUFFERED_HOURS as i64 + 5) {
            acc.record(hour(100 + h), 1);
        }
        assert_eq!(
            acc.len(),
            MAX_BUFFERED_HOURS,
            "buffer never grows past bound"
        );
    }
}
