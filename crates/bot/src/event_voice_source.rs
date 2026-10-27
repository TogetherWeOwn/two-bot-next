//! Fresh Discord source and durable event-voice bindings. The ten-minute mirror
//! is not eligibility evidence. Neither a source read nor an orphaned SQL row
//! proves a room empty/deleted; that authority stays with live reconciliation.

use super::*;
use serde_json::json;
use std::collections::{BTreeSet, HashMap};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use two_bot_core::settings::{parse_event_voice_opt_outs, EVENT_VOICE_OPT_OUT_KEY};
use two_bot_discord::voice_rooms::RoomHttpError;

const MAX_SOURCE_AGE: Duration = Duration::from_secs(60);

/// This state belongs to the supervised guild context, not an individual tick.
/// A source GET's 429 must not pause channel create/delete or another guild.
#[derive(Default)]
pub(super) struct Reader {
    pauses: Mutex<HashMap<u64, tokio::time::Instant>>,
}

pub(super) struct Snapshot {
    pub events: Vec<Value>,
    guild: String,
    observed: OffsetDateTime,
    began: tokio::time::Instant,
}

impl Snapshot {
    /// Recheck after queue/admission waits as well as at publication. Wall time
    /// fences future stamps; the monotonic budget survives wall-clock rollback.
    pub fn is_fresh(&self) -> bool {
        let age = OffsetDateTime::now_utc() - self.observed;
        !age.is_negative()
            && age <= time::Duration::seconds(60)
            && self.began.elapsed() <= MAX_SOURCE_AGE
    }

    /// The mirror can change while this event waits for admission. Check the
    /// observation AND its content: a same-millisecond cancellation must also
    /// invalidate the queue entry. Never authorize a caller-supplied event.
    pub async fn eligible(
        &self,
        pool: &PgPool,
        guild: &str,
        event: &Value,
    ) -> Result<bool, ErrorClass> {
        if guild != self.guild
            || !self.is_fresh()
            || !self.events.contains(event)
            || event_voice_ticker::text(event, "entity_type") != "VOICE"
            || event_voice_ticker::text(event, "status") != "SCHEDULED"
        {
            return Ok(false);
        }
        let stamp = two_bot_core::funnel::format_iso_millis(
            i64::try_from(self.observed.unix_timestamp_nanos().div_euclid(1_000_000))
                .map_err(|_| ErrorClass::Rest)?,
        );
        let starts = two_bot_core::funnel::format_iso_millis(event_voice_ticker::starts(event));
        let current: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1 FROM scheduled_events WHERE guild_id=$1 AND event_id=$2
                AND updated_at=$3 AND status='scheduled' AND starts_at=$4 AND name=$5
                AND channel_id IS NOT DISTINCT FROM $6
             ) AND NOT EXISTS (
                SELECT 1 FROM scheduled_events_tombstone WHERE guild_id=$1 AND deleted_at >= $3
             ) AND NOT EXISTS (
                SELECT 1 FROM scheduled_events_claim WHERE guild_id=$1 AND epoch > $3
             )",
        )
        .bind(guild)
        .bind(event_voice_ticker::text(event, "event_id"))
        .bind(&stamp)
        .bind(starts)
        .bind(event_voice_ticker::text(event, "name"))
        .bind(event["attached_channel_id"].as_str())
        .fetch_one(pool)
        .await
        .map_err(|_| ErrorClass::Database)?;
        Ok(current && self.is_fresh())
    }
}

