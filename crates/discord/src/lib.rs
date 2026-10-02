//! Twilight adapter seam for two-bot-next (ADR 0001).
//!
//! Converts twilight gateway dispatches into framework-free
//! [`two_bot_core::CoreEvent`]s. The bot crate owns the shard and the cache;
//! this crate owns the *translation*, so S3+ handler logic stays testable
//! without a gateway connection.

pub mod adapter;
pub mod audit_mirror;
pub mod automod;
pub mod automod_activation;
pub mod channel_access;
pub mod command_registry;
pub mod executor;
mod executor_metrics;
pub mod intents;
pub mod interactions;
pub mod internal_actions;
#[cfg(feature = "db")]
pub mod internal_channel_moderation;
pub mod internal_events;
#[cfg(feature = "db")]
pub mod leveling_runtime;
mod message_safety;
pub mod pipeline;
pub mod ratelimit_guard;
#[cfg(feature = "db")]
pub mod rsvp;
pub mod voice_rooms;

#[cfg(feature = "db")]
pub use leveling_runtime::{LevelingRuntime, OrderedLevelingPipeline};

pub use adapter::event_to_core;
pub use executor::{
    lockdown_masks, pace_delay_ms, paced_step, throw_for_status, timeout_until_iso, unlock_masks,
    ActionExecutor, ChannelCall, ChannelCallOutcome, DiscordCall, DiscordError, EveryoneOverwrite,
    PacedStep, RawResponse, KICK_INTERVAL_MS, MAX_AUDIT_REASON_CHARS, MAX_MESSAGE_CHARS,
    MODERATION_TIMEOUT_MS, PACE_INTERVAL_MS,
};
pub use intents::{cache_resource_types, gateway_intents, needs_message_content};
pub use interactions::{
    command_to_twilight, deferred_response, dispatch_interaction, publish_commands,
    refusal_response, response_for_slash, route_interaction, text_response, DispatchOptions,
    InteractionReplyTransport, RoutedInteraction,
};
pub use internal_events::{event_status_name, scheduled_event_body, EventActionError, EventCall};
pub use pipeline::{
    build_cache, ChannelClassifier, InviteSource, MemPipeline, MessageEligibility,
    NoClassification, NoInvites, Pipeline, PipelineSnapshots, ScriptedInvites,
};
