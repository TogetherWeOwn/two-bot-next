//! Fault fixtures for the actual dispatch worker, never a runtime injection API.
//! Register as a child of leveling_runtime under the existing test-support feature.
#![cfg(feature = "test-support")]

use super::*;

const PAYLOAD: &str = "synthetic-worker-poison-not-a-diagnostic";

fn poison<T: Send + 'static>(lock: Arc<Mutex<T>>) {
    let holder = std::thread::spawn(move || {
        let _held = lock.lock().expect("fixture starts healthy");
        std::panic::panic_any(PAYLOAD);
    });
    assert!(holder.join().is_err(), "fault holder must be joined");
}

pub fn poison_leveling<S: FunnelStore, I: InviteSource, P: InviteSnapshotStore>(
    owner: &OrderedLevelingPipeline<S, I, P>,
) {
    poison(Arc::clone(&owner.pending.0));
}

pub fn poison_facts(facts: &DeferredCommunityFacts) {
    poison(Arc::clone(&facts.0));
}

pub fn assert_leveling_preserved<S: FunnelStore, I: InviteSource, P: InviteSnapshotStore>(
    owner: &OrderedLevelingPipeline<S, I, P>,
    expected: usize,
) {
    assert!(owner.pending.0.is_poisoned());
    let held = owner
        .pending
        .0
        .lock()
        .expect_err("never heal poisoned leveling");
    assert_eq!(
        held.get_ref().len(),
        expected,
        "never drain untrusted requests"
    );
}

pub fn assert_facts_preserved(facts: &DeferredCommunityFacts, expected: (usize, usize, usize)) {
    assert!(facts.0.is_poisoned());
    let held = facts.0.lock().expect_err("never heal poisoned facts");
    let state = held.get_ref();
    assert_eq!(
        (state.pending.len(), state.rules.len(), state.joins.len()),
        expected,
        "never drain an untrusted fact family"
    );
}
