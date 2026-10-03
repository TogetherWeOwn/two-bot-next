//! Hermetic acceptance cases against the public rename-coalescer core API.

use two_bot_core::voice_rename_coalescer::{
    should_rename, QueueOutcome, RenameCoalescer, RenameError, MAX_CHANNEL_NAME_CHARS,
    MAX_PENDING_PER_CHANNEL,
};

// ---- should_rename ----

#[test]
fn identical_name_needs_no_rename() {
    assert!(!should_rename("Lounge", "Lounge"));
    assert!(!should_rename("", ""));
}

#[test]
fn any_difference_needs_a_rename() {
    assert!(should_rename("Lounge", "Lounge 2"));
    // Comparison is exact: case and spacing are significant.
    assert!(should_rename("Lounge", "lounge"));
    assert!(should_rename("Lounge", " Lounge"));
    assert!(should_rename("Lounge", "Lounge "));
}

// ---- queue_rename ----

#[test]
fn first_update_queues_one_pending_name() {
    let mut coalescer = RenameCoalescer::new();
    assert!(coalescer.is_empty());
    assert_eq!(
        coalescer.queue_rename(10, "Apex #1"),
        Ok(QueueOutcome::Queued)
    );
    assert_eq!(coalescer.pending(10), Some("Apex #1"));
    assert!(coalescer.contains(10));
    assert_eq!(coalescer.len(), 1);
    assert_eq!(coalescer.pending_slots(), 1);
}

#[test]
fn second_update_overwrites_pending() {
    let mut coalescer = RenameCoalescer::new();
    coalescer.queue_rename(10, "Apex #1").unwrap();
    assert_eq!(
        coalescer.queue_rename(10, "Apex #2"),
        Ok(QueueOutcome::Coalesced {
            previous: "Apex #1".to_owned()
        })
    );
    assert_eq!(coalescer.pending(10), Some("Apex #2"));
    assert_eq!(coalescer.len(), 1);
}

#[test]
fn identical_name_is_a_no_op() {
    let mut coalescer = RenameCoalescer::new();
    coalescer.queue_rename(10, "Apex #1").unwrap();
    assert_eq!(
        coalescer.queue_rename(10, "Apex #1"),
        Ok(QueueOutcome::Unchanged)
    );
    assert_eq!(coalescer.pending(10), Some("Apex #1"));
    assert_eq!(coalescer.len(), 1);
}

#[test]
fn depth_is_bounded_at_one_per_channel() {
    assert_eq!(MAX_PENDING_PER_CHANNEL, 1);
    let mut coalescer = RenameCoalescer::new();
    for name in ["one", "two", "three", "four", "five"] {
        coalescer.queue_rename(10, name).unwrap();
        assert_eq!(coalescer.pending(10), Some(name));
        assert_eq!(coalescer.len(), 1);
        assert_eq!(coalescer.pending_slots(), 1);
    }
}

#[test]
fn channels_are_independent_slots() {
    let mut coalescer = RenameCoalescer::new();
    coalescer.queue_rename(10, "A").unwrap();
    coalescer.queue_rename(20, "B").unwrap();
    coalescer.queue_rename(30, "C").unwrap();
    assert_eq!(coalescer.pending_slots(), 3);
    // Coalescing one channel leaves the others untouched.
    assert_eq!(
        coalescer.queue_rename(20, "B2"),
        Ok(QueueOutcome::Coalesced {
            previous: "B".to_owned()
        })
    );
    assert_eq!(coalescer.pending(10), Some("A"));
    assert_eq!(coalescer.pending(20), Some("B2"));
    assert_eq!(coalescer.pending(30), Some("C"));
    assert_eq!(coalescer.pending_slots(), 3);
}

// ---- backlog never blocks create/delete ----

#[test]
fn forgetting_on_delete_drops_only_that_channel() {
    let mut coalescer = RenameCoalescer::new();
    coalescer.queue_rename(10, "A").unwrap();
    coalescer.queue_rename(20, "B").unwrap();
    assert!(coalescer.forget(10));
    assert_eq!(coalescer.pending(10), None);
    assert_eq!(coalescer.pending(20), Some("B"));
    assert_eq!(coalescer.pending_slots(), 1);
    // Forgetting an unknown or already-forgotten channel is a no-op false.
    assert!(!coalescer.forget(10));
    assert!(!coalescer.forget(99));
}

