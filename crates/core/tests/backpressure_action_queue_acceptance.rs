//! Protected acceptance tests: the voice action queue is bounded and
//! refuses new work past its cap instead of buffering forever.
//!
//! PROTECTED FILE: the paired Builder implements the production bound
//! without editing this file. It is a `crates/core/tests/` integration
//! target, so no wiring outside it exists at all.
//!
//! Contract under test (all through the public [`ActionQueue`] API plus the
//! two additive seams below):
//!
//! - Burst growth is bounded: after `BURST` enqueues with no drain, the
//!   guild's total pending (urgent + deferred) never exceeds
//!   [`MAX_QUEUE_DEPTH`]. Today `enqueue` is uncapped, so the burst tests
//!   fail on the depth assertion (behavioral RED, not a compile failure).
//! - Policy is refuse-new: ``ActionQueue::try_enqueue`` returns `None` past
//!   the cap and leaves the queue unchanged; the oldest admitted work still
//!   pops first. Evict-oldest would pass the depth pin but fail the
//!   oldest-first pin, so the policy is pinned, not just the bound.
//! - Refusals are counted: ``ActionQueue::overflow_dropped`` reports the
//!   monotonic count of refused enqueues. A depth gauge alone cannot prove
//!   the excess was refused rather than lost.
//! - Healthy path is unchanged: under the cap every enqueue is admitted and
//!   urgent-before-deferred FIFO order plus full drain hold.
//!
//! Builder instructions (additive only; do not change existing signatures):
//!
//! - Add inherent `ActionQueue::try_enqueue(guild, action) -> Option<u64>`
//!   with exactly this signature. Method resolution prefers it over the
//!   adapter trait below, so these tests exercise the real bound with no
//!   test-file edit. Keep `enqueue` (still used by call sites that must
//!   admit) or rewire call sites to `try_enqueue`; either way the inherent
//!   method must exist with this name and shape.
//! - Add inherent `ActionQueue::overflow_dropped(&self) -> u64`, monotonic
//!   per queue, counting refused `try_enqueue` calls.
//! - Pick any per-guild cap in [`HEALTHY_FLOOR`]..=[`MAX_QUEUE_DEPTH`]
//!   (per-lane or combined, as long as the combined total never exceeds the
//!   ceiling) and document it on the inherent methods. A smaller cap still
//!   passes: the burst tests assert lower bounds on refusals.
//! - A refused `try_enqueue` must not allocate an id, must not touch
//!   `enqueued_at`, and must leave both lanes byte-identical.
//!
//! Regression note: the oldest-first pin also guards the coalescing paths
//! (`RenameRoom` / `RespaceChannels` overwrite a pending entry and return its
//! id): coalescing is not refusal and must not bump the overflow counter.

use two_bot_core::voice_rooms::{ActionQueue, RoomAction, MAX_QUEUE_PENDING_PER_GUILD};

/// Ceiling on one guild's total pending actions (urgent + deferred). The
/// Builder may choose a smaller cap; the burst tests assert lower bounds so
/// any cap in [`HEALTHY_FLOOR`]..=`MAX_QUEUE_DEPTH` passes.
const MAX_QUEUE_DEPTH: usize = 1024;
/// Burst sized to overflow any compliant cap: 4x the ceiling.
const BURST: usize = 4 * MAX_QUEUE_DEPTH;
/// Healthy load the cap must always admit: mixed-lane traffic well under any
/// reasonable bound (the gateway dispatch backlog is 64; the queue must hold
/// at least that much healthy traffic).
const HEALTHY_FLOOR: usize = 64;
const GUILD: u64 = 100;

/// Adapter: today's `enqueue` always admits, so the adapter always returns
/// `Some`. After the Builder adds the inherent `try_enqueue`, method
/// resolution prefers the inherent method and the refusal tests below
/// exercise the real bound. This is what makes RED behavioral: the file
/// compiles today and the assertions fail on current behavior.
trait TryEnqueue {
    fn try_enqueue(&self, guild: u64, action: RoomAction) -> Option<u64>;
}

impl TryEnqueue for ActionQueue {
    fn try_enqueue(&self, guild: u64, action: RoomAction) -> Option<u64> {
        Some(self.enqueue(guild, action))
    }
}

/// Adapter: today nothing counts overflow, so the adapter reports zero.
/// The inherent `overflow_dropped` supersedes it after the build.
trait OverflowCount {
    fn overflow_dropped(&self) -> u64;
}

impl OverflowCount for ActionQueue {
    fn overflow_dropped(&self) -> u64 {
        0
    }
}

fn delete(channel_id: u64) -> RoomAction {
    RoomAction::DeleteRoom { channel_id }
}

fn rename(channel_id: u64) -> RoomAction {
    RoomAction::RenameRoom {
        channel_id,
        name: "rename-pin".to_owned(),
    }
}

fn pending_total(queue: &ActionQueue) -> usize {
    let (urgent, deferred) = queue.pending_counts(GUILD);
    urgent + deferred
}

