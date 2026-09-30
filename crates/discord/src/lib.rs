//! Twilight adapter seam for two-bot-next (ADR 0001).
//!
//! Converts twilight gateway dispatches into framework-free
//! [`two_bot_core::CoreEvent`]s. The bot crate owns the shard and the cache;
//! this crate owns the *translation*, so S3+ handler logic stays testable
//! without a gateway connection.

pub mod adapter;
pub mod intents;
pub mod interactions;
pub mod internal_actions;
pub mod pipeline;

pub use adapter::event_to_core;
pub use intents::{cache_resource_types, gateway_intents, needs_message_content};
pub use interactions::{
    command_to_twilight, publish_commands, refusal_response, response_for_slash, route_interaction,
    RoutedInteraction,
};
pub use pipeline::{
    build_cache, ChannelClassifier, InviteSource, MemPipeline, NoClassification, NoInvites,
    Pipeline, PipelineSnapshots, ScriptedInvites,
};
