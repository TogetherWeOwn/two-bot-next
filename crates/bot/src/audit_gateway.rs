//! Gateway recording for the staff audit sink.
//!
//! [`translate`] runs on the serial dispatch worker before the funnel mutates
//! the cache: member role/nickname deltas and voice channel boundaries need
//! the pre-update rows. [`record_all`] stores the translated rows through the
//! shared [`crate::audit_runtime`] handle before the gateway checkpoint
//! commits; the store write is idempotent, so a crash between the two replays
//! safely. A failed write logs the entry id only (never content or error
//! text) and the dispatch continues — recording never stalls the gateway.
//!
//! Rows carry IDs, counts and flags only. Correlation to the in-process
//! moderation service (the `moderation-success:` rows) lands with the
//! moderation wiring; without a bot id the audit-log rows stay uncorrelated
//! `discord-audit:` rows, which is exactly the legacy shape for foreign
//! executor entries.

use std::sync::OnceLock;

use twilight_cache_inmemory::InMemoryCache;
use twilight_gateway::Event;
use two_bot_core::audit::AuditEvent;
use two_bot_core::audit_mirror::AuditMirror;
use two_bot_core::classify::{
    classify_member_update, classify_moderation_audit, classify_raw_message,
    classify_voice_boundary, MemberDelta, RawAuditLogEntry, RawDispatch, VoiceBoundary,
};

use crate::audit_runtime::{handle, AuditRuntime};

/// `TWO_MODERATION_AUDIT_SECRET` is a container secret (see
/// `wrangler/src/container-env.ts` `NOT_FORWARDED`): env-only here, read once
/// per process. The systemd credential-file path lands with the moderation
/// wiring; an absent secret only means uncorrelated audit-log rows.
static SECRET: OnceLock<Option<String>> = OnceLock::new();

fn cached_secret() -> Option<String> {
    SECRET
        .get_or_init(|| {
            std::env::var("TWO_MODERATION_AUDIT_SECRET")
                .ok()
                .filter(|value| !value.is_empty())
        })
        .clone()
}

/// Translate one gateway dispatch into audit rows, reading pre-update state
/// from the cache the funnel has not yet mutated. Pure apart from the cached
/// secret env read; `sequence` feeds the raw-message fallback identity.
pub(crate) fn translate(
    event: &Event,
    cache: &InMemoryCache,
    observed_at: &str,
    sequence: u64,
) -> Vec<AuditEvent> {
    let secret = cached_secret();
    let bot_id = cache.current_user().map(|user| user.id.get().to_string());
    translate_with(
        event,
        cache,
        observed_at,
        sequence,
        secret.as_deref(),
        bot_id.as_deref(),
    )
}

