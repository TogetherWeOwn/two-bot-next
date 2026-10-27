//! Single-attempt event voice requests. The ticker, not the HTTP client, owns
//! retry scheduling. Every request still uses governed send admission.

use super::*;
use twilight_model::channel::ChannelType;

/// A confirmed receipt or a fixed-vocabulary failure; never a remote body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventVoiceResponse {
    Created(String),
    Deleted,
    RateLimited { retry_after_ms: u64 },
    Refused(u16),
    Uncertain,
}

impl ActionExecutor {
    /// Fresh event-source read for the minute ticker, not the ten-minute mirror.
    /// One governed attempt; the caller owns route backoff. Count raw bytes so
    /// unknown fields and whitespace cannot evade the 1 MiB snapshot bound.
    pub async fn read_event_voice_events(
        &self,
        guild: u64,
    ) -> Result<serde_json::Value, crate::voice_rooms::RoomHttpError> {
        use crate::voice_rooms::RoomHttpError;
        if guild == 0 {
            return Err(RoomHttpError::InvalidRequest);
        }
        let request = Request::from_route(&Route::GetGuildScheduledEvents {
            guild_id: guild,
            with_user_count: false,
        });
        let (response, _) = self
            .send_with_timeout(&request, Some(false))
            .await
            .map_err(|_| RoomHttpError::UnknownOutcome)?;
        match response.status {
            200 if response.body.len() <= 1_048_576 => {
                serde_json::from_slice(&response.body).map_err(|_| RoomHttpError::UnknownOutcome)
            }
            401 => Err(RoomHttpError::Unauthorized),
            403 => Err(RoomHttpError::AccessDenied),
            404 => Err(RoomHttpError::NotFound),
            429 => {
                let EventVoiceResponse::RateLimited { retry_after_ms } =
                    event_voice_failure(&response)
                else {
                    return Err(RoomHttpError::UnknownOutcome);
                };
                let global = serde_json::from_slice::<serde_json::Value>(&response.body)
                    .ok()
                    .and_then(|body| body.get("global").and_then(serde_json::Value::as_bool))
                    .unwrap_or(false);
                Err(RoomHttpError::RateLimited {
                    retry_after_ms,
                    global,
                })
            }
            _ => Err(RoomHttpError::UnknownOutcome),
        }
    }

    /// One live channel read for category preflight and binding reconciliation.
    /// Only a matching guild/channel document is evidence; 404 is distinct from
    /// an unavailable or permission-denied read. The ticker owns all retries.
    pub async fn read_event_voice_channel(
        &self,
        guild: u64,
        channel: u64,
    ) -> Result<twilight_model::channel::Channel, crate::voice_rooms::RoomHttpError> {
        use crate::voice_rooms::RoomHttpError;
        let channel_id = Id::<ChannelMarker>::new_checked(channel)
            .filter(|_| guild != 0)
            .ok_or(RoomHttpError::InvalidRequest)?;
        let request = Self::request_of(self.inner.factory.channel(channel_id))
            .map_err(|_| RoomHttpError::InvalidRequest)?;
        let (response, _) = self
            .send_with_timeout(&request, Some(false))
            .await
            .map_err(|_| RoomHttpError::UnknownOutcome)?;
        match response.status {
            200 => {
                let document: twilight_model::channel::Channel =
                    serde_json::from_slice(&response.body)
                        .map_err(|_| RoomHttpError::UnknownOutcome)?;
                if document.id != channel_id || document.guild_id.map(Id::get) != Some(guild) {
                    return Err(RoomHttpError::UnknownOutcome);
                }
                Ok(document)
            }
            401 => Err(RoomHttpError::Unauthorized),
            403 => Err(RoomHttpError::AccessDenied),
            404 => Err(RoomHttpError::NotFound),
            429 => {
                let EventVoiceResponse::RateLimited { retry_after_ms } =
                    event_voice_failure(&response)
                else {
                    return Err(RoomHttpError::UnknownOutcome);
                };
                let global = serde_json::from_slice::<serde_json::Value>(&response.body)
                    .ok()
                    .and_then(|body| body.get("global").and_then(serde_json::Value::as_bool))
                    .unwrap_or(false);
                Err(RoomHttpError::RateLimited {
                    retry_after_ms,
                    global,
                })
            }
            _ => Err(RoomHttpError::UnknownOutcome),
        }
    }

