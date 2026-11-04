//! Five-minute voice-heat sampler (contract v2 B5).
//!
//! Records the guild-wide `total_in_voice` (AFK- and bot-excluded, the
//! `voice.snapshot` definition) from the shared gateway cache and flushes
//! completed UTC hours to `voice_heat_hours`. The sampler owns an in-memory
//! hourly [`Accumulator`]; the durable half lives in
//! [`two_bot_core::voice_heat_store`]. Member-level facts never leave this
//! module: only the guild aggregate crosses into storage, so the heat grid is
//! aggregate-legal for the `web_v1` publication.
//!
//! Skip semantics: a disconnected gateway, an incomplete cache or an
//! over-budget projection records nothing — gaps are gaps, never zeros. Only
//! completed hours flush (`ON CONFLICT DO NOTHING` claims each hour, so a
//! retried flush writes nothing). Completed buckets stay buffered until a
//! successful flush, including across connection failures and cancelled ticks.
//! A transient database failure returns [`ErrorClass::Database`] for a bounded
//! retry.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::sync::RwLock;
use twilight_cache_inmemory::DefaultInMemoryCache;
use two_bot_core::voice_heat_store::{self, HourBucket};
use two_bot_core::Snowflake;

use crate::{
    gateway::GatewayState,
    jobs::{self, ErrorClass, Job},
    voice_snapshot,
    website_jobs::Context,
};

/// Supervised job name, also the `/readyz` status entry.
pub const NAME: &str = "voice_heat";

/// Fixed-phase five-minute cadence inside the 60 s..=1 h contract.
const CADENCE: Duration = Duration::from_secs(300);
/// Per-attempt budget: one minute, positive and no greater than cadence.
const TIMEOUT: Duration = Duration::from_secs(60);

/// In-memory hourly buckets, keyed by UTC hour start. At most 14 days of
/// hourly buckets (336) survive a long database outage; beyond that the
/// oldest hour drops with a warn so memory stays bounded while the database
/// is unreachable.
const MAX_BUFFERED_HOURS: usize = 336;

/// Hourly accumulation of five-minute `total_in_voice` samples. Pure over the
/// injected clock except for the eviction warn, so bucketing pins without a
/// gateway cache or a database.
#[derive(Debug, Default)]
pub(crate) struct Accumulator {
    buckets: BTreeMap<time::OffsetDateTime, (i64, i64)>,
}

impl Accumulator {
    /// Record one sample at `sample_at`. Negative totals clamp to zero; a
    /// full buffer evicts the oldest hour with a warn, never an unbounded
    /// grow.
    pub(crate) fn record(&mut self, sample_at: time::OffsetDateTime, total: i64) {
        let hour = voice_heat_store::truncate_to_hour(sample_at);
        let entry = self.buckets.entry(hour).or_insert((0, 0));
        entry.0 = entry.0.saturating_add(1);
        entry.1 = entry.1.saturating_add(total.max(0));
        if self.buckets.len() > MAX_BUFFERED_HOURS {
            let oldest = *self.buckets.keys().next().expect("non-empty buffer");
            self.buckets.remove(&oldest);
            tracing::warn!(
                job = NAME,
                "voice-heat buffer full; dropping oldest hour, retryable"
            );
        }
    }

    /// Snapshot every hour strictly before the current UTC hour. Keep the
    /// rows buffered across every await: the supervisor may cancel a tick.
    pub(crate) fn completed(&self, now: time::OffsetDateTime) -> Vec<HourBucket> {
        let current = voice_heat_store::truncate_to_hour(now);
        self.buckets
            .range(..current)
            .map(|(&hour_start, &(samples, occupancy_sum))| HourBucket {
                hour_start,
                samples,
                occupancy_sum,
            })
            .collect()
    }

    /// Forget only successfully flushed buckets. A partial write is safe to
    /// retry because the store claims each hour with ON CONFLICT DO NOTHING.
    pub(crate) fn acknowledge(&mut self, buckets: &[HourBucket]) {
        for bucket in buckets {
            self.buckets.remove(&bucket.hour_start);
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.buckets.len()
    }
}

/// The supervised voice-heat job: fixed-phase [`CADENCE`], jittered start,
/// per-attempt [`TIMEOUT`], panic-isolated by the supervisor like every job.
/// Holds the gateway state to gate on `Connected`, the shared voice cache to
/// read from, and the accumulator across ticks.
#[must_use]
pub fn job(
    context: Arc<Context>,
    cache: Arc<DefaultInMemoryCache>,
    gateway: Arc<RwLock<GatewayState>>,
) -> Job {
    let accumulator = Arc::new(Mutex::new(Accumulator::default()));
    Job {
        name: NAME,
        cadence: CADENCE,
        cadence_source: None,
        startup_jitter: jobs::startup_jitter(CADENCE, rand::random()),
        timeout: TIMEOUT,
        action: Arc::new(move || {
            let context = context.clone();
            let cache = cache.clone();
            let gateway = gateway.clone();
            let accumulator = accumulator.clone();
            Box::pin(async move { tick(&context, &cache, &gateway, &accumulator).await })
        }),
    }
}

/// One sampler tick: gate on the live gateway, read the snapshot total,
/// record the sample, and flush completed hours. Skips record nothing;
/// database failures keep buffered buckets and read as [`ErrorClass::Database`].
async fn tick(
    context: &Arc<Context>,
    cache: &Arc<DefaultInMemoryCache>,
    gateway: &Arc<RwLock<GatewayState>>,
    accumulator: &Arc<Mutex<Accumulator>>,
) -> Result<(), ErrorClass> {
    if !matches!(*gateway.read().await, GatewayState::Connected) {
        return Ok(());
    }
    let guild: Snowflake = context
        .guild
        .parse()
        .map_err(|_| ErrorClass::Configuration)?;
    let total = match voice_snapshot::read_voice_snapshot(cache, guild) {
        Ok(value) => value
            .get("total_in_voice")
            .and_then(serde_json::Value::as_u64)
            .and_then(|total| i64::try_from(total).ok())
            .ok_or(ErrorClass::Rest)?,
        Err(
            voice_snapshot::SnapshotRefusal::CacheIncomplete
            | voice_snapshot::SnapshotRefusal::OverBudget,
        ) => return Ok(()),
    };
    let now = time::OffsetDateTime::now_utc();
    let buckets = {
        let mut guard = accumulator.lock().map_err(|_| ErrorClass::Database)?;
        guard.record(now, total);
        guard.completed(now)
    };
    if buckets.is_empty() {
        return Ok(());
    }
    let pool = context.pool().await?.clone();
    if let Err(err) = voice_heat_store::flush_hour_buckets(&pool, &buckets).await {
        let reason = match &err {
            voice_heat_store::VoiceHeatStoreError::Db(inner) => crate::db_log::db_class(inner),
        };
        tracing::warn!(
            job = NAME,
            reason = reason,
            "voice-heat flush failed, retryable"
        );
        return Err(ErrorClass::Database);
    }
    accumulator
        .lock()
        .map_err(|_| ErrorClass::Database)?
        .acknowledge(&buckets);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(tokio::time::timeout(Duration::from_millis(1), attempt)
            .await
            .is_err());
        let retry = accumulator.lock().expect("accumulator").completed(hour(10));
        assert_eq!(
            retry,
            vec![HourBucket {
                hour_start: hour(9),
                samples: 1,
                occupancy_sum: 4
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
