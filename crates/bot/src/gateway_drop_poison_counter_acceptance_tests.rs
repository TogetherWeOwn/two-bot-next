//! Drop-only counted recovery for the binary-private `VoiceConnectionGuard`.
//!
//! Contract under test (follow-up to #1530): dropping the guard whose real
//! library `VoiceRuntime` actor registry is poisoned still finishes disconnect
//! — closing real open voice spans (same collector assertions as
//! `gateway_drop_poison_acceptance_tests.rs`; actor-view invalidation is pinned
//! by `voice_disconnect_poison_acceptance_tests.rs` on the shared disconnect
//! logic, which the drop-only variant preserves) — and additionally emits
//! exactly one `two_bot_drop_poison_total{guard="voice_connection_guard"}`.
//! A healthy drop leaves zero delta. A poisoned drop through the production
//! `VoiceResponder` sink pins that sink's own drop-only override the same way.
//!
//! RED on main (silent `into_inner`, no counter series): counter assertions
//! fail (series missing). GREEN after Builder (guard drop calls only the
//! drop-only disconnect using `recover_drop_poison`; live `disconnect` panics,
//! pinned by `voice_disconnect_loud_acceptance_tests.rs`). Deltas use the
//! global with a serializing lock — never absolute counts.
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
use twilight_model::application::interaction::Interaction;
use twilight_model::http::interaction::InteractionResponse;
use two_bot::voice_rooms::{
    BotAccess, GuildSnapshot, InteractionReplies, VoiceResponder, VoiceRuntime,
};
use two_bot_core::game_activity::{ActivitySnapshot, ActivityType, PresenceDelta};
use two_bot_cutover::voice_rooms::PgRoomStore;
use two_bot_discord::voice_rooms::{RoomHttp, RoomHttpError};

const GUARD: &str = "voice_connection_guard";

/// Reply seam for the responder-wrapped drop fixture. The disconnect path
/// never answers an interaction, so every method is an inert stub.
struct DropOnlyReplies;

impl InteractionReplies for DropOnlyReplies {
    async fn defer(&self, _: &Interaction) -> Result<(), RoomHttpError> {
        Ok(())
    }

    async fn complete(&self, _: &Interaction, _: InteractionResponse) -> Result<(), RoomHttpError> {
        Ok(())
    }

    async fn respond(&self, _: &Interaction, _: InteractionResponse) -> Result<(), RoomHttpError> {
        Ok(())
    }
}

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

fn connection_guard_drop_with_counter(
    poisoned: bool,
    expected_delta: u64,
    through_responder: bool,
) {
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
    // The responder variant pins the production sink's own
    // `disconnect_from_drop` override: without it the trait default forwards
    // to loud `disconnect`, which panics on the poisoned registry above.
    let sink: Arc<dyn VoiceEventSink> = if through_responder {
        Arc::new(VoiceResponder::new(
            Arc::clone(&runtime),
            Arc::new(DropOnlyReplies),
        ))
    } else {
        runtime.clone()
    };
    let guard = VoiceConnectionGuard(Some(sink));

    let (outcome, before_count, after_count) = {
        let _serial = DROP_POISON_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let before_count = drop_poison_count(GUARD);
        let outcome = catch_unwind(AssertUnwindSafe(|| drop(guard)));
        let after_count = drop_poison_count(GUARD);
        (outcome, before_count, after_count)
    };
    let after = two_bot_core::funnel::now_millis_for_test();
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
    let (before_count, after_count) = (
        before_count.expect("poison counter series must render"),
        after_count.expect("poison counter series must render"),
    );
    assert_eq!(
        after_count.saturating_sub(before_count),
        expected_delta,
        "guard drop must emit exactly {expected_delta} {GUARD} count(s)"
    );
}

#[test]
fn healthy_voice_connection_guard_drop_leaves_counter_at_zero_delta() {
    connection_guard_drop_with_counter(false, 0, false);
}

#[test]
fn poisoned_voice_connection_guard_drop_counts_exactly_one() {
    connection_guard_drop_with_counter(true, 1, false);
}

#[test]
fn poisoned_voice_connection_guard_drop_through_responder_counts_exactly_one() {
    connection_guard_drop_with_counter(true, 1, true);
}
