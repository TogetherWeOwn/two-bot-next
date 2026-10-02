//! Internal scheduled-event executors, independent of the HTTP receiver.
//!
//! The receiver must validate the request, enforce action flags, acquire the
//! durable execution claim, and resolve `event_key` in the configured guild
//! before constructing these calls. IDs here are trusted mapping results, not
//! raw website fields. No automatic retries: an unreadable success or failed
//! mirror write after a Discord mutation must retain the execution fence.

use serde_json::{json, Value};
use twilight_http::request::Request;
use twilight_http::routing::Route;
use twilight_model::id::marker::{GuildMarker, ScheduledEventMarker};
use two_bot_core::internal_actions::{ActionError, ErrorCode, EventInput, EventPlace};
use two_bot_core::{normalize_event, EventStatus, RawScheduledEvent, ScheduledEventMirror};

use crate::executor::snowflake;
use crate::{ActionExecutor, DiscordError};

/// An already-validated action. `None` creates; a mapped ID updates. Cancelling
/// retains the mapping so a later edit cannot silently recreate the event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventCall {
    Upsert {
        event_id: Option<String>,
        input: EventInput,
    },
    Cancel {
        event_id: String,
    },
    Read {
        event_id: String,
    },
}

/// Distinguishes upstream refusal from failures after a successful mutation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EventActionError {
    #[error(transparent)]
    Discord(#[from] DiscordError),
    #[error("Discord answered with an unreadable scheduled event")]
    InvalidResponse,
    #[error("scheduled-event mirror write failed")]
    Mirror,
}

impl EventActionError {
    #[must_use]
    pub fn is_safe_pre_mutation(&self) -> bool {
        matches!(self, Self::Discord(error) if error.is_safe_pre_mutation())
    }

    /// Wire error for the receiver. Details never include upstream event text
    /// or database errors; those are not safe response payloads.
    #[must_use]
    pub fn action_error(&self) -> ActionError {
        let (code, reason) = match self {
            Self::Discord(DiscordError::Rejected(_)) => {
                (ErrorCode::DiscordRejected, "discord_rejected")
            }
            Self::Discord(DiscordError::Timeout) => {
                (ErrorCode::UpstreamTimeout, "upstream_timeout")
            }
            Self::Discord(DiscordError::RateLimited) => (ErrorCode::RateLimited, "rate_limited"),
            Self::Discord(DiscordError::Unavailable(_)) => {
                (ErrorCode::DiscordUnavailable, "discord_unavailable")
            }
            Self::InvalidResponse => (ErrorCode::DiscordUnavailable, "discord_mirror_unreadable"),
            Self::Mirror => (ErrorCode::Internal, "event_mirror_write_failed"),
        };
        ActionError::new(code, self.to_string(), reason)
    }
}

/// Legacy `scheduledEventBody`: omitted description stays omitted (not null),
/// and switching to an external event explicitly clears the channel.
#[must_use]
pub fn scheduled_event_body(input: &EventInput) -> Value {
    let mut body = json!({
        "name": input.name,
        "scheduled_start_time": input.starts_at,
        "scheduled_end_time": input.ends_at,
        "privacy_level": 2,
    });
    if let Some(description) = &input.description {
        body["description"] = json!(description);
    }
    match &input.place {
        EventPlace::Channel(channel_id) => {
            body["entity_type"] = json!(2);
            body["channel_id"] = json!(channel_id);
        }
        EventPlace::Location(location) => {
            body["entity_type"] = json!(3);
            body["channel_id"] = Value::Null;
            body["entity_metadata"] = json!({"location": location});
        }
    }
    body
}

/// Legacy `eventStatusName` uses Discord's uppercase spelling, not the DB's
/// lowercase `cancelled`. Unknown numeric codes remain readable as numbers;
/// the mirror normalizer still rejects them rather than persisting bad status.
#[must_use]
pub fn event_status_name(status: &Value) -> Option<String> {
    match status.as_i64() {
        Some(1) => Some("SCHEDULED".to_owned()),
        Some(2) => Some("ACTIVE".to_owned()),
        Some(3) => Some("COMPLETED".to_owned()),
        Some(4) => Some("CANCELED".to_owned()),
        _ => status.as_number().map(ToString::to_string),
    }
}