    /// One create with no automatic retry, including on 429 and 5xx. A valid
    /// receipt is required before releasing the consuming admission permit.
    pub async fn create_event_voice_channel(
        &self,
        guild: &str,
        category: &str,
        name: &str,
    ) -> Result<EventVoiceResponse, DiscordError> {
        let guild_id: Id<GuildMarker> = snowflake(guild)?;
        let category_id: Id<ChannelMarker> = snowflake(category)?;
        let request = Self::request_of(
            self.inner
                .factory
                .create_guild_channel(guild_id, name)
                .kind(ChannelType::GuildVoice)
                .parent_id(category_id),
        )?;
        let (mut response, _) = self.send_with_timeout(&request, Some(false)).await?;
        if response.status == 201 || response.status == 200 {
            let document: serde_json::Value = serde_json::from_slice(&response.body)
                .map_err(|_| DiscordError::Unavailable("invalid event voice receipt".into()))?;
            let id = document
                .get("id")
                .and_then(serde_json::Value::as_str)
                .filter(|id| canonical_channel_id(id));
            if document.get("guild_id").and_then(serde_json::Value::as_str) != Some(guild)
                || document
                    .get("parent_id")
                    .and_then(serde_json::Value::as_str)
                    != Some(category)
                || document.get("type").and_then(serde_json::Value::as_u64) != Some(2)
                || id.is_none()
            {
                return Ok(EventVoiceResponse::Uncertain);
            }
            let id = id
                .ok_or_else(|| DiscordError::Unavailable("invalid event voice receipt".into()))?
                .to_owned();
            response.complete_confirmed().await?;
            return Ok(EventVoiceResponse::Created(id));
        }
        Ok(event_voice_failure(&response))
    }

    /// One delete. The caller must hold the room worker's live empty guard;
    /// a stale snapshot is never authority to call this method.
    pub async fn delete_event_voice_channel(
        &self,
        channel: &str,
    ) -> Result<EventVoiceResponse, DiscordError> {
        let channel: Id<ChannelMarker> = snowflake(channel)?;
        let request = Self::request_of(self.inner.factory.delete_channel(channel))?;
        let (mut response, _) = self.send_with_timeout(&request, Some(false)).await?;
        if response.status == 204 || response.status == 404 {
            response.complete_confirmed().await?;
            return Ok(EventVoiceResponse::Deleted);
        }
        Ok(event_voice_failure(&response))
    }
}

fn canonical_channel_id(raw: &str) -> bool {
    raw.parse::<u64>()
        .is_ok_and(|id| id != 0 && id.to_string() == raw)
}

fn event_voice_failure(response: &RawResponse) -> EventVoiceResponse {
    if response.status == 429 {
        // Never shorten Discord's delay to the legacy paced lane's 60s cap.
        let header = response
            .retry_after_header
            .as_deref()
            .and_then(|s| s.parse::<f64>().ok());
        let delay = header
            .into_iter()
            .chain(response.body_retry_after_secs())
            .filter(|s| s.is_finite() && *s >= 0.0)
            .fold(1.0_f64, f64::max);
        EventVoiceResponse::RateLimited {
            retry_after_ms: (delay * 1000.0).ceil() as u64,
        }
    } else if (400..500).contains(&response.status) && response.status != 408 {
        EventVoiceResponse::Refused(response.status)
    } else {
        EventVoiceResponse::Uncertain
    }
}
