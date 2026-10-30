//! Exercise the actual binary-private guard with a real library VoiceRuntime.
//! Actor/view assertions live in voice_disconnect_poison_acceptance_tests.rs;
//! here the real downstream collector proves the guard finishes disconnect.
#![cfg(test)]

use super::{VoiceConnectionGuard, VoiceEventSink};
use crate::drop_poison_test_serial::DROP_POISON_SERIAL;
use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;
use two_bot::voice_rooms::{BotAccess, GuildSnapshot, VoiceRuntime};
use two_bot_core::game_activity::{ActivitySnapshot, ActivityType, PresenceDelta};
use two_bot_cutover::voice_rooms::PgRoomStore;
use two_bot_discord::voice_rooms::RoomHttp;

fn connection_guard_drop(poisoned: bool) {
    let holder_entered = Arc::new(AtomicBool::new(false));
    let runtime = Arc::new(
        VoiceRuntime::<PgRoomStore, RoomHttp>::new(
            || std::panic::panic_any("no actor factory or external I/O in this fixture"),
            Duration::from_secs(60),
            true,
        )
        .with_live_source({
            let entered = Arc::clone(&holder_entered);
            move || {
                // ensure_actor resolves settings while holding actors. This
                // public seam poisons the actual library mutex, without a
                // production test hook or an imitation VoiceEventSink.
                entered.store(true, Ordering::Release);
                std::panic::panic_any("poison the real actors holder");
            }
        }),
    );
    if poisoned {
        let holder = catch_unwind(AssertUnwindSafe(|| {
            runtime.publish_snapshot(
                100,
                GuildSnapshot {
                    channels: vec![],
                    members: vec![],
                    bot: BotAccess {
                        member_id: 999,
                        guild_owner_id: 998,
                        system_channel_id: None,
                        member_roles: vec![],
                        roles: vec![],
                    },
                    member_roles: HashMap::new(),
                },
            );
        }));
        assert!(holder.is_err());
        assert!(
            holder_entered.load(Ordering::Acquire),
            "panic was inside the actors holder"
        );
    }
    let collector = runtime.game_activity_collector();
    let before = two_bot_core::funnel::now_millis_for_test();
    let joined_at = before - 10_000;
    collector.observe_presence(
        100,
        PresenceDelta {
            user_key: "300".into(),
            observed_at_ms: joined_at,
            activities: vec![ActivitySnapshot {
                activity_type: ActivityType::Playing,
                name: "Fixture game".into(),
                application_id: None,
                details: None,
                state: None,
                party_size: None,
                assets: false,
                button_labels: vec![],
                button_urls: vec![],
                stream_url: None,
            }],
        },
    );
    collector.observe_join(100, 300, joined_at);
    let sink: Arc<dyn VoiceEventSink> = runtime.clone();
    let guard = VoiceConnectionGuard(Some(sink));
    // The poison counter series is process-global: hold the shared lock so
    // this drop cannot land inside a counter test's delta window.
    let outcome = {
        let _serial = DROP_POISON_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        catch_unwind(AssertUnwindSafe(|| drop(guard)))
    };
    let after = two_bot_core::funnel::now_millis_for_test();
    // End the play interval well after disconnect. An omitted/aborted
    // close_all_opens would incorrectly credit the offline gap as voice time.
    let stopped_at = after + 60_000;
    collector.observe_presence(
        100,
        PresenceDelta {
            user_key: "300".into(),
            observed_at_ms: stopped_at,
            activities: vec![],
        },
    );
    let rows = collector.flush(100, 900_000, stopped_at);
    assert!(
        outcome.is_ok(),
        "VoiceConnectionGuard::drop must survive real actor poison"
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].game.as_deref(), Some("Fixture game"));
    assert_eq!(rows[0].session_count, 1);
    assert!(rows[0].in_voice_ms >= before - joined_at);
    assert!(
        rows[0].in_voice_ms <= after - joined_at,
        "the guard must close real open voice spans at disconnect, not at flush"
    );
    assert!(rows[0].in_voice_ms < rows[0].total_play_ms);
}

#[test]
fn healthy_voice_connection_guard_drop_closes_real_runtime_voice_spans() {
    connection_guard_drop(false);
}

#[test]
fn poisoned_voice_connection_guard_drop_reaches_real_runtime_cleanup() {
    connection_guard_drop(true);
}