/// [`translate`] with explicit correlation inputs (the tests pin these; the
/// worker resolves them from the environment and the cache).
pub(crate) fn translate_with(
    event: &Event,
    cache: &InMemoryCache,
    observed_at: &str,
    sequence: u64,
    secret: Option<&str>,
    bot_id: Option<&str>,
) -> Vec<AuditEvent> {
    match event {
        Event::MemberUpdate(update) => {
            let guild_id = update.guild_id.get();
            let member_id = update.user.id.get();
            let new_roles: Vec<String> = update.roles.iter().map(ToString::to_string).collect();
            let Some(old) = cache.member(update.guild_id, update.user.id) else {
                // No baseline: reporting current roles as granted would lie.
                return Vec::new();
            };
            let old_roles: Vec<String> = old.roles().iter().map(ToString::to_string).collect();
            let nickname_changed = old.nick() != update.nick.as_deref();
            let delta = MemberDelta::diff(nickname_changed, &old_roles, &new_roles);
            classify_member_update(guild_id, member_id, delta, observed_at, false)
                .into_iter()
                .collect()
        }
        Event::VoiceStateUpdate(update) => {
            let Some(guild) = update.guild_id else {
                return Vec::new();
            };
            // Pre-update channel: mute/deafen/camera frames carry no channel
            // change and are not session boundaries (mirrors the funnel read).
            let old_channel = cache
                .voice_state(update.user_id, guild)
                .map(|state| state.channel_id().get());
            let new_channel = update.channel_id.map(|channel| channel.get());
            let Some(boundary) = VoiceBoundary::classify(old_channel, new_channel) else {
                return Vec::new();
            };
            let is_bot = update.member.as_ref().is_some_and(|member| member.user.bot);
            vec![classify_voice_boundary(
                guild.get(),
                update.user_id.get(),
                boundary,
                observed_at,
                is_bot,
            )]
        }
        Event::MessageUpdate(message) => {
            let inner = &message.0;
            raw_message(
                "MESSAGE_UPDATE",
                sequence,
                observed_at,
                inner.guild_id.map(|guild| guild.get().to_string()),
                Some(inner.channel_id.get().to_string()),
                Some(inner.id.get().to_string()),
                Some(inner.author.id.get().to_string()),
                inner.edited_timestamp.map(|stamp| {
                    two_bot_core::format_iso_millis(stamp.as_micros().div_euclid(1000))
                }),
            )
            .into_iter()
            .collect()
        }
        Event::MessageDelete(deleted) => raw_message(
            "MESSAGE_DELETE",
            sequence,
            observed_at,
            deleted.guild_id.map(|guild| guild.get().to_string()),
            Some(deleted.channel_id.get().to_string()),
            Some(deleted.id.get().to_string()),
            None,
            None,
        )
        .into_iter()
        .collect(),
        Event::GuildAuditLogEntryCreate(entry) => {
            let Some(guild) = entry.guild_id.map(|guild| guild.get().to_string()) else {
                return Vec::new();
            };
            // Legacy reads `entry.createdTimestamp`: the snowflake's embedded
            // millis (Discord epoch 1420070400000).
            let created_timestamp_ms = ((entry.id.get() >> 22) + 1_420_070_400_000) as i64;
            let options = entry.options.as_ref();
            let raw = RawAuditLogEntry {
                action_id: u16::from(entry.action_type),
                log_entry_id: entry.id.get().to_string(),
                executor_id: entry.user_id.map(|user| user.get().to_string()),
                target_id: entry.target_id.map(|target| target.get().to_string()),
                reason: entry.reason.clone(),
                created_timestamp_ms,
                extra_channel_id: options
                    .and_then(|info| info.channel_id)
                    .map(|channel| channel.get().to_string()),
                extra_count: options
                    .and_then(|info| info.count.clone())
                    .map(serde_json::Value::String),
                extra_removed: options
                    .and_then(|info| info.members_removed.clone())
                    .map(serde_json::Value::String),
            };
            classify_moderation_audit(&raw, &guild, bot_id, secret)
                .into_iter()
                .collect()
        }
        _ => Vec::new(),
    }
}

/// One raw message dispatch through the core classifier. Shard 0: this
/// deployment runs a single shard ([`twilight_gateway::ShardId::ONE`]).
#[allow(clippy::too_many_arguments)]
fn raw_message(
    event_type: &str,
    sequence: u64,
    observed_at: &str,
    guild_id: Option<String>,
    channel_id: Option<String>,
    message_id: Option<String>,
    author_id: Option<String>,
    edited_timestamp: Option<String>,
) -> Option<AuditEvent> {
    let packet = RawDispatch {
        op: 0,
        event_type: Some(event_type),
        sequence: Some(sequence),
        shard_id: 0,
        guild_id: guild_id.as_deref(),
        channel_id: channel_id.as_deref(),
        message_id: message_id.as_deref(),
        author_id: author_id.as_deref(),
        edited_timestamp: edited_timestamp.as_deref(),
    };
    classify_raw_message(&packet, observed_at)
}

/// Store translated rows through the shared handle. A failed write logs the
/// entry id only and the dispatch continues; no handle (audit unconfigured)
/// is a silent no-op.
pub(crate) async fn record_all(events: &[AuditEvent]) {
    if events.is_empty() {
        return;
    }
    let Some(runtime) = handle() else {
        return;
    };
    record_into(&runtime, events).await;
}

/// [`record_all`] against an explicit runtime (the tests inject a double).
pub(crate) async fn record_into<M: AuditMirror>(runtime: &AuditRuntime<M>, events: &[AuditEvent]) {
    for event in events {
        if runtime.record(event).await.is_err() {
            tracing::warn!(entry_id = %event.entry_id, "audit_gateway_store_failed");
        }
    }
}

#[cfg(test)]
#[path = "audit_gateway_tests.rs"]
mod tests;
