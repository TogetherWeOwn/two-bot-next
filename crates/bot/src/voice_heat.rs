//! Dormant adapter from aggregate voice observations to hourly persistence.
//!
//! This module does not read the gateway, start a timer or publish data. Its
//! buffer is process-local: a restart leaves gaps rather than invented zeros.

use std::{ops::AsyncFnOnce, time::Duration};

use sqlx::PgPool;
use time::OffsetDateTime;
use two_bot_core::{
    metrics::JobErrorClass,
    voice_heat::{Accumulator, HourBucket},
    voice_heat_store::{flush_hour_buckets, VoiceHeatStoreError},
};

const FLUSH_BUDGET: Duration = Duration::from_secs(60);

/// Owns the hourly buffer for one sampler; mutable access serializes ticks.
#[derive(Default)]
pub struct VoiceHeat {
    accumulator: Accumulator,
}

impl VoiceHeat {
    /// Record an eligible scalar at `now` and attempt to flush completed hours.
    /// `None` is a gap, not a zero; it does not stop previously completed hours
    /// from flushing. The supplied pool is unused until there is a batch.
    ///
    /// # Errors
    /// Returns `Database` on a failed flush or `Timeout` if it runs over budget.
    /// Either way, the batch remains buffered for immutable replay.
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
        if let Some(total) = total {
            self.accumulator.record(now, total);
        }
        let buckets = self.accumulator.completed(now);
        if buckets.is_empty() {
            return Ok(());
        }
        tokio::time::timeout(FLUSH_BUDGET, flush(&buckets))
            .await
            .map_err(|_| JobErrorClass::Timeout)?
            .map_err(|_| JobErrorClass::Database)?;
        // A committed duplicate/expired-only batch may insert zero rows.
        self.accumulator.acknowledge(&buckets);
        Ok(())
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
        assert_eq!(
            adapter.tick(NEXT, None, &pool).await,
            Err(JobErrorClass::Database)
        );
        assert_eq!(adapter.accumulator.completed(NEXT), original);

        let started = tokio::time::Instant::now();
        assert_eq!(
            adapter
                .tick_with(NEXT, None, async |buckets| {
                    assert_eq!(buckets, original.as_slice());
                    std::future::pending().await
                })
                .await,
            Err(JobErrorClass::Timeout)
        );
        assert!((Duration::from_secs(60)..Duration::from_secs(61)).contains(&started.elapsed()));
        assert_eq!(adapter.accumulator.completed(NEXT), original);

        adapter
            .tick_with(NEXT, None, async |buckets| {
                assert_eq!(buckets, original.as_slice());
                Ok(0) // The timed-out attempt could have committed already.
            })
            .await
            .expect("successful replay acknowledges the original batch");
        assert!(adapter.accumulator.completed(LATER).is_empty());
    }
}