/// Drain every due action for the guild in pop order, completing each so the
/// single in-flight slot frees for the next. Returns the popped actions.
fn drain_all(queue: &ActionQueue) -> Vec<RoomAction> {
    let mut drained = Vec::new();
    while let Some(action) = queue.pop_due(GUILD, 0) {
        drained.push(action.action.clone());
        assert!(
            queue.mark_succeeded(&action),
            "completion of a just-popped action must release the in-flight slot"
        );
    }
    drained
}

#[test]
fn queue_burst_stays_within_documented_ceiling() {
    let queue = ActionQueue::new();
    for i in 0..BURST {
        let _ = queue.try_enqueue(GUILD, delete(100_000 + i as u64));
    }
    let total = pending_total(&queue);
    assert!(
        total <= MAX_QUEUE_DEPTH,
        "burst of {BURST} with no drain must stay within the {MAX_QUEUE_DEPTH} cap, held {total}"
    );
}

#[test]
fn queue_refuses_new_work_beyond_cap() {
    let queue = ActionQueue::new();
    let mut refused = false;
    for i in 0..=BURST {
        if queue
            .try_enqueue(GUILD, delete(200_000 + i as u64))
            .is_none()
        {
            refused = true;
            break;
        }
    }
    assert!(
        refused,
        "try_enqueue must refuse (None) once the cap is reached; a burst of {BURST} was admitted whole"
    );
    assert!(
        pending_total(&queue) <= MAX_QUEUE_DEPTH,
        "refused enqueues must leave the queue within the cap"
    );
}

#[test]
fn queue_overflow_refusals_are_counted() {
    let queue = ActionQueue::new();
    for i in 0..BURST {
        let _ = queue.try_enqueue(GUILD, delete(300_000 + i as u64));
    }
    let dropped = queue.overflow_dropped();
    let minimum = (BURST - MAX_QUEUE_DEPTH) as u64;
    assert!(
        dropped >= minimum,
        "burst of {BURST} past a cap of at most {MAX_QUEUE_DEPTH} must count at least {minimum} refusals, counted {dropped}"
    );
}

#[test]
fn queue_bounded_retention_keeps_oldest_first() {
    // Refuse-new pin: the retained set stays within the cap AND the first
    // admitted action still pops first. Evict-oldest would fail the order
    // half; unbounded growth fails the depth half.
    const FIRST_CHANNEL: u64 = 400_000;
    let queue = ActionQueue::new();
    let admitted = 2 * MAX_QUEUE_DEPTH + 8;
    for i in 0..admitted {
        let _ = queue.try_enqueue(GUILD, delete(FIRST_CHANNEL + i as u64));
    }
    let drained = drain_all(&queue);
    assert!(
        drained.len() <= MAX_QUEUE_DEPTH,
        "retained work must stay within the {MAX_QUEUE_DEPTH} cap, kept {}",
        drained.len()
    );
    match drained.first() {
        Some(RoomAction::DeleteRoom { channel_id }) => assert_eq!(
            *channel_id, FIRST_CHANNEL,
            "refuse-new keeps the oldest admitted work first"
        ),
        other => panic!("first pop must be the oldest urgent action, got {other:?}"),
    }
}

#[test]
fn queue_healthy_path_admits_orders_and_drains() {
    // Control: under the cap everything is admitted, urgent drains before
    // deferred, each lane stays FIFO, and the queue drains to empty.
    let queue = ActionQueue::new();
    for channel in [501, 502, 503] {
        assert!(
            queue.try_enqueue(GUILD, delete(channel)).is_some(),
            "urgent enqueue under the cap must admit"
        );
    }
    for channel in [601, 602] {
        assert!(
            queue.try_enqueue(GUILD, rename(channel)).is_some(),
            "deferred enqueue under the cap must admit"
        );
    }
    assert_eq!(queue.pending_counts(GUILD), (3, 2));
    let drained = drain_all(&queue);
    assert_eq!(drained.len(), 5, "healthy load must drain completely");
    let channels: Vec<u64> = drained
        .iter()
        .map(|action| match action {
            RoomAction::DeleteRoom { channel_id } | RoomAction::RenameRoom { channel_id, .. } => {
                *channel_id
            }
            other => panic!("unexpected action in healthy drain: {other:?}"),
        })
        .collect();
    assert_eq!(
        channels,
        vec![501, 502, 503, 601, 602],
        "urgent-before-deferred FIFO order must hold at low load"
    );
    assert_eq!(queue.pending_counts(GUILD), (0, 0));
}

#[test]
fn queue_healthy_mixed_burst_has_no_refusal_under_cap() {
    // Control pinning the cap floor: HEALTHY_FLOOR mixed actions must all
    // admit. The Builder's cap must be at least this large.
    let queue = ActionQueue::new();
    for i in 0..HEALTHY_FLOOR {
        let channel = 500_000 + i as u64;
        let action = if i % 2 == 0 {
            delete(channel)
        } else {
            rename(channel)
        };
        assert!(
            queue.try_enqueue(GUILD, action).is_some(),
            "mixed enqueue {} of {HEALTHY_FLOOR} must admit under the cap",
            i + 1
        );
    }
    assert_eq!(pending_total(&queue), HEALTHY_FLOOR);
    assert_eq!(
        queue.overflow_dropped(),
        0,
        "no refusal may be counted when everything fits under the cap"
    );
}

