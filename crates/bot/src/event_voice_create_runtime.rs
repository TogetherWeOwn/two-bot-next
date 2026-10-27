//! Durable event-create orchestration. Caller owns the whole-tick/preflight gate;
//! this adapter owns the claim, capacity reservation and receipt transaction.
//! Unknown POST outcomes retain BOTH fences, including across cancellation.

use super::{
    event_voice_ticker::{self, CreateResult, TickIo},
    *,
};
use time::OffsetDateTime;
use two_bot_core::voice_create_admission::CreateAdmissionConfig;
use two_bot_cutover::{
    event_voice::{
        bind_admitted_event_voice_channel, claim_current_event_voice_channel,
        clear_event_voice_lease, count_live_event_voice_channels_for_guild,
        EventVoiceCreateBinding,
    },
    voice_rooms::{CreateClaim, PgRoomStore},
};
use two_bot_discord::executor::EventVoiceResponse;

pub(super) struct Creator<'a> {
    pub pool: &'a PgPool,
    pub rest: &'a ActionExecutor,
    pub guild: &'a str,
    pub actor: u64,
    pub admission: CreateAdmissionConfig,
}

/// Production [`TickIo`](event_voice_ticker::TickIo): durable claims before
/// POST against the fresh source observation. Deletion fails closed until
/// the room worker's send-time live-empty guard is available to this adapter.
pub(super) struct DurableTickIo<'a> {
    pub pool: &'a PgPool,
    pub rest: &'a ActionExecutor,
    pub guild: &'a str,
    pub snapshot: &'a super::event_voice_source::Snapshot,
    pub actor: u64,
    pub admission: CreateAdmissionConfig,
}

impl TickIo for DurableTickIo<'_> {
    async fn create(&self, event: &Value, category: &str) -> Result<CreateResult, ErrorClass> {
        Creator {
            pool: self.pool,
            rest: self.rest,
            guild: self.guild,
            actor: self.actor,
            admission: self.admission,
        }
        .create_observed(self.snapshot, event, category)
        .await
    }

    async fn delete(&self, event: &Value, channel: &str) -> Result<EventVoiceResponse, ErrorClass> {
        // Persisted spans can lag gateway joins. Neither an empty SQL result
        // nor the source overlay is the worker's live guard, and the generic
        // executor can await admission before sending. Until the guarded
        // worker write is wired here, never issue DELETE or release capacity.
        tracing::info!(
            event_id = event_voice_ticker::text(event, "event_id"),
            channel_id = channel,
            "event voice far-future delete deferred: live worker guard unavailable"
        );
        Ok(EventVoiceResponse::Uncertain)
    }
}

fn id(raw: &str) -> Result<u64, ErrorClass> {
    raw.parse::<u64>()
        .ok()
        .filter(|n| *n != 0 && n.to_string() == raw)
        .ok_or(ErrorClass::Rest)
}

fn timestamp(ms: i64) -> Result<OffsetDateTime, ErrorClass> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .map_err(|_| ErrorClass::Rest)
}

fn in_window(starts: OffsetDateTime) -> bool {
    let now = OffsetDateTime::now_utc();
    starts > now && starts <= now + time::Duration::minutes(15)
}