impl Reader {
    pub async fn read(
        &self,
        pool: &PgPool,
        rest: &ActionExecutor,
        guild: &str,
        shutdown: &watch::Receiver<bool>,
    ) -> Result<Option<Snapshot>, ErrorClass> {
        if canonical_snowflake(&json!(guild)) != Some(guild) {
            return Err(ErrorClass::Configuration);
        }
        let guild_id = guild.parse().map_err(|_| ErrorClass::Configuration)?;
        let began = tokio::time::Instant::now();
        if publication_stopped(shutdown)
            || self
                .pauses
                .lock()
                .await
                .get(&guild_id)
                .is_some_and(|until| *until > began)
        {
            return Ok(None);
        }
        let observed = OffsetDateTime::now_utc();
        let response = tokio::select! {
            biased;
            _ = server::shutdown_requested(shutdown.clone()) => return Ok(None),
            result = rest.read_event_voice_events(guild_id) => result,
        };
        match response {
            Ok(response) => publish(pool, guild, &response, observed, began, shutdown).await,
            Err(RoomHttpError::RateLimited { retry_after_ms, .. }) => {
                let until =
                    tokio::time::Instant::now().checked_add(Duration::from_millis(retry_after_ms));
                // An unrepresentable delay must never accidentally reopen a route.
                let until = until.ok_or(ErrorClass::Rest)?;
                self.pauses.lock().await.insert(guild_id, until);
                Ok(None)
            }
            Err(_) => Err(ErrorClass::Rest),
        }
    }
}

/// Publish the minute read with the existing epoch/tombstone protocol before
/// exposing eligibility: the mirror is also presence/erasure guild scope.
/// A newer per-event mutation can survive an otherwise successful replacement,
/// so only rows actually owned by this observation may enter the create queue.
pub(super) async fn publish(
    pool: &PgPool,
    guild: &str,
    response: &Value,
    observed: OffsetDateTime,
    began: tokio::time::Instant,
    shutdown: &watch::Receiver<bool>,
) -> Result<Option<Snapshot>, ErrorClass> {
    let mut snapshot = Snapshot {
        events: Vec::new(),
        guild: guild.to_owned(),
        observed,
        began,
    };
    if publication_stopped(shutdown) || !snapshot.is_fresh() {
        return Ok(None);
    }
    parse(response, guild)?;
    let raw = response
        .as_array()
        .ok_or(ErrorClass::Rest)?
        .iter()
        .map(|value| raw_event(value, guild))
        .collect::<Result<Vec<_>, _>>()?;
    let events = normalize_events(&raw).ok_or(ErrorClass::Rest)?;
    let stamp = two_bot_core::funnel::format_iso_millis(
        i64::try_from(observed.unix_timestamp_nanos().div_euclid(1_000_000))
            .map_err(|_| ErrorClass::Rest)?,
    );
    if replace_events(pool, guild, &stamp, &events)
        .await
        .map_err(|_| ErrorClass::Database)?
        .is_some()
    {
        return Ok(None);
    }
    let suppressed: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM scheduled_events_tombstone WHERE guild_id=$1 AND deleted_at >= $2)",
    ).bind(guild).bind(&stamp).fetch_one(pool).await.map_err(|_| ErrorClass::Database)?;
    if suppressed || publication_stopped(shutdown) || !snapshot.is_fresh() {
        return Ok(None);
    }
    let current: Vec<String> = sqlx::query_scalar(
        "SELECT event_id FROM scheduled_events WHERE guild_id=$1 AND updated_at=$2",
    )
    .bind(guild)
    .bind(&stamp)
    .fetch_all(pool)
    .await
    .map_err(|_| ErrorClass::Database)?;
    let current: BTreeSet<_> = current.into_iter().collect();
    let response = Value::Array(
        response
            .as_array()
            .ok_or(ErrorClass::Rest)?
            .iter()
            .filter(|event| event["id"].as_str().is_some_and(|id| current.contains(id)))
            .cloned()
            .collect(),
    );
    snapshot.events = load(pool, guild, &response).await?;
    if publication_stopped(shutdown) || !snapshot.is_fresh() {
        return Ok(None);
    }
    Ok(Some(snapshot))
}

