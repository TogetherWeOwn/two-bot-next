//! Framework-free domain core for two-bot-next.
//!
//! This crate knows nothing about Discord wire types: it models guild-member,
//! voice-session and moderation events as plain Rust data so the same logic
//! can be driven by the twilight adapter (`two-bot-discord`), by unit tests,
//! or by future transports. Slices S3+ build on these seams.

pub mod config;
pub mod events;
pub mod health;

pub use config::Config;
pub use events::{CoreEvent, VoiceSessionDelta};
pub use health::{ComponentStatus, HealthReport};
