//! Twilight adapter seam for two-bot-next (ADR 0001).
//!
//! Converts twilight gateway dispatches into framework-free
//! [`two_bot_core::CoreEvent`]s. The bot crate owns the shard and the cache;
//! this crate owns the *translation*, so S3+ handler logic stays testable
//! without a gateway connection.

pub mod adapter;
pub mod intents;

pub use adapter::event_to_core;
pub use intents::{cache_resource_types, gateway_intents};
