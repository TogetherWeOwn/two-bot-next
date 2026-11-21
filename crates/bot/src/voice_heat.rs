//! Adapter from aggregate voice observations to hourly persistence.
//!
//! This module does not read the gateway, start a timer or publish data. Its
//! buffer is process-local: a restart leaves gaps rather than invented zeros.

use std::{ops::AsyncFnOnce, time::Duration};

use sqlx::PgPool;
use time::OffsetDateTime;
use two_bot_core::{
    metrics::JobErrorClass,
    voice_heat::{truncate_to_hour, Accumulator, HourBucket},
    voice_heat_store::{flush_hour_buckets, VoiceHeatStoreError},
};

const FLUSH_BUDGET: Duration = Duration::from_secs(60);

/// Owns the hourly buffer for one sampler; mutable access serializes ticks.
#[derive(Default)]
pub struct VoiceHeat {
    accumulator: Accumulator,
    latest_hour: Option<OffsetDateTime>,
}

impl VoiceHeat {
    /// Leave time for the inner flush to time out and record the observation.
    pub const TICK_TIMEOUT: Duration = Duration::from_secs(65);

    /// Attempt to flush completed hours, then record an eligible scalar at `now`.
    /// `None` is a gap, not a zero, and a clock rewind never reopens a closed hour.
    /// Neither stops completed hours from flushing. The pool is unused until
    /// there is a batch.
    ///
    /// # Errors
    /// Returns `Database` on a failed flush or `Timeout` if it runs over budget.
    /// Failed buckets remain buffered for immutable replay, subject to the
    /// accumulator's oldest-first eviction when a new hour fills the buffer.
    /// Cancellation before the attempt returns leaves the backlog untouched
    /// but skips the current observation.
    pub async fn tick(
        &mut self,
        now: OffsetDateTime,
        total: Option<i64>,
        pool: &PgPool,
    ) -> Result<(), JobErrorClass> {
        self.tick_with(now, total, async |buckets| {
            flush_hour_buckets(pool, buckets).await
        })
        .await
    }

