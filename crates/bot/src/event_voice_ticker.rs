//! Snapshot decision engine for event voice. Durable claims belong to the
//! runtime adapter; this layer keeps only failed-attempt pacing, never dedup.

use crate::jobs::ErrorClass;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    sync::{Arc, LazyLock},
    time::Duration,
};
use tokio::sync::{watch, Mutex};
use two_bot_core::metrics;
#[cfg(test)]
use two_bot_core::JobGate;
use two_bot_discord::executor::EventVoiceResponse;
#[cfg(test)]
use two_bot_discord::ActionExecutor;

pub const INTERVAL_MS: u64 = 60_000;
pub const LIVE_CAP: usize = 10;
const MINUTE: i64 = 60_000;
const DAY: i64 = 24 * 60 * MINUTE;

#[derive(Default)]
struct Pacing {
    create_until: i64,
    delete_until: i64,
    failures: HashMap<(String, i64), (i64, i64)>,
}

static PACING: LazyLock<Mutex<HashMap<String, Arc<Mutex<Pacing>>>>> = LazyLock::new(Mutex::default);

pub(crate) enum CreateResult {
    Response(EventVoiceResponse),
    ClaimLost,
    AdmissionDenied,
    Ineligible,
}

/// Persistence and guarded room operations travel together: production claims
/// before POST and binds its receipt before returning to the decision engine.
pub(crate) trait TickIo: Sync {
    fn create(
        &self,
        event: &Value,
        category: &str,
    ) -> impl Future<Output = Result<CreateResult, ErrorClass>> + Send;
    fn delete(
        &self,
        event: &Value,
        channel: &str,
    ) -> impl Future<Output = Result<EventVoiceResponse, ErrorClass>> + Send;
}

/// Snapshot-only REST io for the pinned seam; production decides through
/// the durable adapter instead, so this never ships in the container.
#[cfg(test)]
struct RestIo<'a> {
    rest: &'a ActionExecutor,
    guild: &'a str,
}
#[cfg(test)]
impl TickIo for RestIo<'_> {
    async fn create(&self, event: &Value, category: &str) -> Result<CreateResult, ErrorClass> {
        self.rest
            .create_event_voice_channel(self.guild, category, text(event, "name"))
            .await
            .map(CreateResult::Response)
            .map_err(|_| ErrorClass::Rest)
    }
    async fn delete(
        &self,
        _event: &Value,
        channel: &str,
    ) -> Result<EventVoiceResponse, ErrorClass> {
        self.rest
            .delete_event_voice_channel(channel)
            .await
            .map_err(|_| ErrorClass::Rest)
    }
}

pub(crate) fn text<'a>(event: &'a Value, key: &str) -> &'a str {
    event.get(key).and_then(Value::as_str).unwrap_or("")
}
pub(crate) fn starts(event: &Value) -> i64 {
    event
        .get("instance_starts_at_ms")
        .and_then(Value::as_i64)
        .unwrap_or(i64::MIN)
}
pub(crate) fn instance(event: &Value) -> i64 {
    event
        .get("binding_key_ms")
        .and_then(Value::as_i64)
        .unwrap_or_else(|| starts(event))
}
fn flag(event: &Value, key: &str) -> bool {
    event.get(key).and_then(Value::as_bool).unwrap_or(false)
}
fn occupied(event: &Value) -> bool {
    event
        .get("occupant_ids")
        .and_then(Value::as_array)
        .is_some_and(|ids| !ids.is_empty())
        || event.get("occupancy_known").and_then(Value::as_bool) == Some(false)
}
fn record(event: &Value, outcome: &str, reason: &str, channel: Option<&str>) -> Value {
    json!({"event_id": text(event, "event_id"), "instance_starts_at_ms": starts(event),
        "binding_key_ms": instance(event), "outcome": outcome, "reason": reason, "channel_id": channel})
}
fn empty_outcome(now: i64) -> Value {
    json!({"tick_ms":now, "counters":{"created":0,"rate_limited":0,"perm_denied":0,"no_category":0,"deleted":0}, "events":[]})
}