fn assert_coalescing_preserves_full_queue(original: RoomAction, newest: RoomAction) {
    // `enqueue` deliberately constructs the over-cap case: lifecycle and
    // accepted-user writes still must admit. This is a try_enqueue control,
    // not a claim that production's entire action queue has a global cap.
    for excess in [0, 7] {
        let queue = ActionQueue::new();
        let existing_id = queue.enqueue(GUILD, original.clone());
        let mut urgent = Vec::new();
        let mut deferred = vec![(existing_id, newest.clone())];
        let mut last_id = existing_id;
        for index in 1..MAX_QUEUE_PENDING_PER_GUILD + excess {
            let channel = 1_000_000 + index as u64;
            let action = if index % 2 == 0 {
                delete(channel)
            } else {
                rename(channel)
            };
            last_id = queue.enqueue(GUILD, action.clone());
            if index % 2 == 0 {
                urgent.push((last_id, action));
            } else {
                deferred.push((last_id, action));
            }
        }
        let counts = (urgent.len(), deferred.len());
        assert_eq!(queue.pending_counts(GUILD), counts);
        assert_eq!(pending_total(&queue), MAX_QUEUE_PENDING_PER_GUILD + excess);
        let oldest = queue.oldest_pending_at(GUILD).expect("pending age");
        assert_eq!(queue.overflow_dropped(), 0);
        for _ in 0..2 {
            // Mutation: checking capacity before coalescing returns None,
            // increments overflow, or allocates a second id at/past the cap.
            assert_eq!(queue.try_enqueue(GUILD, newest.clone()), Some(existing_id));
            assert_eq!(queue.pending_counts(GUILD), counts);
            assert_eq!(queue.oldest_pending_at(GUILD), Some(oldest));
            assert_eq!(queue.overflow_dropped(), 0);
        }
        if let RoomAction::RenameRoom { channel_id, name } = &newest {
            assert_eq!(
                queue.queued_rename_for(GUILD, *channel_id),
                Some(name.clone())
            );
        }
        // A truly new action in either lane is refused, exactly once per
        // call. Neither refusal may change any retained action or its id.
        for (index, rejected) in [delete(2_000_000), rename(2_000_001)]
            .into_iter()
            .enumerate()
        {
            assert_eq!(queue.try_enqueue(GUILD, rejected), None);
            assert_eq!(queue.overflow_dropped(), index as u64 + 1);
            assert_eq!(queue.pending_counts(GUILD), counts);
            assert_eq!(queue.oldest_pending_at(GUILD), Some(oldest));
        }
        // Coalescing and refusals allocate no id. Another guild remains
        // healthy even when this one is above the opt-in intake threshold.
        assert_eq!(
            queue.try_enqueue(GUILD + 1, delete(3_000_000)),
            Some(last_id + 1)
        );
        assert_eq!(queue.overflow_dropped(), 2);
        urgent.extend(deferred);
        let mut actual = Vec::new();
        while let Some(action) = queue.pop_due(GUILD, 0) {
            actual.push((action.id, action.action.clone()));
            assert!(queue.mark_succeeded(&action));
        }
        assert_eq!(
            actual, urgent,
            "both lanes retain their FIFO positions and ids"
        );
        assert_eq!(queue.pending_counts(GUILD), (0, 0));
        assert_eq!(queue.oldest_pending_at(GUILD), None);
        assert_eq!(queue.overflow_dropped(), 2);
    }
}

#[test]
fn queue_rename_coalesces_at_and_beyond_capacity_without_refusal_or_id_allocation() {
    assert_coalescing_preserves_full_queue(
        RoomAction::RenameRoom {
            channel_id: 800_000,
            name: "previous name".to_owned(),
        },
        RoomAction::RenameRoom {
            channel_id: 800_000,
            name: "newest name".to_owned(),
        },
    );
}

#[test]
fn queue_respace_coalesces_at_and_beyond_capacity_without_refusal_or_id_allocation() {
    assert_coalescing_preserves_full_queue(
        RoomAction::RespaceChannels {
            room_channel_id: 800_000,
        },
        RoomAction::RespaceChannels {
            room_channel_id: 800_000,
        },
    );
}

#[test]
fn queue_legacy_adapter_path_stays_available() {
    // Control pinning the legacy adapter path: qualified calls bypass any
    // inherent bound by design, so this passes both before and after the
    // Builder adds inherent `try_enqueue` / `overflow_dropped`. It also keeps
    // the adapter traits referenced once inherent methods shadow unqualified
    // calls, avoiding never-used warnings under `-D warnings`.
    let queue = ActionQueue::new();
    let admitted = <ActionQueue as TryEnqueue>::try_enqueue(&queue, GUILD, delete(900_000));
    assert!(
        admitted.is_some(),
        "legacy adapter path must admit on a fresh queue"
    );
    assert_eq!(
        <ActionQueue as OverflowCount>::overflow_dropped(&queue),
        0,
        "legacy adapter reports zero overflow on a fresh queue"
    );
}