    async fn tick_with(
        &mut self,
        now: OffsetDateTime,
        total: Option<i64>,
        flush: impl AsyncFnOnce(&[HourBucket]) -> Result<u64, VoiceHeatStoreError>,
    ) -> Result<(), JobErrorClass> {
        let sample_hour = truncate_to_hour(now);
        // Advance before awaiting: a cancelled write may already have committed.
        let current_hour = self
            .latest_hour
            .map_or(sample_hour, |hour| hour.max(sample_hour));
        self.latest_hour = Some(current_hour);

        let buckets = self.accumulator.completed(current_hour);
        let result = if buckets.is_empty() {
            Ok(())
        } else {
            match tokio::time::timeout(FLUSH_BUDGET, flush(&buckets)).await {
                Ok(Ok(_)) => {
                    // A committed duplicate/expired-only batch may insert zero rows.
                    self.accumulator.acknowledge(&buckets);
                    Ok(())
                }
                Ok(Err(_)) => Err(JobErrorClass::Database),
                Err(_) => Err(JobErrorClass::Timeout),
            }
        };
        // Give the oldest bucket a flush attempt before admission can evict it.
        if let Some(total) = total.filter(|_| sample_hour == current_hour) {
            self.accumulator.record(now, total);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use time::macros::datetime;

    const FIRST: OffsetDateTime = datetime!(2026-11-21 00:00 UTC);
    const NEXT: OffsetDateTime = datetime!(2026-11-21 01:00 UTC);
    const LATER: OffsetDateTime = datetime!(2026-11-21 02:00 UTC);

    #[tokio::test]
    async fn gap_tick_flushes_only_previously_observed_hours() {
        let mut adapter = VoiceHeat::default();
        let pool = PgPoolOptions::new().connect_lazy_with(PgConnectOptions::new_without_pgpass());
        pool.close().await;
        // No batch opens the store, even with a closed pool. A complete empty
        // observation is distinct from a gap and retains an actual watermark.
        assert_eq!(adapter.tick(FIRST, None, &pool).await, Ok(()));
        assert!(adapter.accumulator.completed(NEXT).is_empty());
        assert_eq!(adapter.tick(FIRST, Some(0), &pool).await, Ok(()));
        adapter
            .tick_with(NEXT, None, async |buckets| {
                assert_eq!(buckets.len(), 1);
                assert_eq!(buckets[0].hour_start, FIRST);
                assert_eq!(buckets[0].samples, 1);
                assert_eq!(buckets[0].occupancy_sum, 0);
                assert_eq!(buckets[0].last_observed_at, Some(FIRST));
                Ok(1)
            })
            .await
            .expect("gap still flushes completed hours");
        assert!(adapter.accumulator.completed(LATER).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_and_db_failure_keep_the_same_batch_until_commit() {
        let mut adapter = VoiceHeat::default();
        let pool = PgPoolOptions::new().connect_lazy_with(PgConnectOptions::new_without_pgpass());
        pool.close().await;
        assert_eq!(adapter.tick(FIRST, Some(5_000), &pool).await, Ok(()));
        let original = adapter.accumulator.completed(NEXT);
        assert_eq!(original.len(), 1);
        assert_eq!(original[0].occupancy_sum, 5_000);

        // The timeout must be the first tick at NEXT, so the rewind below tests
        // that this attempt closed FIRST even though the write did not return.
        let started = tokio::time::Instant::now();
        assert_eq!(
            tokio::time::timeout(
                VoiceHeat::TICK_TIMEOUT,
                adapter.tick_with(NEXT, Some(7), async |buckets| {
                    assert_eq!(buckets, original.as_slice());
                    std::future::pending().await
                }),
            )
            .await,
            Ok(Err(JobErrorClass::Timeout))
        );
        assert!((Duration::from_secs(60)..Duration::from_secs(61)).contains(&started.elapsed()));
        assert_eq!(adapter.accumulator.completed(NEXT), original);

        let rewind = NEXT - time::Duration::minutes(5);
        assert_eq!(
            adapter.tick(rewind, Some(8), &pool).await,
            Err(JobErrorClass::Database)
        );
        assert_eq!(adapter.accumulator.completed(NEXT), original);

        adapter
            .tick_with(NEXT, None, async |buckets| {
                assert_eq!(buckets, original.as_slice());
                Ok(0) // The timed-out attempt could have committed already.
            })
            .await
            .expect("successful replay acknowledges the original batch");
        assert_eq!(adapter.tick(rewind, Some(8), &pool).await, Ok(()));
        let retained = adapter.accumulator.completed(LATER);
        assert_eq!(retained.len(), 1);
        assert_eq!(
            (retained[0].hour_start, retained[0].occupancy_sum),
            (NEXT, 7)
        );
    }

    #[tokio::test]
    async fn full_backlog_is_offered_before_recording_the_new_hour() {
        const BUFFERED_HOURS: i64 = 14 * 24;
        let recovery = FIRST + time::Duration::hours(BUFFERED_HOURS);
        let next = recovery + time::Duration::hours(1);

        for recovered in [true, false] {
            let mut adapter = VoiceHeat::default();
            for h in 0..BUFFERED_HOURS {
                adapter
                    .accumulator
                    .record(FIRST + time::Duration::hours(h), 1);
            }
            let result = adapter
                .tick_with(recovery, Some(7), async |buckets| {
                    assert_eq!(buckets.len() as i64, BUFFERED_HOURS);
                    assert_eq!(buckets[0].hour_start, FIRST);
                    assert_eq!(
                        buckets.last().expect("last buffered hour").hour_start,
                        recovery - time::Duration::hours(1)
                    );
                    if recovered {
                        Ok(buckets.len() as u64)
                    } else {
                        Err(sqlx::Error::PoolClosed.into())
                    }
                })
                .await;

            let pending = adapter.accumulator.completed(next);
            if recovered {
                assert_eq!(result, Ok(()));
                assert_eq!(pending.len(), 1);
            } else {
                assert_eq!(result, Err(JobErrorClass::Database));
                assert_eq!(pending.len() as i64, BUFFERED_HOURS);
                assert_eq!(pending[0].hour_start, NEXT);
            }
            let new_hour = pending.last().expect("new observation was recorded");
            assert_eq!(new_hour.hour_start, recovery);
            assert_eq!((new_hour.samples, new_hour.occupancy_sum), (1, 7));
        }
    }
}
