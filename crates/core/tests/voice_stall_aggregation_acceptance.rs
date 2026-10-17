//! Acceptance contract: a healthy guild actor must not hide another guild
//! actor's voice stall (paired test card for the "metrics: prevent healthy
//! guilds from hiding voice stalls" build slice). Test-first: this file is
//! compile-red until the Builder adds the per-actor aggregation seam below,
//! and the Builder must not need to edit it.
//!
//! Today `Metrics::set_voice_halted` / `Metrics::set_voice_queue` are
//! last-writer-wins (crates/core/src/metrics.rs) and every guild actor
//! publishes into those same fields every tick
//! (crates/bot/src/voice_rooms.rs), so an idle healthy guild overwrites a
//! halted guild's signal with zero between scrapes.
//!
//! Contract for `two_bot_core::metrics::Metrics` (instance-local; the
//! runtime wiring half is pinned in
//! `voice_stall_aggregation_wiring_acceptance.rs`):
//!
//! - `set_voice_actor_halted(actor, generation, halted)` records one guild
//!   actor's halt latch. `actor` is an opaque process-local key (the guild
//!   actor's identity); it is never rendered, labelled or persisted.
//! - `set_voice_actor_queue(actor, generation, urgent, deferred,
//!   oldest_age_secs)` records that actor's queued depth by lane plus its
//!   oldest pending age (0 when its queue is empty, as today).
//! - `remove_voice_actor(actor, generation)` retires that actor
//!   incarnation's contributions (actor shutdown / guild removal).
//! - `generation` is the actor incarnation: reports from a generation older
//!   than the one currently recorded for that actor are ignored, a report
//!   from a newer generation replaces the whole recorded contribution
//!   (replacement retires the old incarnation), and `remove_voice_actor`
//!   with an older generation must not erase a replacement's state.
//! - The existing public series keep their names and labels and become
//!   aggregates over the current actor set, recomputed from the recorded
//!   contributions (not sticky): `two_bot_voice_worker_halted` is 1 while
//!   ANY current actor is halted, `two_bot_voice_queue_pending{lane}` is
//!   the saturating sum per lane, and
//!   `two_bot_voice_queue_oldest_age_seconds` is the maximum reported age.
//!   Publication order never changes the result, and recovery or removal
//!   of one actor clears only that actor's contribution.
//! - Storage stays bounded by the live actor set: no member/guild/channel
//!   labels, no actor key in the exposition, no per-guild series.
//! - Neighbouring series are out of scope and unchanged: creator-map
//!   generation/age and the in-flight REST age keep rendering as before.

use two_bot_core::metrics::Metrics;

// Opaque actor keys, shaped like the snowflake ids that must never appear
// in the exposition.
const ACTOR_A: u64 = 940_271_830_465_192_837;
const ACTOR_B: u64 = 812_556_409_173_828_461;
const GEN1: u64 = 1;
const GEN2: u64 = 2;

fn sample(metrics: &Metrics, prefix: &str) -> u64 {
    let text = metrics.render(None);
    let line = text
        .lines()
        .find(|line| line.starts_with(prefix))
        .unwrap_or_else(|| panic!("missing sample {prefix} in:\n{text}"));
    line.rsplit_once(' ')
        .unwrap()
        .1
        .parse()
        .unwrap_or_else(|_| panic!("non-numeric sample: {line}"))
}

fn halted(metrics: &Metrics) -> u64 {
    sample(metrics, "two_bot_voice_worker_halted ")
}

fn pending(metrics: &Metrics, lane: &str) -> u64 {
    sample(
        metrics,
        &format!("two_bot_voice_queue_pending{{lane=\"{lane}\"}} "),
    )
}

fn oldest_age(metrics: &Metrics) -> u64 {
    sample(metrics, "two_bot_voice_queue_oldest_age_seconds ")
}

#[test]
fn voice_stall_agg_series_render_at_zero_before_any_actor_reports() {
    let metrics = Metrics::default();
    assert_eq!(halted(&metrics), 0);
    assert_eq!(pending(&metrics, "urgent"), 0);
    assert_eq!(pending(&metrics, "deferred"), 0);
    assert_eq!(oldest_age(&metrics), 0);
    // Out-of-scope neighbours keep rendering (unchanged by this slice).
    assert_eq!(
        sample(&metrics, "two_bot_voice_rest_in_flight_age_seconds "),
        0
    );
    assert_eq!(sample(&metrics, "two_bot_voice_creator_map_generation "), 0);
    assert_eq!(
        sample(&metrics, "two_bot_voice_creator_map_age_seconds "),
        0
    );
}

