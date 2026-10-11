//! Rename-pipeline acceptance: the naming engine drives the rename
//! coalescer through the public core API only.
//!
//! The room-lifecycle runtime (V1) does not exist yet, so these tests model
//! its future call path on every membership change: re-render the room name
//! with [`resolve_room_name`], compare against the authoritative current name
//! with [`should_rename`], coalesce into the backlog with
//! [`queue_rename`](RenameCoalescer::queue_rename), and drain with
//! [`take_pending`](RenameCoalescer::take_pending) /
//! [`observe_current`](RenameCoalescer::observe_current).
//!
//! Every case crosses the render-to-queue seam. Isolated engine rows (golden
//! corpus, parser properties) and isolated coalescer rows (refusals, forget,
//! clear, backlog shape) stay in `voice_naming_wiring.rs` and
//! `voice_rename_coalescer.rs` and are not re-asserted here.

use std::collections::HashMap;

use two_bot_core::voice_naming::{resolve_room_name, ChannelKind, RoomContext};
use two_bot_core::voice_rename_coalescer::{
    should_rename, RenameCoalescer, MAX_PENDING_PER_CHANNEL,
};

fn ctx(owner: &str, members: u32, room_number: u32, seed: u64) -> RoomContext {
    RoomContext {
        channel_kind: ChannelKind::Temporary,
        room_number,
        owner_name: owner.to_string(),
        original_creator_name: owner.to_string(),
        member_count: members,
        owner_present: true,
        live_count: 0,
        user_limit: 0,
        game_name: String::new(),
        stream_title: String::new(),
        members_playing: 0,
        parties: Vec::new(),
        timestamp: 1_790_683_200, // 2026-09-29 12:00:00 UTC (a Tuesday).
        room_minutes: 0,
        game_minutes: 0,
        tz_offset_minutes: 0,
        seed,
        named_lists: HashMap::new(),
        fallback_name: "Hangout".to_string(),
    }
}

/// One runtime tick: render the desired name, then queue it only when it
/// differs from the authoritative current name. Returns the rendered name.
fn render_and_queue(
    coalescer: &mut RenameCoalescer,
    channel_id: u64,
    current_name: &str,
    template: &str,
    context: &RoomContext,
    raw_name: &str,
) -> String {
    let desired = resolve_room_name(template, context, raw_name);
    if should_rename(current_name, &desired) {
        coalescer
            .queue_rename(channel_id, &desired)
            .expect("rendered names are queueable");
    }
    desired
}

/// A membership-change re-render that differs queues exactly one pending
/// name; an identical re-render queues nothing.
#[test]
fn differing_rerender_queues_one_identical_rerender_queues_nothing() {
    let mut coalescer = RenameCoalescer::new();
    let template = "@@owner@@ ## @@num@@ <<person/people>>";
    let solo = ctx("Ava", 1, 1, 7);
    // Authoritative guild state already shows the solo render
    // ("Ava #1 1 person"), so the identical re-render queues nothing.
    let current = "Ava #1 1 person";

    // Identical re-render: the rename is already in effect, so nothing queues.
    let same = render_and_queue(&mut coalescer, 10, current, template, &solo, current);
    assert!(!should_rename(current, &same));
    assert!(coalescer.is_empty());

    // Membership change renders a different name: exactly one pending name.
    let busy = render_and_queue(
        &mut coalescer,
        10,
        current,
        template,
        &ctx("Ava", 4, 1, 7),
        current,
    );
    assert!(should_rename(current, &busy));
    assert_ne!(same, busy);
    assert_eq!(coalescer.pending(10), Some(busy.as_str()));
    assert_eq!(coalescer.len(), 1);

    // Re-rendering the same busy state is a no-op on the backlog.
    let again = render_and_queue(
        &mut coalescer,
        10,
        current,
        template,
        &ctx("Ava", 4, 1, 7),
        current,
    );
    assert_eq!(again, busy);
    assert_eq!(coalescer.pending(10), Some(busy.as_str()));
    assert_eq!(coalescer.len(), 1);
}

