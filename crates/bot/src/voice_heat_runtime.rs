//! Supervise aggregate-only five-minute sampling from the live fenced cache.

use std::{sync::Arc, time::Duration};

use sqlx::PgPool;
use time::OffsetDateTime;
use tokio::sync::{Mutex, RwLock};
use two_bot::voice_heat::VoiceHeat;
use two_bot_core::Snowflake;
use two_bot_discord::SharedCache;

use crate::{
    gateway::GatewayState,
    jobs::{self, ErrorClass, Job},
    voice_snapshot,
};

pub(crate) const NAME: &str = "voice_heat";
const CADENCE: Duration = Duration::from_secs(300);
const TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) fn job(
    pool: PgPool,
    guild: Snowflake,
    cache: Arc<SharedCache>,
    gateway: Arc<RwLock<GatewayState>>,
) -> Job {
    let buffer = Arc::new(Mutex::new(VoiceHeat::default()));
    Job {
        name: NAME,
        cadence: CADENCE,
        cadence_source: None,
        startup_jitter: jobs::startup_jitter(CADENCE, rand::random()),
        timeout: TIMEOUT,
        action: Arc::new(move || {
            let (pool, cache, gateway, buffer) =
                (pool.clone(), cache.clone(), gateway.clone(), buffer.clone());
            Box::pin(async move {
                let mut buffer = buffer.lock().await;
                tick(
                    &mut buffer,
                    &pool,
                    &cache,
                    &gateway,
                    guild,
                    OffsetDateTime::now_utc(),
                )
                .await
            })
        }),
    }
}

async fn tick(
    buffer: &mut VoiceHeat,
    pool: &PgPool,
    cache: &SharedCache,
    gateway: &RwLock<GatewayState>,
    guild: Snowflake,
    now: OffsetDateTime,
) -> Result<(), ErrorClass> {
    // Hold the gateway state stable through the fenced cache read. Neither
    // fence waits: a transition or rebuild is a gap, and never holds up a flush.
    let total = gateway.try_read().ok().and_then(|state| {
        if *state != GatewayState::Connected {
            return None;
        }
        let total = voice_snapshot::read_voice_total(cache, guild).ok()?;
        i64::try_from(total).ok()
    });
    buffer.tick(now, total, pool).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use time::{macros::datetime, Duration as WallDuration};
    use twilight_model::gateway::{event::Event, payload::incoming::GuildCreate};

    #[tokio::test]
    async fn only_a_connected_complete_snapshot_adds_an_observation() {
        let cache = voice_snapshot::new_shared_cache();
        let gateway = Arc::new(RwLock::new(GatewayState::Connected));
        let pool = PgPoolOptions::new().connect_lazy_with(PgConnectOptions::new_without_pgpass());
        pool.close().await;
        let job = job(pool.clone(), 100, cache.clone(), gateway.clone());
        assert_eq!(job.cadence, CADENCE);
        assert_eq!(job.timeout, TIMEOUT);
        assert_eq!((job.action)().await, Ok(()));

        let mut buffer = VoiceHeat::default();
        let first = datetime!(2026-11-21 00:00 UTC);
        assert_eq!(
            tick(&mut buffer, &pool, &cache, &gateway, 100, first).await,
            Ok(())
        );

        cache.update(&Event::GuildCreate(Box::new(GuildCreate::Available(
            serde_json::from_str(include_str!("../tests/fixtures/voice_guild.json"))
                .expect("complete, empty guild fixture"),
        ))));
        *gateway.write().await = GatewayState::Armed;
        let next = first + WallDuration::hours(1);
        assert_eq!(
            tick(&mut buffer, &pool, &cache, &gateway, 100, next).await,
            Ok(())
        );
        let third = next + WallDuration::hours(1);
        // The previous cold and disconnected ticks did not buffer a zero.
        *gateway.write().await = GatewayState::Connected;
        assert_eq!(
            tick(&mut buffer, &pool, &cache, &gateway, 100, third).await,
            Ok(())
        );
        // A complete empty cache contributes a true zero. An ensuing gap
        // still tries to flush that completed hour, exposing the closed pool.
        *gateway.write().await = GatewayState::Draining;
        assert_eq!(
            tick(
                &mut buffer,
                &pool,
                &cache,
                &gateway,
                100,
                third + WallDuration::hours(1)
            )
            .await,
            Err(ErrorClass::Database)
        );
    }
}