#[test]
fn voice_stall_agg_halt_is_any_actor_independent_of_publication_order() {
    // Stalled A first, then healthy B: B's zero must not hide A's halt.
    let stalled_first = Metrics::default();
    stalled_first.set_voice_actor_halted(ACTOR_A, GEN1, true);
    stalled_first.set_voice_actor_halted(ACTOR_B, GEN1, false);
    assert_eq!(
        halted(&stalled_first),
        1,
        "healthy actor B overwrote halted actor A"
    );

    // Reverse order: same result.
    let healthy_first = Metrics::default();
    healthy_first.set_voice_actor_halted(ACTOR_B, GEN1, false);
    healthy_first.set_voice_actor_halted(ACTOR_A, GEN1, true);
    assert_eq!(halted(&healthy_first), 1);

    // Healthy actors only: no halt.
    let healthy = Metrics::default();
    healthy.set_voice_actor_halted(ACTOR_A, GEN1, false);
    healthy.set_voice_actor_halted(ACTOR_B, GEN1, false);
    assert_eq!(halted(&healthy), 0);
}

#[test]
fn voice_stall_agg_queue_depths_sum_and_oldest_age_is_the_maximum() {
    for stalled_first in [true, false] {
        let metrics = Metrics::default();
        let stalled = |m: &Metrics| m.set_voice_actor_queue(ACTOR_A, GEN1, 3, 4, 90);
        let healthy = |m: &Metrics| m.set_voice_actor_queue(ACTOR_B, GEN1, 1, 2, 10);
        if stalled_first {
            stalled(&metrics);
            healthy(&metrics);
        } else {
            healthy(&metrics);
            stalled(&metrics);
        }
        assert_eq!(
            pending(&metrics, "urgent"),
            4,
            "urgent must sum across actors"
        );
        assert_eq!(
            pending(&metrics, "deferred"),
            6,
            "deferred must sum across actors"
        );
        assert_eq!(
            oldest_age(&metrics),
            90,
            "oldest age must be the maximum across actors"
        );
    }
}

#[test]
fn voice_stall_agg_queue_depth_sums_saturate() {
    let metrics = Metrics::default();
    metrics.set_voice_actor_queue(ACTOR_A, GEN1, u64::MAX, u64::MAX - 1, 5);
    metrics.set_voice_actor_queue(ACTOR_B, GEN1, 7, 7, 5);
    assert_eq!(pending(&metrics, "urgent"), u64::MAX);
    assert_eq!(pending(&metrics, "deferred"), u64::MAX);
}

#[test]
fn voice_stall_agg_actor_updates_replace_only_its_own_contribution() {
    let metrics = Metrics::default();
    // Repeated ticks from one actor replace, never accumulate.
    metrics.set_voice_actor_queue(ACTOR_A, GEN1, 3, 3, 30);
    metrics.set_voice_actor_queue(ACTOR_A, GEN1, 1, 1, 5);
    assert_eq!(pending(&metrics, "urgent"), 1);
    assert_eq!(pending(&metrics, "deferred"), 1);
    assert_eq!(
        oldest_age(&metrics),
        5,
        "oldest age is recomputed, not sticky"
    );

    // A drains and recovers; B's stall stays fully visible.
    metrics.set_voice_actor_halted(ACTOR_A, GEN1, true);
    metrics.set_voice_actor_halted(ACTOR_B, GEN1, true);
    metrics.set_voice_actor_queue(ACTOR_B, GEN1, 2, 6, 44);
    metrics.set_voice_actor_halted(ACTOR_A, GEN1, false);
    metrics.set_voice_actor_queue(ACTOR_A, GEN1, 0, 0, 0);
    assert_eq!(halted(&metrics), 1, "B is still halted after A recovers");
    assert_eq!(pending(&metrics, "urgent"), 2);
    assert_eq!(pending(&metrics, "deferred"), 6);
    assert_eq!(oldest_age(&metrics), 44);
}

#[test]
fn voice_stall_agg_removal_retires_the_actor_contribution() {
    let metrics = Metrics::default();
    metrics.set_voice_actor_halted(ACTOR_A, GEN1, true);
    metrics.set_voice_actor_queue(ACTOR_A, GEN1, 3, 4, 90);
    metrics.set_voice_actor_halted(ACTOR_B, GEN1, false);
    metrics.set_voice_actor_queue(ACTOR_B, GEN1, 1, 2, 10);

    metrics.remove_voice_actor(ACTOR_A, GEN1);
    assert_eq!(halted(&metrics), 0, "removed actor must not stay halted");
    assert_eq!(pending(&metrics, "urgent"), 1);
    assert_eq!(pending(&metrics, "deferred"), 2);
    assert_eq!(oldest_age(&metrics), 10);

    // Removing an unknown or already-removed actor changes nothing.
    metrics.remove_voice_actor(ACTOR_A, GEN1);
    metrics.remove_voice_actor(424_242, GEN1);
    assert_eq!(pending(&metrics, "urgent"), 1);
    assert_eq!(oldest_age(&metrics), 10);

    // Removing the last actor returns every aggregate to zero; the series
    // still render so scrapes and alerts see an explicit healthy zero.
    metrics.remove_voice_actor(ACTOR_B, GEN1);
    assert_eq!(halted(&metrics), 0);
    assert_eq!(pending(&metrics, "urgent"), 0);
    assert_eq!(pending(&metrics, "deferred"), 0);
    assert_eq!(oldest_age(&metrics), 0);
}