/// A burst of renders before delivery collapses to the latest name only:
/// depth stays bounded at one pending name per channel.
#[test]
fn burst_of_renders_collapses_to_latest_name_only() {
    assert_eq!(MAX_PENDING_PER_CHANNEL, 1);
    let mut coalescer = RenameCoalescer::new();
    let template = "@@owner@@ ## @@num@@ <<person/people>>";
    let current = "Ava #1";

    // Four membership changes arrive before the runtime spends rename budget.
    let mut last = String::new();
    for members in [2u32, 3, 4, 5] {
        last = render_and_queue(
            &mut coalescer,
            10,
            current,
            template,
            &ctx("Ava", members, 1, 7),
            current,
        );
        assert_eq!(coalescer.pending(10), Some(last.as_str()));
        assert_eq!(coalescer.len(), 1);
        assert_eq!(coalescer.pending_slots(), 1);
    }

    // Only the latest render survives; earlier renders left no trace.
    assert!(last.ends_with("5 people"), "unexpected {last:?}");
    assert_eq!(coalescer.pending(10), Some(last.as_str()));
    assert_eq!(coalescer.len(), 1);

    // Re-rendering the final busy state through the same tick leaves the
    // collapsed entry untouched: no second slot, latest name still pending.
    let settled = render_and_queue(
        &mut coalescer,
        10,
        current,
        template,
        &ctx("Ava", 5, 1, 7),
        current,
    );
    assert_eq!(settled, last);
    assert_eq!(coalescer.pending(10), Some(last.as_str()));
    assert_eq!(coalescer.len(), 1);
}

/// Seeded random picks stay stable across re-renders: membership change
/// updates the counts without re-rolling the emoji or list choice, and the
/// backlog still collapses to the latest render.
#[test]
fn seeded_picks_stable_across_rerenders_through_queue() {
    let mut coalescer = RenameCoalescer::new();
    let template = "@@random_emoji@@ [[den/crew/lair]] @@num@@ <<person/people>>";
    let current = "old name";

    let solo = render_and_queue(
        &mut coalescer,
        10,
        current,
        template,
        &ctx("Ava", 1, 1, 9),
        current,
    );
    assert!(solo.ends_with("1 person"), "unexpected {solo:?}");
    assert_eq!(coalescer.pending(10), Some(solo.as_str()));

    let busy = render_and_queue(
        &mut coalescer,
        10,
        current,
        template,
        &ctx("Ava", 4, 1, 9),
        &solo,
    );
    assert!(busy.ends_with("4 people"), "unexpected {busy:?}");

    // Same seed, so the emoji/list head is untouched; only the counts moved
    // (`@@num@@` and the plural sit after the seeded picks in the template).
    let solo_head: Vec<&str> = solo.split(' ').take(2).collect();
    let busy_head: Vec<&str> = busy.split(' ').take(2).collect();
    assert_eq!(
        solo_head, busy_head,
        "membership change re-rolled the seeded pick: {solo:?} vs {busy:?}"
    );

    // The second render coalesced over the first: latest name only.
    assert_eq!(coalescer.pending(10), Some(busy.as_str()));
    assert_eq!(coalescer.len(), 1);
}

/// Delivery (`take_pending`) plus guild-state sync (`observe_current`) clears
/// the slot, and channels stay independent through the whole pipeline.
#[test]
fn delivery_and_observe_clear_slot_channels_stay_independent() {
    let mut coalescer = RenameCoalescer::new();
    let template = "@@owner@@ ## @@num@@ <<person/people>>";
    let current_a = "Ava #1";
    let current_b = "Bo #2";

    let desired_a = render_and_queue(
        &mut coalescer,
        10,
        current_a,
        template,
        &ctx("Ava", 4, 1, 7),
        current_a,
    );
    let desired_b = render_and_queue(
        &mut coalescer,
        20,
        current_b,
        template,
        &ctx("Bo", 2, 2, 11),
        current_b,
    );
    assert_ne!(desired_a, desired_b);
    assert_eq!(coalescer.pending_slots(), 2);

    // Spend rename budget on channel 10: delivered, slot cleared, 20 untouched.
    assert_eq!(coalescer.take_pending(10), Some(desired_a.clone()));
    assert_eq!(coalescer.pending(10), None);
    assert_eq!(coalescer.pending(20), Some(desired_b.as_str()));

    // Guild state now shows the delivered name on 20: sync drops the entry.
    assert!(coalescer.observe_current(20, &desired_b));
    assert!(coalescer.is_empty());

    // A stale observation keeps a still-needed rename.
    let desired_c = render_and_queue(
        &mut coalescer,
        30,
        current_a,
        template,
        &ctx("Ava", 3, 1, 7),
        current_a,
    );
    assert!(!coalescer.observe_current(30, current_a));
    assert_eq!(coalescer.pending(30), Some(desired_c.as_str()));
}
