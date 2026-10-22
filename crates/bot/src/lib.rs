//! Runtime services shared by the gateway and interaction server.

mod voice_room_plan;
pub mod voice_rooms;

// Bounded gateway reconnect schedule: the gateway receive loop paces
// `Reconnect` failures on it, and the voice-actor load backoff shares its
// jitter stream. The supervised episode runner stays in the binary.
pub mod gateway_backoff;

// Worker-path runtimes: the staging-soak acceptance pins
// `containment_runtime::{start, ContainmentSettings, SettingsSource}` as its
// seam. The binary compiles these same source files as crate-local modules.
// Visibility widens (`pub(crate)` -> `pub`) with no logic change.
pub mod containment_runtime;
pub mod join_risk_runtime;
pub mod raid_runtime;

// Shared by the raid/containment tuning test modules; declared once so the
// file is not loaded as two modules (clippy::duplicate_mod).
#[cfg(test)]
mod tuning_test_support;

// Shared serial lock for drop-poison counter tests; the binary declares the
// same file so its counter tests share one lock per test binary (one module
// per crate, so no clippy::duplicate_mod).
#[cfg(test)]
mod drop_poison_test_serial;