impl ActionExecutor {
    /// Execute one event call, then await its single-row mirror refresh before
    /// returning the legacy result. `observed_at` is the caller's UTC-millis
    /// clock reading; it is injected so offline tests can assert exact rows.
    /// The receiver stores/replays the result; replay must not call this again.
    pub async fn execute_event<M: ScheduledEventMirror>(
        &self,
        guild_id: &str,
        call: &EventCall,
        mirror: &M,
        observed_at: &str,
    ) -> Result<Value, EventActionError> {
        let guild = snowflake::<GuildMarker>(guild_id)?;
        let (route, body, expected_id, outcome) = match call {
            EventCall::Upsert { event_id, input } => {
                if let EventPlace::Channel(channel_id) = &input.place {
                    snowflake::<twilight_model::id::marker::ChannelMarker>(channel_id)?;
                }
                let route = if let Some(id) = event_id {
                    Route::UpdateGuildScheduledEvent {
                        guild_id: guild.get(),
                        scheduled_event_id: snowflake::<ScheduledEventMarker>(id)?.get(),
                    }
                } else {
                    Route::CreateGuildScheduledEvent {
                        guild_id: guild.get(),
                    }
                };
                (
                    route,
                    Some(scheduled_event_body(input)),
                    event_id.as_deref(),
                    if event_id.is_some() {
                        "updated"
                    } else {
                        "created"
                    },
                )
            }
            EventCall::Cancel { event_id } => (
                Route::UpdateGuildScheduledEvent {
                    guild_id: guild.get(),
                    scheduled_event_id: snowflake::<ScheduledEventMarker>(event_id)?.get(),
                },
                Some(json!({"status": 4})),
                Some(event_id.as_str()),
                "cancelled",
            ),
            EventCall::Read { event_id } => (
                Route::GetGuildScheduledEvent {
                    guild_id: guild.get(),
                    scheduled_event_id: snowflake::<ScheduledEventMarker>(event_id)?.get(),
                    with_user_count: false,
                },
                None,
                Some(event_id.as_str()),
                "read",
            ),
        };
        let mut request = Request::builder(&route);
        if let Some(body) = body {
            request =
                request.body(serde_json::to_vec(&body).map_err(|error| {
                    DiscordError::Rejected(format!("build event body: {error}"))
                })?);
        }
        let request = request
            .build()
            .map_err(|error| DiscordError::Rejected(format!("build: {error}")))?;
        // Legacy cancel does not swallow 404 or an already-cancelled 400.
        // Idempotency is durable replay of the original claim, not a new PATCH.
        let response = self.call_once_raw(request, &[200, 201]).await?;
        let body: Value = serde_json::from_slice(&response.body)
            .map_err(|_| EventActionError::InvalidResponse)?;
        let string = |key: &str| body.get(key).and_then(Value::as_str).map(str::to_owned);
        let event = normalize_event(&RawScheduledEvent {
            id: string("id"),
            name: string("name"),
            scheduled_start_time: string("scheduled_start_time"),
            channel_id: string("channel_id"),
            description: string("description"),
            status: body.get("status").and_then(Value::as_i64),
        })
        .ok_or(EventActionError::InvalidResponse)?;
        // A mismatched identity must never refresh another event/guild row.
        if snowflake::<ScheduledEventMarker>(&event.id).is_err()
            || expected_id.is_some_and(|id| id != event.id)
            || body
                .get("guild_id")
                .is_some_and(|id| id.as_str() != Some(guild_id))
            || (matches!(call, EventCall::Cancel { .. }) && event.status != EventStatus::Cancelled)
        {
            return Err(EventActionError::InvalidResponse);
        }
        mirror
            .upsert(guild_id, observed_at, &event)
            .await
            .map_err(|_| EventActionError::Mirror)?;
        if matches!(call, EventCall::Read { .. }) {
            Ok(json!({
                "outcome": "read",
                "event_id": event.id,
                "name": event.name,
                "starts_at": body["scheduled_start_time"],
                "location": body.get("entity_metadata").and_then(|metadata| metadata.get("location")).and_then(Value::as_str),
                "status": event_status_name(&body["status"]),
                "observed_at": observed_at,
            }))
        } else {
            Ok(json!({"outcome": outcome, "event_id": event.id}))
        }
    }
}
