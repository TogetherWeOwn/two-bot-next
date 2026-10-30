//! Loud live `disconnect` for poisoned actor registries (no silent recovery).
//!
//! Contract under test (follow-up to #1530): a poisoned live `disconnect`
//! (trait `disconnect` via a direct sink call, NOT through
//! `VoiceConnectionGuard::drop`) PANICS without counting
//! `two_bot_drop_poison_total{guard="voice_connection_guard"}` — fail loud.
//! The guard's drop-only disconnect (pinned by
//! `gateway_drop_poison_counter_acceptance_tests.rs`) still recovers with a
//! count. A healthy live disconnect succeeds without counting.
//!
//! RED on main (silent `into_inner` in shared `disconnect`): the poisoned
//! panic assertion fails (no panic; views invalidated). GREEN after Builder
//! (live `disconnect` uses `.expect`; drop-only variant uses
//! `recover_drop_poison`). Note: existing
//! `voice_disconnect_poison_acceptance_tests.rs` pins silent `disconnect`
//! (no panic); Builder updates it to the loud contract (expect panic) or to
//! the drop-only variant — this file pins loud and never changes.
//!
//! `disconnect` stores `gateway_connected=false` before acquiring the registry,
//! so a loud panic still disconnects the gateway but invalidates no views and
//! closes no spans (panic before the loop). The test asserts exactly that.
#![cfg(test)]

use super::*;
use crate::drop_poison_test_serial::DROP_POISON_SERIAL;
use std::panic::{catch_unwind, AssertUnwindSafe};

const GUARD: &str = "voice_connection_guard";

fn drop_poison_count(guard: &str) -> Option<u64> {
    let rendered = two_bot_core::metrics::global().render(None);
    let prefix = format!("two_bot_drop_poison_total{{guard=\"{guard}\"}} ");
    rendered.lines().find_map(|line| {
        line.strip_prefix(prefix.as_str())?
            .trim()
            .parse::<u64>()
            .ok()
    })
}

async fn live_disconnect(poisoned: bool, expect_panic: bool) {
    let runtime = Arc::new(test_runtime(Trace::default()));
    assert!(runtime.publish_snapshot(GUILD, snapshot(&[500], vec![])));
    runtime.gateway_now_connected();
    let actor = runtime.live_actor(GUILD).expect("seeded real actor");
    let generation = actor.live.read_state().generation;
    assert!(actor.live.read_state().ready);
    assert!(runtime.gateway_connected.load(Ordering::Acquire));
    if poisoned {
        let holder = catch_unwind(AssertUnwindSafe(|| {
            let _held = runtime.actors.lock().expect("healthy actor registry");
            std::panic::panic_any("poison the actor registry");
        }));
        assert!(holder.is_err());
        assert!(runtime.actors.is_poisoned());
    }

    // Direct sink call, NOT through the guard's drop: the live path.
    let sink: Arc<dyn VoiceEventSink> = runtime.clone();
    let (outcome, before, after) = {
        let _serial = DROP_POISON_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let before = drop_poison_count(GUARD).unwrap_or(0);
        let outcome = catch_unwind(AssertUnwindSafe(|| sink.disconnect()));
        let after = drop_poison_count(GUARD).unwrap_or(0);
        (outcome, before, after)
    };
    let gateway_disconnected = !runtime.gateway_connected.load(Ordering::Acquire);
    let (ready, after_generation) = {
        let live = actor.live.read_state();
        (live.ready, live.generation)
    };
    runtime
        .actors
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clear();

    assert_eq!(after, before, "live disconnect must not emit {GUARD}");
    assert!(
        gateway_disconnected,
        "transport loss stops snapshot self-healing even when loud"
    );
    if expect_panic {
        assert!(
            outcome.is_err(),
            "poisoned live disconnect must panic (fail loud)"
        );
        assert!(
            ready,
            "loud failure must panic before invalidating the actor view"
        );
        assert_eq!(
            after_generation, generation,
            "loud failure must panic before bumping the generation"
        );
    } else {
        assert!(outcome.is_ok(), "healthy live disconnect must succeed");
        assert!(!ready, "healthy disconnect invalidates the actor view");
        assert_eq!(
            after_generation,
            generation + 1,
            "healthy disconnect bumps the generation"
        );
    }
}

#[tokio::test]
async fn healthy_live_disconnect_succeeds_without_counter() {
    live_disconnect(false, false).await;
}

#[tokio::test]
async fn poisoned_live_disconnect_panics_without_counter() {
    live_disconnect(true, true).await;
}
