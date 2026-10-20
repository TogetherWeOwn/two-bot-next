//! Runtime services shared by the gateway and interaction server.

mod voice_room_plan;
pub mod voice_rooms;

// Worker-path runtimes: the staging-soak acceptance pins
// `containment_runtime::{start, ContainmentSettings, SettingsSource}` as its
// seam. The binary compiles these same source files as crate-local modules.
// Visibility widens (`pub(crate)` -> `pub`) with no logic change.
pub mod containment_runtime;
pub mod join_risk_runtime;
pub mod raid_runtime;