impl Creator<'_> {
    /// The production path requires a fresh, published source observation.
    pub async fn create_observed(
        &self,
        snapshot: &super::event_voice_source::Snapshot,
        event: &Value,
        category: &str,
    ) -> Result<CreateResult, ErrorClass> {
        self.create_checked(event, category, Some(snapshot)).await
    }

    #[cfg(test)]
    pub async fn create(&self, event: &Value, category: &str) -> Result<CreateResult, ErrorClass> {
        self.create_checked(event, category, None).await
    }

    async fn source_eligible(
        &self,
        snapshot: Option<&super::event_voice_source::Snapshot>,
        event: &Value,
    ) -> Result<bool, ErrorClass> {
        match snapshot {
            Some(snapshot) => snapshot.eligible(self.pool, self.guild, event).await,
            #[cfg(test)]
            None => Ok(true),
            #[cfg(not(test))]
            None => Ok(false),
        }
    }

    async fn create_checked(
        &self,
        event: &Value,
        category: &str,
        snapshot: Option<&super::event_voice_source::Snapshot>,
    ) -> Result<CreateResult, ErrorClass> {
        let guild = id(self.guild)?;
        id(category)?;
        if self.actor == 0 {
            return Err(ErrorClass::Configuration);
        }
        let event_id = event_voice_ticker::text(event, "event_id");
        id(event_id)?;
        let name = event_voice_ticker::text(event, "name");
        if name.trim().is_empty()
            || name.chars().count() > two_bot_core::scheduled_events::MAX_EVENT_NAME_CHARS
        {
            return Err(ErrorClass::Rest);
        }
        let starts = timestamp(event_voice_ticker::starts(event))?;
        let instance = timestamp(event_voice_ticker::instance(event))?;
        let ends = match event.get("ends_at_ms") {
            None | Some(Value::Null) => None,
            Some(value) => Some(timestamp(value.as_i64().ok_or(ErrorClass::Rest)?)?),
        };
        // A scan may spend seconds in the guild's serial queue. Re-evaluate
        // against the real clock, not the tick's frozen observation time.
        if !in_window(starts)
            || !self.source_eligible(snapshot, event).await?
            || super::event_voice_source::opted_out(self.pool, self.guild, event_id).await?
        {
            return Ok(CreateResult::Ineligible);
        }
        // Durable per-guild live cap, independent of the current snapshot:
        // bindings absent from this tick's source still occupy a slot. The
        // caller's own event never counts against itself; a bound event
        // proceeds to the claim below and loses there. Checked before claiming
        // so an over-cap candidate leaves no row for the next tick to clean.
        let live = count_live_event_voice_channels_for_guild(self.pool, self.guild, event_id)
            .await
            .map_err(|_| ErrorClass::Database)?;
        if live >= i64::try_from(event_voice_ticker::LIVE_CAP).unwrap_or(i64::MAX) {
            return Ok(CreateResult::AdmissionDenied);
        }
        let lease = hex::encode(rand::random::<[u8; 16]>());
        let Some(instance) =
            claim_current_event_voice_channel(self.pool, event_id, instance, starts, ends, &lease)
                .await
                .map_err(|_| ErrorClass::Database)?
        else {
            return Ok(CreateResult::ClaimLost);
        };
        let store = PgRoomStore::new(self.pool.clone());
        let reservation = match store
            .claim_event_create(
                guild,
                self.actor,
                &self.admission,
                OffsetDateTime::now_utc().unix_timestamp(),
            )
            .await
            .map_err(|_| ErrorClass::Database)?
        {
            CreateClaim::Admitted { reservation_id } => reservation_id,
            CreateClaim::Refused(_) => {
                self.release_no_effect(event_id, instance, &lease, None)
                    .await?;
                return Ok(CreateResult::AdmissionDenied);
            }
        };
        let opted_out = super::event_voice_source::opted_out(self.pool, self.guild, event_id).await;
        let source_eligible = self.source_eligible(snapshot, event).await;
        if !in_window(starts)
            || !matches!(opted_out, Ok(false))
            || !matches!(source_eligible, Ok(true))
        {
            self.release_no_effect(event_id, instance, &lease, Some(&reservation))
                .await?;
            opted_out?;
            source_eligible?;
            return Ok(CreateResult::Ineligible);
        }
        // Never retry or clear on a transport error, malformed receipt, 5xx,
        // or cancellation: an unobserved channel could already exist.
        let response = self
            .rest
            .create_event_voice_channel(self.guild, category, name)
            .await
            .unwrap_or(EventVoiceResponse::Uncertain);
        match &response {
            EventVoiceResponse::Created(channel) => {
                // Preserve a known receipt in bounded audit evidence even when
                // persistence fails; never log a remote body or database error.
                tracing::info!(
                    event_id,
                    instance_start = %instance,
                    channel_id = channel,
                    "event_voice_create_receipt"
                );
                let bound = bind_admitted_event_voice_channel(
                    self.pool,
                    &EventVoiceCreateBinding {
                        guild_id: guild,
                        event_id,
                        instance_starts_at: instance,
                        lease: &lease,
                        reservation_id: &reservation,
                        channel_id: channel,
                    },
                )
                .await
                .map_err(|_| ErrorClass::Database)?;
                if !bound {
                    return Err(ErrorClass::Database);
                }
            }
            EventVoiceResponse::Refused(_) | EventVoiceResponse::RateLimited { .. } => {
                self.release_no_effect(event_id, instance, &lease, Some(&reservation))
                    .await?;
            }
            EventVoiceResponse::Uncertain | EventVoiceResponse::Deleted => {}
        }
        Ok(CreateResult::Response(response))
    }

    async fn release_no_effect(
        &self,
        event: &str,
        instance: OffsetDateTime,
        lease: &str,
        reservation: Option<&str>,
    ) -> Result<(), ErrorClass> {
        if let Some(reservation) = reservation {
            let released = PgRoomStore::new(self.pool.clone())
                .settle_create(reservation)
                .await
                .map_err(|_| ErrorClass::Database)?;
            if !released {
                return Err(ErrorClass::Database);
            }
        }
        // Capacity first, then the retry fence. A failed settlement can never
        // expose the event to another POST with the old reservation outstanding.
        let cleared = clear_event_voice_lease(self.pool, event, instance, lease)
            .await
            .map_err(|_| ErrorClass::Database)?;
        if !cleared {
            return Err(ErrorClass::Database);
        }
        Ok(())
    }
}
