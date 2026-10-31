//! Shared serial lock for drop-poison counter tests.
//!
//! `two_bot_drop_poison_total{guard}` is process-global, so a poisoned drop
//! in one test file can land inside another test's before/drop/after window
//! and flake its exact-delta assertion under parallel `cargo test`. Every
//! test that drops a poisoned guard — or measures one of these counters —
//! holds this lock around that window.
#![cfg(test)]

use std::sync::{LazyLock, Mutex};

/// Serializes drop-poison counter windows in this test binary. Callers
/// recover a poisoned lock; a panicking holder must not fail other tests.
pub(crate) static DROP_POISON_SERIAL: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
