//! Twilight adapter seam for two-bot-next (ADR 0001).
//!
//! Converts twilight gateway dispatches into framework-free
//! [`two_bot_core::CoreEvent`]s. The bot crate owns the shard and the cache;
//! this crate owns the *translation*, so S3+ handler logic stays testable
//! without a gateway connection.

pub mod adapter;
pub mod audit_mirror;
pub mod automod;
pub mod channel_access;
pub mod executor;
mod executor_metrics;
pub mod intents;
pub mod interactions;
pub mod internal_actions;
mod message_safety;
pub mod pipeline;

pub use adapter::event_to_core;
pub use executor::{
    lockdown_masks, pace_delay_ms, paced_step, throw_for_status, timeout_until_iso, unlock_masks,
    ActionExecutor, ChannelCall, ChannelCallOutcome, DiscordCall, DiscordError, EveryoneOverwrite,
    PacedStep, RawResponse, KICK_INTERVAL_MS, MAX_AUDIT_REASON_CHARS, MAX_MESSAGE_CHARS,
    MODERATION_TIMEOUT_MS, PACE_INTERVAL_MS,
};
pub use intents::{cache_resource_types, gateway_intents, needs_message_content};
pub use interactions::{
    command_to_twilight, publish_commands, refusal_response, response_for_slash, route_interaction,
    RoutedInteraction,
};
pub use pipeline::{
    build_cache, ChannelClassifier, InviteSource, MemPipeline, NoClassification, NoInvites,
    Pipeline, PipelineSnapshots, ScriptedInvites,
};
