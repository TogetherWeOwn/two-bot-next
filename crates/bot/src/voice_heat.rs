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
//! Skip semantics: a disconnected gateway or incomplete cache records
//! nothing — gaps are gaps, never zeros. The HTTP listing budget does not
//! constrain scalar observations. Skips still flush completed buckets. Only
//! completed hours flush (`ON CONFLICT DO NOTHING` claims each hour, so a
//! retried flush writes nothing). Completed buckets stay buffered until a
//! successful flush, including across connection failures and cancelled ticks.
//! A transient database failure returns [`ErrorClass::Database`] for a bounded
//! retry. The contract's hourly buffer is deliberately process-local: restart
//! loses unflushed observations, including the open hour. Those remain coverage
//! gaps (not invented zeros); a lost hour can make the shared grid insufficient
//! until it leaves the 28-day window. This is not a durable sampling journal.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::sync::{OnceCell, RwLock};
use two_bot_core::voice_heat_store::{self, HourBucket};
use two_bot_core::Snowflake;

use crate::{
    gateway::GatewayState,
    jobs::{self, ErrorClass, Job},
    voice_snapshot,
};

/// No REST dependency: registration needs only gateway prerequisites, and the
/// DML-only pool is opened lazily when a completed bucket is ready to flush.
struct Context {
    url: String,
    pool: OnceCell<sqlx::PgPool>,
    guild: Snowflake,
}

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
    /// oldest hour with a warn, never an unbounded grow.
    pub(crate) fn record(&mut self, sample_at: time::OffsetDateTime, total: i64) {
        let hour = voice_heat_store::truncate_to_hour(sample_at);
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
            .map(|(&hour_start, entry)| HourBucket {
                hour_start,
                samples: i64::from(entry.slots.count_ones()),
                occupancy_sum: entry.occupancy_sum,
                last_observed_at: entry.last_observed_at,
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
    database_url: &str,
    guild: Snowflake,
    cache: Arc<two_bot_discord::SharedCache>,
    gateway: Arc<RwLock<GatewayState>>,
) -> Job {
    let context = Arc::new(Context {
        url: database_url.to_owned(),
        pool: OnceCell::new(),
        guild,
    });
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
    cache: &Arc<two_bot_discord::SharedCache>,
    gateway: &Arc<RwLock<GatewayState>>,
    accumulator: &Arc<Mutex<Accumulator>>,
) -> Result<(), ErrorClass> {
    let total = if matches!(*gateway.read().await, GatewayState::Connected) {
        voice_snapshot::read_voice_total(cache, context.guild).ok()
    } else {
        None
    };
    let now = time::OffsetDateTime::now_utc();
    let buckets = {
        let mut guard = accumulator.lock().map_err(|_| ErrorClass::Database)?;
        if let Some(total) = total {
            guard.record(now, i64::try_from(total).map_err(|_| ErrorClass::Rest)?);
        }
        // Eligibility gates only this observation. Previously valid completed
        // hours must persist even while the gateway/cache is unavailable.
        guard.completed(now)
    };
    if buckets.is_empty() {
        return Ok(());
    }
    let pool = context
        .pool
        .get_or_try_init(|| async {
            let db = two_bot_cutover::connect(
                &context.url,
                two_bot_cutover::DB_POOL_MAX_DEFAULT,
                true, // Runtime is DML-only; migrations/views are provisioned first.
            )
            .await
            .map_err(|_| ErrorClass::Database)?;
            Ok::<_, ErrorClass>(db.pool().clone())
        })
        .await?;
    if let Err(err) = voice_heat_store::flush_hour_buckets(pool, &buckets).await {
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

    #[tokio::test]
    async fn cache_only_ticks_preserve_gaps_without_opening_a_pool() {
        let context = Arc::new(Context {
            url: "not a database URL".to_owned(),
            pool: OnceCell::new(),
            guild: 2222,
        });
        let cache = voice_snapshot::new_shared_cache();
        let gateway = Arc::new(RwLock::new(GatewayState::Armed));
        let accumulator = Arc::new(Mutex::new(Accumulator::default()));
        tick(&context, &cache, &gateway, &accumulator)
            .await
            .unwrap();
        *gateway.write().await = GatewayState::Connected;
        tick(&context, &cache, &gateway, &accumulator)
            .await
            .unwrap();
        assert!(context.pool.get().is_none(), "skips never open the DB");
        assert_eq!(accumulator.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn rebuilding_and_unavailable_guilds_are_gaps_but_complete_empty_is_zero() {
        use serde_json::json;
        use twilight_model::gateway::{event::Event, payload::incoming::GuildCreate};
        let context = Arc::new(Context {
            url: "not a database URL".to_owned(),
            pool: OnceCell::new(),
            guild: 100,
        });
        let cache = voice_snapshot::new_shared_cache();
        let full = || {
            Event::GuildCreate(Box::new(GuildCreate::Available(
                serde_json::from_str(include_str!("../tests/fixtures/voice_guild.json")).unwrap(),
            )))
        };
        cache.update(&full());
        cache.update(&Event::Ready(serde_json::from_value(json!({
            "v": 10, "session_id": "fixture", "resume_gateway_url": "ws://fixture.invalid",
            "application": {"id": "999", "flags": 0},
            "user": {"id": "999", "username": "fixture", "discriminator": "0", "mfa_enabled": false},
            "guilds": [{"id": "100", "unavailable": true}]
        })).unwrap()));
        let gateway = Arc::new(RwLock::new(GatewayState::Connected));
        let accumulator = Arc::new(Mutex::new(Accumulator::default()));
        tick(&context, &cache, &gateway, &accumulator)
            .await
            .unwrap();
        assert_eq!(
            accumulator.lock().unwrap().len(),
            0,
            "READY is not a full guild snapshot"
        );
        cache.update(&Event::GuildCreate(Box::new(GuildCreate::Unavailable(
            serde_json::from_value(json!({"id": "100", "unavailable": true})).unwrap(),
        ))));
        tick(&context, &cache, &gateway, &accumulator)
            .await
            .unwrap();
        assert_eq!(
            accumulator.lock().unwrap().len(),
            0,
            "unavailable is not zero"
        );
        cache.update(&full());
        tick(&context, &cache, &gateway, &accumulator)
            .await
            .unwrap();
        let buckets = accumulator
            .lock()
            .unwrap()
            .completed(time::OffsetDateTime::now_utc() + time::Duration::hours(1));
        assert_eq!(buckets.len(), 1);
        assert_eq!((buckets[0].samples, buckets[0].occupancy_sum), (1, 0));
        assert!(context.pool.get().is_none());
    }

    #[tokio::test]
    async fn skipped_observations_still_persist_completed_hours() {
        let url = match std::env::var("TWO_TEST_DATABASE_URL") {
            Err(std::env::VarError::NotPresent) => return,
            result => result.expect("valid UTF-8 test bootstrap configuration"),
        };
        let fixture = two_bot_testsupport::TestDatabase::create(
            &url,
            &sqlx::migrate!("../cutover/migrations"),
        )
        .await
        .expect("disposable database");
        let context = Arc::new(Context {
            url: "unused: pool already initialized".to_owned(),
            pool: OnceCell::from(fixture.pool().clone()),
            guild: 2222,
        });
        let cache = voice_snapshot::new_shared_cache();
        let gateway = Arc::new(RwLock::new(GatewayState::Armed));
        let accumulator = Arc::new(Mutex::new(Accumulator::default()));
        let now = time::OffsetDateTime::now_utc();
        let start = voice_heat_store::truncate_to_hour(now) - time::Duration::hours(2);
        for (offset, state) in [(0, GatewayState::Armed), (1, GatewayState::Connected)] {
            *gateway.write().await = state;
            accumulator
                .lock()
                .unwrap()
                .record(start + time::Duration::hours(offset), 4);
            tick(&context, &cache, &gateway, &accumulator)
                .await
                .unwrap();
            assert_eq!(
                accumulator.lock().unwrap().len(),
                0,
                "skips flush completed samples without inventing a new observation"
            );
        }
        let rows: Vec<(time::OffsetDateTime, i32, i64)> = sqlx::query_as(
            "SELECT hour_start_utc, samples, occupancy_sum FROM voice_heat_hours ORDER BY hour_start_utc",
        ).fetch_all(fixture.pool()).await.unwrap();
        assert_eq!(
            rows,
            vec![(start, 1, 4), (start + time::Duration::hours(1), 1, 4)]
        );
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn over_budget_snapshot_still_records_the_eligible_scalar() {
        use serde_json::json;
        use twilight_model::gateway::{event::Event, payload::incoming::GuildCreate};
        let mut guild: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/voice_guild.json")).unwrap();
        guild["id"] = json!("2222");
        guild["afk_channel_id"] = json!("34");
        guild["channels"] = json!([31, 34, 35].map(|id| json!({
            "id": id.to_string(), "guild_id": "2222", "type": if id == 35 { 13 } else { 2 },
            "name": "voice", "permission_overwrites": []
        })));
        guild["members"] = json!([{
            "user": {"id": "43", "username": "fixture", "discriminator": "0", "bot": true},
            "roles": [], "deaf": false, "mute": false,
            "joined_at": "2026-09-30T00:00:00.000000+00:00", "flags": 0
        }]);
        // Unknown identities remain humans, plus one bot, one AFK occupant,
        // and one stage speaker that must not enter the aggregate.
        let mut states: Vec<_> = (1..=5000)
            .map(|n| (100_000_000_000_000_000u64 + n, 31))
            .collect();
        states.extend([(43, 31), (44, 34), (45, 35)]);
        guild["voice_states"] =
            json!(states.into_iter().map(|(user, channel)| json!({
            "guild_id": "2222", "channel_id": channel.to_string(), "user_id": user.to_string(),
            "session_id": "fixture", "deaf": false, "mute": false,
            "self_deaf": false, "self_mute": false, "self_video": false, "suppress": false
        })).collect::<Vec<_>>());
        let cache = voice_snapshot::new_shared_cache();
        cache.update(&Event::GuildCreate(Box::new(GuildCreate::Available(
            serde_json::from_value(guild).unwrap(),
        ))));
        assert_eq!(
            voice_snapshot::read_voice_snapshot(&cache, 2222),
            Err(voice_snapshot::SnapshotRefusal::OverBudget),
            "HTTP limit remains enforced"
        );
        let context = Arc::new(Context {
            url: "not a database URL".to_owned(),
            pool: OnceCell::new(),
            guild: 2222,
        });
        let gateway = Arc::new(RwLock::new(GatewayState::Connected));
        let accumulator = Arc::new(Mutex::new(Accumulator::default()));
        tick(&context, &cache, &gateway, &accumulator)
            .await
            .unwrap();
        let pending = accumulator
            .lock()
            .unwrap()
            .completed(time::OffsetDateTime::now_utc() + time::Duration::hours(1));
        assert_eq!(pending.len(), 1);
        assert_eq!((pending[0].samples, pending[0].occupancy_sum), (1, 5000));
        assert!(context.pool.get().is_none(), "open hour needs no database");
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
