//! Fresh, guild-attributed channel probes. No DELETE and no lease-age recovery.
use super::*;
use time::OffsetDateTime;
use two_bot_cutover::event_voice::{
    mark_admitted_event_voice_missing, restore_admitted_event_voice_channel,
    EventVoiceMissingBinding,
};
use two_bot_discord::voice_rooms::RoomHttpError;

#[derive(Default)]
pub(super) struct Reconciler {
    pauses: Mutex<HashMap<(u64, u64), tokio::time::Instant>>,
}

pub(super) struct Missing {
    pub event_id: String,
    pub instance: OffsetDateTime,
    pub channel_id: String,
}

#[derive(Default)]
pub(super) struct Report {
    pub missing: Vec<Missing>,
    pub perm_denied: u64,
    pub rate_limited: u64,
    pub unavailable: u64,
}

impl Reconciler {
    pub async fn run(
        &self,
        pool: &PgPool,
        rest: &ActionExecutor,
        guild: u64,
        shutdown: &watch::Receiver<bool>,
    ) -> Result<Report, ErrorClass> {
        if guild == 0 {
            return Err(ErrorClass::Configuration);
        }
        let mut report = Report::default();
        if publication_stopped(shutdown) {
            return Ok(report);
        }
        let bindings: Vec<(String, OffsetDateTime, String, Option<String>)> = sqlx::query_as(
            "SELECT e.event_id,e.instance_starts_at,e.voice_channel_id,e.owner_tick_lease
             FROM event_voice_channels e
             WHERE e.voice_channel_id IS NOT NULL
               AND (e.status='creating' OR (e.status='orphaned' AND e.owner_tick_lease IS NULL))
               AND (EXISTS (SELECT 1 FROM voice_create_reservations r
                            WHERE r.guild_id=$1 AND r.channel_id=e.voice_channel_id
                              AND (r.settled_at IS NULL OR e.status='orphaned'))
                    OR EXISTS (SELECT 1 FROM scheduled_events s
                               WHERE s.guild_id=$1 AND s.event_id=e.event_id))
               AND NOT EXISTS (SELECT 1 FROM voice_rooms v
                               WHERE v.guild_id=$1 AND v.channel_id=e.voice_channel_id)
             ORDER BY e.voice_channel_id",
        )
        .bind(guild.to_string())
        .fetch_all(pool)
        .await
        .map_err(|_| ErrorClass::Database)?;
        for (event_id, instance, channel_id, lease) in bindings {
            if publication_stopped(shutdown) {
                break;
            }
            let channel = channel_id.parse().map_err(|_| ErrorClass::Rest)?;
            let now = tokio::time::Instant::now();
            {
                let mut pauses = self.pauses.lock().await;
                pauses.retain(|_, until| *until > now);
                if pauses.contains_key(&(guild, channel)) {
                    report.rate_limited += 1;
                    continue;
                }
            }
            let response = tokio::select! {
                biased;
                _ = server::shutdown_requested(shutdown.clone()) => break,
                result = rest.read_event_voice_channel(guild, channel) => result,
            };
            if publication_stopped(shutdown) {
                break;
            }
            if let Err(RoomHttpError::RateLimited { retry_after_ms, .. }) = response {
                report.rate_limited += 1;
                let until = tokio::time::Instant::now()
                    .checked_add(Duration::from_millis(retry_after_ms))
                    .ok_or(ErrorClass::Rest)?;
                self.pauses.lock().await.insert((guild, channel), until);
                continue;
            }
            match &response {
                Err(RoomHttpError::AccessDenied | RoomHttpError::Unauthorized) => {
                    report.perm_denied += 1;
                }
                Err(RoomHttpError::NotFound) | Ok(_) => {}
                Err(_) => report.unavailable += 1,
            }
            let receipt = EventVoiceMissingBinding {
                guild_id: guild,
                event_id: &event_id,
                instance_starts_at: instance,
                channel_id: &channel_id,
                lease: lease.as_deref(),
            };
            if response.is_ok() {
                restore_admitted_event_voice_channel(pool, &receipt)
                    .await
                    .map_err(|_| ErrorClass::Database)?;
            }
            if matches!(response, Err(RoomHttpError::NotFound))
                && mark_admitted_event_voice_missing(pool, &receipt)
                    .await
                    .map_err(|_| ErrorClass::Database)?
            {
                report.missing.push(Missing {
                    event_id,
                    instance,
                    channel_id,
                });
            }
        }
        Ok(report)
    }
}
