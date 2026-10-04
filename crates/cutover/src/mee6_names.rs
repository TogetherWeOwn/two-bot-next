//! MEE6 custom-command name cleaning + deterministic suffixed names.
//!
//! Moved to [`two_bot_core::mee6`]: the import executor in core applies the
//! same translation the cutover CLIs use, so one implementation serves both.
//! This module re-exports it so existing `mee6_names::` paths keep working.

pub use two_bot_core::mee6::*;