#[test]
fn voice_stall_agg_old_generation_cannot_touch_a_replacement_actor() {
    let metrics = Metrics::default();
    // Generation 1 of actor A stalls hard, then A is replaced: generation 2
    // starts healthy and its first report retires the old contribution.
    metrics.set_voice_actor_halted(ACTOR_A, GEN1, true);
    metrics.set_voice_actor_queue(ACTOR_A, GEN1, 3, 4, 90);
    metrics.set_voice_actor_queue(ACTOR_A, GEN2, 0, 0, 0);
    metrics.set_voice_actor_halted(ACTOR_A, GEN2, false);
    assert_eq!(halted(&metrics), 0);
    assert_eq!(pending(&metrics, "urgent"), 0);
    assert_eq!(oldest_age(&metrics), 0);

    // Late reports from the replaced generation (e.g. its watchdog sibling
    // outliving it by a tick) must not resurrect the old stall.
    metrics.set_voice_actor_halted(ACTOR_A, GEN1, true);
    metrics.set_voice_actor_queue(ACTOR_A, GEN1, 9, 9, 99);
    assert_eq!(halted(&metrics), 0, "stale generation resurrected a halt");
    assert_eq!(pending(&metrics, "urgent"), 0);
    assert_eq!(oldest_age(&metrics), 0);

    // The replacement reports real work; a late shutdown of the old
    // generation must not erase it.
    metrics.set_voice_actor_queue(ACTOR_A, GEN2, 1, 0, 5);
    metrics.remove_voice_actor(ACTOR_A, GEN1);
    assert_eq!(
        pending(&metrics, "urgent"),
        1,
        "old-generation removal erased the replacement"
    );
    assert_eq!(oldest_age(&metrics), 5);

    // Shutdown of the current generation does retire it.
    metrics.remove_voice_actor(ACTOR_A, GEN2);
    assert_eq!(pending(&metrics, "urgent"), 0);
    assert_eq!(oldest_age(&metrics), 0);
}

#[test]
fn voice_stall_agg_exposition_never_carries_actor_identity_or_new_labels() {
    let metrics = Metrics::default();
    metrics.set_voice_actor_halted(ACTOR_A, GEN1, true);
    metrics.set_voice_actor_queue(ACTOR_A, GEN1, 3, 4, 90);
    // Many actors must not grow the exposition: storage and samples stay
    // bounded by the aggregate families, never per-guild series.
    for actor in 0..50_u64 {
        let actor = 700_000_000_000_000_000 + actor;
        metrics.set_voice_actor_halted(actor, GEN1, false);
        metrics.set_voice_actor_queue(actor, GEN1, 1, 1, 1);
    }
    let text = metrics.render(None);
    for forbidden in [
        ACTOR_A.to_string(),
        ACTOR_B.to_string(),
        "700000000000000000".to_owned(),
        "guild=".to_owned(),
        "actor=".to_owned(),
        "channel=".to_owned(),
        "member=".to_owned(),
    ] {
        assert!(
            !text.contains(&forbidden),
            "exposition leaked {forbidden:?}"
        );
    }
    // Exactly the pre-existing samples: halted (label-free), one queue
    // sample per existing lane label, oldest age (label-free).
    let stall_samples: Vec<&str> = text
        .lines()
        .filter(|line| {
            line.starts_with("two_bot_voice_worker_halted ")
                || line.starts_with("two_bot_voice_queue_pending{")
                || line.starts_with("two_bot_voice_queue_oldest_age_seconds ")
        })
        .collect();
    assert_eq!(
        stall_samples.len(),
        4,
        "stall samples must stay fixed-cardinality: {stall_samples:?}"
    );
    assert!(!text.contains("two_bot_voice_worker_halted{"));
    assert!(!text.contains("two_bot_voice_queue_oldest_age_seconds{"));
    // The 50 extra actors really did contribute to the sums (3 + 50 urgent).
    assert_eq!(pending(&metrics, "urgent"), 53);
    assert_eq!(pending(&metrics, "deferred"), 54);
    assert_eq!(halted(&metrics), 1);
    assert_eq!(oldest_age(&metrics), 90);
}