fn millis(raw: &str) -> Result<i64, ErrorClass> {
    let instant = OffsetDateTime::parse(raw, &Rfc3339).map_err(|_| ErrorClass::Rest)?;
    i64::try_from(instant.unix_timestamp_nanos().div_euclid(1_000_000))
        .map_err(|_| ErrorClass::Rest)
}

fn timestamp(ms: i64) -> Result<OffsetDateTime, ErrorClass> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .map_err(|_| ErrorClass::Rest)
}

pub(super) fn parse(response: &Value, guild: &str) -> Result<Vec<Value>, ErrorClass> {
    if canonical_snowflake(&json!(guild)) != Some(guild) {
        return Err(ErrorClass::Configuration);
    }
    let values = response.as_array().ok_or(ErrorClass::Rest)?;
    let raw = values
        .iter()
        .map(|value| raw_event(value, guild))
        .collect::<Result<Vec<_>, _>>()?;
    let normalized = normalize_events(&raw).ok_or(ErrorClass::Rest)?;
    values
        .iter()
        .zip(normalized)
        .map(|(value, event)| {
            if event.name.trim().is_empty() {
                return Err(ErrorClass::Rest);
            }
            let kind = match value["entity_type"].as_u64() {
                Some(1) => "STAGE",
                Some(2) => "VOICE",
                Some(3) => "EXTERNAL",
                _ => return Err(ErrorClass::Rest),
            };
            let starts = millis(&event.starts_at)?;
            let ends = match value.get("scheduled_end_time") {
                None | Some(Value::Null) => None,
                Some(Value::String(raw)) => Some(millis(raw)?),
                _ => return Err(ErrorClass::Rest),
            };
            if ends.is_some_and(|end| end <= starts) {
                return Err(ErrorClass::Rest);
            }
            Ok(json!({
                "event_id":event.id, "name":event.name, "entity_type":kind,
                "status": match event.status {
                    two_bot_core::scheduled_events::EventStatus::Scheduled => "SCHEDULED",
                    two_bot_core::scheduled_events::EventStatus::Active => "ACTIVE",
                    two_bot_core::scheduled_events::EventStatus::Completed => "COMPLETED",
                    two_bot_core::scheduled_events::EventStatus::Cancelled => "CANCELED",
                },
                "instance_starts_at_ms":starts, "binding_key_ms":starts, "ends_at_ms":ends,
                "attached_channel_id":event.channel_id, "voice_channel_id":null,
                "voice_opt_out":false, "occupancy_known":false, "binding_deleted":false,
            }))
        })
        .collect()
}

async fn opt_outs(pool: &PgPool, guild: &str) -> Result<BTreeSet<String>, ErrorClass> {
    let value: Option<Value> =
        sqlx::query_scalar("SELECT value FROM guild_settings WHERE guild_id=$1 AND key=$2")
            .bind(guild)
            .bind(EVENT_VOICE_OPT_OUT_KEY)
            .fetch_optional(pool)
            .await
            .map_err(|_| ErrorClass::Database)?;
    match value {
        None => Ok(BTreeSet::new()),
        Some(value) => parse_event_voice_opt_outs(&value).ok_or(ErrorClass::Configuration),
    }
}

/// Re-read at create time too: an admin can disable an event while it waits in
/// the serialized queue. Never consult environment or source-controlled fields.
pub(super) async fn opted_out(pool: &PgPool, guild: &str, event: &str) -> Result<bool, ErrorClass> {
    Ok(opt_outs(pool, guild).await?.contains(event))
}