#[test]
fn queuing_never_fails_because_other_channels_have_backlogs() {
    let mut coalescer = RenameCoalescer::new();
    for id in 1..=50u64 {
        assert_eq!(
            coalescer.queue_rename(id, "room"),
            Ok(QueueOutcome::Queued),
            "channel {id}"
        );
    }
    assert_eq!(coalescer.pending_slots(), 50);
    assert_eq!(
        coalescer.queue_rename(51, "new room"),
        Ok(QueueOutcome::Queued)
    );
}

// ---- take_pending / observe_current / clear ----

#[test]
fn take_pending_removes_and_returns_the_name() {
    let mut coalescer = RenameCoalescer::new();
    coalescer.queue_rename(10, "Apex #1").unwrap();
    assert_eq!(coalescer.take_pending(10), Some("Apex #1".to_owned()));
    assert!(coalescer.is_empty());
    assert_eq!(coalescer.take_pending(10), None);
}

#[test]
fn observe_current_drops_a_matching_entry_and_keeps_a_stale_one() {
    let mut coalescer = RenameCoalescer::new();
    coalescer.queue_rename(10, "Apex #1").unwrap();
    // Rename already in effect (confirmed or made by hand): dropped.
    assert!(coalescer.observe_current(10, "Apex #1"));
    assert!(coalescer.is_empty());
    // Observing an empty slot drops nothing.
    assert!(!coalescer.observe_current(10, "Apex #1"));
    coalescer.queue_rename(10, "Apex #2").unwrap();
    // Current name differs: the pending rename is still needed.
    assert!(!coalescer.observe_current(10, "Apex #1"));
    assert_eq!(coalescer.pending(10), Some("Apex #2"));
    // Unknown channels never store anything.
    assert!(!coalescer.observe_current(99, "Anything"));
    assert_eq!(coalescer.pending_slots(), 1);
}

#[test]
fn clear_drops_every_channel() {
    let mut coalescer = RenameCoalescer::new();
    coalescer.queue_rename(10, "A").unwrap();
    coalescer.queue_rename(20, "B").unwrap();
    coalescer.clear();
    assert!(coalescer.is_empty());
    assert_eq!(coalescer.pending_slots(), 0);
}

// ---- refusals ----

#[test]
fn zero_channel_id_is_refused() {
    let mut coalescer = RenameCoalescer::new();
    assert_eq!(
        coalescer.queue_rename(0, "Apex #1"),
        Err(RenameError::InvalidChannelId)
    );
    assert!(coalescer.is_empty());
}

#[test]
fn empty_names_are_refused() {
    let mut coalescer = RenameCoalescer::new();
    assert_eq!(coalescer.queue_rename(10, ""), Err(RenameError::EmptyName));
    assert!(coalescer.is_empty());
}

#[test]
fn names_over_100_scalars_are_refused_and_errors_echo_nothing() {
    let mut coalescer = RenameCoalescer::new();
    let max = "n".repeat(MAX_CHANNEL_NAME_CHARS);
    coalescer.queue_rename(10, &max).unwrap();
    assert_eq!(MAX_CHANNEL_NAME_CHARS, 100);
    let long = "é".repeat(MAX_CHANNEL_NAME_CHARS + 1);
    assert_eq!(
        coalescer.queue_rename(20, &long),
        Err(RenameError::NameTooLong {
            chars: MAX_CHANNEL_NAME_CHARS + 1,
            max: MAX_CHANNEL_NAME_CHARS,
        })
    );
    // The refusal leaves the map unchanged, and the message echoes no input.
    assert_eq!(coalescer.pending(10), Some(max.as_str()));
    assert_eq!(coalescer.pending(20), None);
    let message = RenameError::NameTooLong {
        chars: MAX_CHANNEL_NAME_CHARS + 1,
        max: MAX_CHANNEL_NAME_CHARS,
    }
    .to_string();
    assert!(!message.contains('é'));
}

#[test]
fn refusals_leave_existing_entries_unchanged() {
    let mut coalescer = RenameCoalescer::new();
    coalescer.queue_rename(10, "Apex #1").unwrap();
    let before = coalescer.clone();
    let _ = coalescer.queue_rename(0, "x");
    let _ = coalescer.queue_rename(10, "");
    let _ = coalescer.queue_rename(10, &"y".repeat(MAX_CHANNEL_NAME_CHARS + 1));
    assert_eq!(coalescer, before);
}