/// Snapshot-only entry behind the pinned seam; production calls
/// [`run_claimed`] directly under the preflight gate it already holds.
#[cfg(test)]
pub(crate) async fn run(
    rest: &ActionExecutor,
    guild: &str,
    events: &[Value],
    category: Option<&str>,
    now: i64,
    shutdown: &watch::Receiver<bool>,
    gate: &JobGate,
) -> Result<Value, ErrorClass> {
    let Some(_held) = gate.try_acquire() else {
        let mut outcome = empty_outcome(now);
        outcome["skipped"] = json!(true);
        outcome["reason"] = json!("gate_held");
        return Ok(outcome);
    };
    run_claimed(
        &RestIo { rest, guild },
        guild,
        events,
        category,
        now,
        shutdown,
    )
    .await
}

/// The caller owns the whole-tick gate, including its preflight and scan.
pub(crate) async fn run_claimed(
    io: &impl TickIo,
    guild: &str,
    events: &[Value],
    category: Option<&str>,
    now: i64,
    shutdown: &watch::Receiver<bool>,
) -> Result<Value, ErrorClass> {
    let pacing = PACING
        .lock()
        .await
        .entry(guild.to_owned())
        .or_default()
        .clone();
    // A second gate accidentally supplied by a caller still cannot overlap a
    // guild's wire attempts. Other guilds never wait on this lock.
    let mut pacing = pacing.lock().await;
    pacing.failures.retain(|_, (_, expires)| *expires > now);
    let mut outcome = empty_outcome(now);
    let mut records = Vec::with_capacity(events.len());
    let mut ordered: Vec<_> = events.iter().enumerate().collect();
    ordered.sort_by_key(|(index, e)| (starts(e), text(e, "event_id"), *index));
    let mut seen = HashSet::new();
    let mut live = events
        .iter()
        .filter(|e| !text(e, "voice_channel_id").is_empty() && !flag(e, "binding_deleted"))
        .map(|e| text(e, "voice_channel_id"))
        .collect::<HashSet<_>>()
        .len();
    let mut last_create = None;
    for (index, event) in ordered {
        let key = (text(event, "event_id").to_owned(), instance(event));
        let binding = text(event, "voice_channel_id");
        let deleted = flag(event, "binding_deleted");
        let result = if !seen.insert(key.clone()) {
            record(event, "skipped", "duplicate_input", None)
        } else if *shutdown.borrow() {
            record(event, "skipped", "shutdown", None)
        } else if flag(event, "voice_opt_out") {
            record(event, "skipped", "opt_out", None)
        } else if text(event, "entity_type") == "EXTERNAL" {
            record(event, "skipped", "external", None)
        } else if text(event, "entity_type") == "STAGE" {
            record(
                event,
                "skipped",
                "stage",
                event.get("attached_channel_id").and_then(Value::as_str),
            )
        } else if text(event, "entity_type") != "VOICE" || text(event, "status") != "SCHEDULED" {
            record(event, "skipped", "not_scheduled_voice", None)
        } else if !binding.is_empty() && occupied(event) {
            record(event, "noop", "occupied_kept", Some(binding))
        } else if !binding.is_empty() && !deleted && starts(event) > now.saturating_add(DAY) {
            if pacing.delete_until > now {
                record(event, "rate_limited", "delete_route_paused", Some(binding))
            } else {
                match io.delete(event, binding).await? {
                    EventVoiceResponse::Deleted => {
                        live = live.saturating_sub(1);
                        record(event, "deleted", "far_future_move", Some(binding))
                    }
                    EventVoiceResponse::RateLimited { retry_after_ms } => {
                        pacing.delete_until =
                            now.saturating_add(i64::try_from(retry_after_ms).unwrap_or(i64::MAX));
                        record(event, "rate_limited", "delete_route_paused", Some(binding))
                    }
                    EventVoiceResponse::Refused(403) => {
                        record(event, "perm_denied", "delete_forbidden", Some(binding))
                    }
                    _ => record(event, "noop", "delete_unconfirmed", Some(binding)),
                }
            }
        } else if !binding.is_empty() && !deleted {
            record(event, "noop", "bound_healthy", Some(binding))
        } else if starts(event) <= now {
            record(event, "skipped", "post_start", None)
        } else if starts(event) > now.saturating_add(15 * MINUTE) {
            record(event, "skipped", "out_of_window", None)
        } else if category.is_none() {
            record(event, "no_category", "no_category", None)
        } else if pacing.failures.contains_key(&key)
            && now >= starts(event).saturating_sub(2 * MINUTE)
        {
            record(event, "give_up", "retry_deadline", None)
        } else if pacing.create_until > now
            || pacing
                .failures
                .get(&key)
                .is_some_and(|(until, _)| *until > now)
        {
            record(event, "rate_limited", "retry_backoff", None)
        } else if live >= LIVE_CAP {
            record(event, "rate_limited", "guild_cap", None)
        } else {
            // Only actual POST attempts consume the 1/s slot, never skips.
            if let Some(last) = last_create {
                tokio::time::sleep_until(last + Duration::from_secs(1)).await;
            }
            if *shutdown.borrow() {
                record(event, "skipped", "shutdown", None)
            } else {
                last_create = Some(tokio::time::Instant::now());
                match io.create(event, category.unwrap_or_default()).await? {
                    CreateResult::ClaimLost => record(event, "skipped", "claimed_elsewhere", None),
                    CreateResult::Ineligible => record(event, "skipped", "window_closed", None),
                    CreateResult::AdmissionDenied => {
                        record(event, "rate_limited", "create_admission", None)
                    }
                    CreateResult::Response(EventVoiceResponse::Created(channel)) => {
                        live += 1;
                        pacing.failures.remove(&key);
                        tracing::info!(
                            event_id = text(event, "event_id"),
                            instance_start = instance(event),
                            channel_id = channel,
                            actor_tick = now,
                            "event voice channel created"
                        );
                        record(
                            event,
                            if deleted { "recreated" } else { "created" },
                            "created",
                            Some(&channel),
                        )
                    }
                    CreateResult::Response(response) => {
                        pacing.failures.insert(
                            key,
                            (
                                now.saturating_add(90_000),
                                starts(event).saturating_add(DAY),
                            ),
                        );
                        let (kind, reason) = match response {
                            EventVoiceResponse::RateLimited { retry_after_ms } => {
                                pacing.create_until = now.saturating_add(
                                    i64::try_from(retry_after_ms).unwrap_or(i64::MAX),
                                );
                                ("rate_limited", "create_route_paused")
                            }
                            EventVoiceResponse::Refused(403) => ("perm_denied", "create_forbidden"),
                            _ => ("rate_limited", "create_unconfirmed"),
                        };
                        tracing::warn!(
                            event_id = text(event, "event_id"),
                            actor_tick = now,
                            reason,
                            "event voice create failed; retry deferred"
                        );
                        record(event, kind, reason, None)
                    }
                }
            }
        };
        records.push((index, result));
    }
    records.sort_by_key(|(index, _)| *index);
    for (_, row) in &records {
        let kind = text(row, "outcome");
        let counter = match kind {
            "recreated" => "created",
            "give_up" => "rate_limited",
            other => other,
        };
        if let Some(count) = outcome["counters"].get_mut(counter) {
            *count = json!(count.as_u64().unwrap_or_default() + 1);
            metrics::global().event_voice_tick(counter);
        }
    }
    outcome["events"] = Value::Array(records.into_iter().map(|(_, row)| row).collect());
    Ok(outcome)
}