/// Overlay live binding state onto snapshot events after publication: open
/// presence spans become `occupant_ids` with `occupancy_known=true`, and an
/// `orphaned` row marks `binding_deleted=true` so the decision core reports
/// the next create as a recreate. Unbound events keep the fail-closed defaults
/// from [`parse`]: unknown occupancy reads as occupied, never empty.
///
/// An orphaned binding ignores its spans: the channel is 404-confirmed gone,
/// so nobody can be inside it, while its stale open spans (the 60s repair
/// skips orphaned rows) must not block the recreate. Spans stay intact for a
/// transient-404 restore; the repair closes them once the row rebinds.
pub(super) async fn overlay_live_state(
    pool: &PgPool,
    events: &mut [Value],
) -> Result<(), ErrorClass> {
    for event in events.iter_mut() {
        if event_voice_ticker::text(event, "voice_channel_id").is_empty() {
            continue;
        }
        let event_id = event_voice_ticker::text(event, "event_id").to_owned();
        let instance = timestamp(event_voice_ticker::instance(event))?;
        let status =
            two_bot_cutover::event_voice::event_voice_binding_status(pool, &event_id, instance)
                .await
                .map_err(|_| ErrorClass::Database)?;
        if status.as_deref() == Some("orphaned") {
            event["binding_deleted"] = json!(true);
            event["occupant_ids"] = json!(Vec::<String>::new());
            event["occupancy_known"] = json!(true);
            continue;
        }
        let occupants =
            two_bot_cutover::event_voice::open_event_voice_occupants(pool, &event_id, instance)
                .await
                .map_err(|_| ErrorClass::Database)?;
        event["binding_deleted"] = json!(false);
        event["occupant_ids"] = json!(occupants);
        event["occupancy_known"] = json!(true);
    }
    Ok(())
}

/// Preserve an existing instance key across a reschedule, including unbound
/// uncertain claims. This card does not guess between multiple live recurrence
/// instances: ambiguous source identity fails closed for the recurrence adapter.
/// Schedule refreshes never touch lease age, capacity, status or channel ownership.
pub(super) async fn load(
    pool: &PgPool,
    guild: &str,
    response: &Value,
) -> Result<Vec<Value>, ErrorClass> {
    let mut events = parse(response, guild)?;
    let opted_out = opt_outs(pool, guild).await?;
    let ids: Vec<_> = events
        .iter()
        .map(|event| event_voice_ticker::text(event, "event_id").to_owned())
        .collect();
    let mut tx = pool.begin().await.map_err(|_| ErrorClass::Database)?;
    let rows: Vec<(String, OffsetDateTime, Option<String>)> = sqlx::query_as(
        "SELECT event_id,instance_starts_at,voice_channel_id FROM event_voice_channels
         WHERE event_id=ANY($1) AND status IN ('creating','orphaned')
         ORDER BY event_id,instance_starts_at LIMIT 101 FOR UPDATE",
    )
    .bind(&ids)
    .fetch_all(&mut *tx)
    .await
    .map_err(|_| ErrorClass::Database)?;
    let mut bindings = HashMap::new();
    for (event, instance, channel) in rows {
        if bindings.insert(event, (instance, channel)).is_some() {
            return Err(ErrorClass::Rest);
        }
    }
    for event in &mut events {
        let id = event_voice_ticker::text(event, "event_id").to_owned();
        event["voice_opt_out"] = json!(opted_out.contains(&id));
        if let Some((instance, channel)) = bindings.get(&id) {
            event["binding_key_ms"] = json!(i64::try_from(
                instance.unix_timestamp_nanos().div_euclid(1_000_000)
            )
            .map_err(|_| ErrorClass::Rest)?);
            event["voice_channel_id"] = json!(channel);
            let starts = timestamp(event_voice_ticker::starts(event))?;
            let ends = event["ends_at_ms"].as_i64().map(timestamp).transpose()?;
            sqlx::query(
                "UPDATE event_voice_channels SET starts_at=$3,ends_at=$4
                 WHERE event_id=$1 AND instance_starts_at=$2",
            )
            .bind(&id)
            .bind(instance)
            .bind(starts)
            .bind(ends)
            .execute(&mut *tx)
            .await
            .map_err(|_| ErrorClass::Database)?;
        }
    }
    tx.commit().await.map_err(|_| ErrorClass::Database)?;
    Ok(events)
}
