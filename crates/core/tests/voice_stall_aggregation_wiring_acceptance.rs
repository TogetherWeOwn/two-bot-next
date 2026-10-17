//! Wiring half of the voice-stall aggregation acceptance contract (see
//! `voice_stall_aggregation_acceptance.rs` for the aggregation API). Split
//! into its own test binary so these pins compile and fail at runtime
//! before the build lands, while the API pins are compile-red.
//!
//! Contract for the Builder, in the guild actor runtime
//! (`crates/bot/src/voice_rooms.rs`, non-test code):
//!
//! - Every per-tick stall publish (the actor loop gauge publish and the
//!   watchdog sibling's snapshot publish) reports through
//!   `set_voice_actor_halted(...)` / `set_voice_actor_queue(...)`, keyed by
//!   that guild actor and its incarnation generation, so one guild can no
//!   longer overwrite another's stall.
//! - Actor shutdown retires its contribution via `remove_voice_actor(...)`
//!   with the same generation it published under.
//! - The last-writer-wins `set_voice_halted(...)` / `set_voice_queue(...)`
//!   calls are gone from the runtime: leaving either in place reintroduces
//!   the overwrite this slice removes. Creator-map freshness and in-flight
//!   REST age publishes are out of scope and stay as they are.
//!
//! And in `docs/metrics.md`: the three existing series keep their names and
//! their rows describe the aggregate semantics (any-actor halt, per-lane
//! sum, maximum oldest age), so scrape consumers are not misled into
//! reading them as one guild's state.

use std::path::{Path, PathBuf};

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The actor-runtime source without its external test module, so literals
/// inside `voice_rooms_tests.rs` cannot satisfy the wiring pins. (Inline
/// `#[cfg(test)]` helpers elsewhere in the file do not publish gauges.)
fn runtime_non_test_source() -> String {
    let source = std::fs::read_to_string(repository_root().join("crates/bot/src/voice_rooms.rs"))
        .expect("voice rooms source");
    match source.find("#[path = \"voice_rooms_tests.rs\"]") {
        Some(cut) => source[..cut].to_owned(),
        None => source,
    }
}

#[test]
fn voice_stall_wiring_runtime_publishes_per_actor_contributions() {
    let source = runtime_non_test_source();
    for needle in [
        "set_voice_actor_halted(",
        "set_voice_actor_queue(",
        "remove_voice_actor(",
    ] {
        assert!(
            source.contains(needle),
            "guild actor runtime must publish/retire per-actor stall contributions via {needle}"
        );
    }
}

#[test]
fn voice_stall_wiring_runtime_no_longer_overwrites_the_global_stall_gauges() {
    let source = runtime_non_test_source();
    for needle in [".set_voice_halted(", ".set_voice_queue("] {
        assert!(
            !source.contains(needle),
            "last-writer-wins publish {needle} must leave the guild actor runtime"
        );
    }
}

fn metrics_doc_row(series: &str) -> String {
    let doc =
        std::fs::read_to_string(repository_root().join("docs/metrics.md")).expect("metrics doc");
    doc.lines()
        .find(|line| line.contains(&format!("`{series}")))
        .unwrap_or_else(|| panic!("docs/metrics.md must document {series}"))
        .to_lowercase()
}

#[test]
fn voice_stall_wiring_metrics_doc_describes_aggregate_semantics() {
    let halted = metrics_doc_row("two_bot_voice_worker_halted");
    assert!(
        halted.contains("any") && (halted.contains("actor") || halted.contains("guild")),
        "halted row must say the gauge is 1 while any guild actor is halted: {halted}"
    );
    let pending = metrics_doc_row("two_bot_voice_queue_pending");
    assert!(
        pending.contains("sum"),
        "queue-pending row must say depths sum across guild actors: {pending}"
    );
    let oldest = metrics_doc_row("two_bot_voice_queue_oldest_age_seconds");
    assert!(
        oldest.contains("max"),
        "oldest-age row must say the age is the maximum across guild actors: {oldest}"
    );
}
