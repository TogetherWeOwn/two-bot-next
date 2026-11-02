//! Real actor/view cleanup behind VoiceEventSink, paired with the binary's
//! actual VoiceConnectionGuard drop tests (the guard is binary-private).
//! Pins the drop-only `disconnect_from_drop` path: live `disconnect` fails
//! loud on a poisoned registry (pinned by
//! `voice_disconnect_loud_acceptance_tests.rs`), while the guard's drop
//! still reaches all cleanup with a counted recovery.
//! Builder retarget (Test Author delegation, see the loud file's header).
#![cfg(test)]

use super::*;
use crate::drop_poison_test_serial::DROP_POISON_SERIAL;
use std::panic::{catch_unwind, AssertUnwindSafe};

async fn disconnect(poison_actors: bool, poison_view: bool) {
    let runtime = Arc::new(test_runtime(Trace::default()));
    assert!(runtime.publish_snapshot(GUILD, snapshot(&[500], vec![])));
    runtime.gateway_now_connected();
    let actor = runtime.live_actor(GUILD).expect("seeded real actor");
    let generation = actor.live.read_state().generation;
    assert!(actor.live.read_state().ready);
    assert!(runtime.gateway_connected.load(Ordering::Acquire));
    if poison_actors {
        let holder = catch_unwind(AssertUnwindSafe(|| {
            let _held = runtime.actors.lock().expect("healthy actor registry");
            std::panic::panic_any("poison the actor registry");
        }));
        assert!(holder.is_err());
        assert!(runtime.actors.is_poisoned());
    }
    if poison_view {
        let holder = catch_unwind(AssertUnwindSafe(|| {
            let _held = actor.live.inner.write().expect("healthy live view");
            std::panic::panic_any("poison the downstream live view");
        }));
        assert!(holder.is_err());
        assert!(actor.live.inner.is_poisoned());
    }

    let sink: Arc<dyn VoiceEventSink> = runtime.clone();
    // A poisoned registry counts on the process-global series: hold the
    // shared lock so this cleanup cannot land in a delta window.
    let outcome = {
        let _serial = DROP_POISON_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        catch_unwind(AssertUnwindSafe(|| sink.disconnect_from_drop()))
    };
    let gateway_disconnected = !runtime.gateway_connected.load(Ordering::Acquire);
    let (ready, after_generation, not_ready_since) = {
        let live = actor.live.read_state();
        (live.ready, live.generation, live.not_ready_since)
    };
    // Do not call remove_guild: its independent poisoned-registry acquisition
    // is not this cleanup path. Close real actor senders without another panic.
    runtime
        .actors
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clear();

    assert!(
        outcome.is_ok(),
        "real VoiceEventSink::disconnect_from_drop must not panic"
    );
    assert!(
        gateway_disconnected,
        "transport loss stops snapshot self-healing"
    );
    assert!(!ready, "the previously ready actor view is invalidated");
    assert_eq!(
        after_generation,
        generation + 1,
        "in-flight writes lose their generation"
    );
    assert!(
        not_ready_since.is_none(),
        "transport loss is not a connected-view fault"
    );
}

#[tokio::test]
async fn healthy_real_voice_disconnect_invalidates_the_actor_view() {
    disconnect(false, false).await;
}

#[tokio::test]
async fn poisoned_actor_registry_disconnect_invalidates_the_real_view() {
    disconnect(true, false).await;
}

#[tokio::test]
async fn poisoned_downstream_live_view_disconnect_still_invalidates_it() {
    disconnect(false, true).await;
}

#[tokio::test]
async fn poisoned_registry_and_live_view_disconnect_reaches_all_cleanup() {
    disconnect(true, true).await;
}
