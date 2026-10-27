//! Runtime category preflight: a fresh Discord document plus live guild roles,
//! never guild permissions alone or an assumed-empty overwrite list.

use crate::discord_test_common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Instant,
};
use twilight_model::{channel::Channel, guild::Permissions};
use two_bot::{
    event_voice_category::{
        CategoryReader, EventVoiceCategoryGate, EventVoiceTbdReason, LiveCategoryReader,
        PreflightDecision, StaffAlert, StaffAlertSink,
    },
    voice_rooms::{BotAccess, GuildSnapshot, LiveGuild},
};
use two_bot_discord::{voice_rooms::RoomHttpError, ActionExecutor};

const GUILD: u64 = 100;
const CATEGORY: u64 = 200;

fn document() -> Value {
    json!({"id":"200", "guild_id":"100", "type":4, "name":"events",
        "permission_overwrites":[]})
}

fn live(permissions: Permissions) -> LiveGuild {
    let live = LiveGuild::new(GUILD);
    assert!(live.publish(GuildSnapshot {
        channels: vec![serde_json::from_value::<Channel>(document()).unwrap()],
        members: vec![],
        member_roles: HashMap::new(),
        bot: BotAccess {
            member_id: 999,
            guild_owner_id: 998,
            system_channel_id: None,
            member_roles: vec![],
            roles: vec![serde_json::from_value(json!({
                "id":"100", "name":"everyone", "color":0, "hoist":false,
                "managed":false, "mentionable":false, "position":0, "flags":0,
                "permissions":permissions.bits().to_string(),
                "colors":{"primary_color":0,"secondary_color":null,"tertiary_color":null}
            }))
            .unwrap()],
        },
    }));
    live
}

fn executor(mock: &MockRest) -> ActionExecutor {
    crate::gateway::ensure_crypto_provider();
    ActionExecutor::with_proxy_and_guard(
        "synthetic-category-token".into(),
        Some(mock.origin()),
        Arc::new(
            two_bot_discord::ratelimit_guard::RateLimitGuard::new(
                two_bot_discord::ratelimit_guard::GuardConfig::default(),
            )
            .unwrap(),
        ),
    )
    .unwrap()
}

fn required() -> Permissions {
    Permissions::VIEW_CHANNEL | Permissions::MANAGE_CHANNELS
}

#[tokio::test]
async fn runtime_category_uses_fresh_overwrites_not_guild_grants_or_cached_category() {
    let mut denied = document();
    denied["permission_overwrites"] = json!([{
        "id":"100", "type":0, "allow":"0",
        "deny":Permissions::MANAGE_CHANNELS.bits().to_string()
    }]);
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, denied)],
        ScriptedResponse::json(200, document()),
    )
    .await;
    let rest = executor(&mock);
    let live = live(required());
    let reader = LiveCategoryReader::new(&rest, &live);
    let denied = reader.category_grant(GUILD, CATEGORY).await.unwrap();
    assert!(!denied
        .bot_permissions
        .unwrap()
        .contains(Permissions::MANAGE_CHANNELS));
    let repaired = reader.category_grant(GUILD, CATEGORY).await.unwrap();
    assert!(repaired.bot_permissions.unwrap().contains(required()));
    assert!(repaired.everyone_can_view);
    assert_eq!(mock.requests().len(), 2);
    for request in mock.requests() {
        assert_eq!(request.method, "GET");
        assert!(request.path.ends_with("/channels/200"));
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn runtime_category_requires_complete_live_identity_and_matching_guild() {
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, document())).await;
    let rest = executor(&mock);
    let disconnected = LiveGuild::new(GUILD);
    let reader = LiveCategoryReader::new(&rest, &disconnected);
    assert!(reader.category_grant(GUILD, CATEGORY).await.is_err());
    let connected = live(required());
    let reader = LiveCategoryReader::new(&rest, &connected);
    assert!(reader.category_grant(GUILD + 1, CATEGORY).await.is_err());
    assert!(
        mock.requests().is_empty(),
        "unavailable evidence must not fetch"
    );
    connected.disconnect();
    assert!(reader.category_grant(GUILD, CATEGORY).await.is_err());
    mock.shutdown().await;
}

#[tokio::test]
async fn runtime_category_disconnect_during_get_invalidates_the_observation() {
    let (mock, response) = MockRest::start_gated(
        vec![ScriptedResponse::json(200, document())],
        ScriptedResponse::json(200, document()),
    )
    .await;
    let rest = executor(&mock);
    let live = live(required());
    let reader = LiveCategoryReader::new(&rest, &live);
    let observation = reader.category_grant(GUILD, CATEGORY);
    let disconnect = async {
        response.wait_for_request().await;
        live.disconnect();
        response.release();
    };
    let (observed, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(observation, disconnect)
    })
    .await
    .unwrap();
    assert_eq!(observed, Err(RoomHttpError::Cancelled));
    assert_eq!(mock.requests().len(), 1);
    mock.shutdown().await;
}

#[tokio::test]
async fn runtime_category_rejects_wrong_ids_wrong_kinds_and_missing_overwrites() {
    for (key, value) in [
        ("id", json!("201")),
        ("guild_id", json!("101")),
        ("guild_id", Value::Null),
        ("type", json!(2)),
        ("permission_overwrites", Value::Null),
    ] {
        let mut bad = document();
        bad[key] = value;
        let mock = MockRest::start(vec![], ScriptedResponse::json(200, bad)).await;
        let rest = executor(&mock);
        let live = live(Permissions::ADMINISTRATOR);
        let reader = LiveCategoryReader::new(&rest, &live);
        assert!(
            reader.category_grant(GUILD, CATEGORY).await.is_err(),
            "{key}"
        );
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn runtime_category_retains_private_everyone_grant_separate_from_bot_member_grant() {
    let mut private = document();
    private["permission_overwrites"] = json!([
        {"id":"100", "type":0, "allow":"0", "deny":Permissions::VIEW_CHANNEL.bits().to_string()},
        {"id":"999", "type":1, "allow":Permissions::VIEW_CHANNEL.bits().to_string(), "deny":"0"}
    ]);
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, private)).await;
    let rest = executor(&mock);
    let live = live(required());
    let snapshot = LiveCategoryReader::new(&rest, &live)
        .category_grant(GUILD, CATEGORY)
        .await
        .unwrap();
    assert!(snapshot.bot_permissions.unwrap().contains(required()));
    assert!(!snapshot.everyone_can_view);
    mock.shutdown().await;
}

#[derive(Default)]
struct Alerts(Mutex<Vec<StaffAlert>>);
impl StaffAlertSink for Alerts {
    fn send_staff_alert(&self, alert: StaffAlert) {
        self.0.lock().unwrap().push(alert);
    }
}

#[tokio::test]
async fn runtime_category_http_failures_are_typed_single_attempt_and_gate_backed_off() {
    for (status, expected) in [
        (403, EventVoiceTbdReason::PermDenied),
        (404, EventVoiceTbdReason::NoCategory),
        (429, EventVoiceTbdReason::RateLimited),
        (500, EventVoiceTbdReason::Unavailable),
        (401, EventVoiceTbdReason::Unavailable),
    ] {
        let mock = MockRest::start(
            vec![],
            ScriptedResponse::json(status, json!({"retry_after":180})),
        )
        .await;
        let rest = executor(&mock);
        let live = live(required());
        let reader = LiveCategoryReader::new(&rest, &live);
        let alerts = Alerts::default();
        let mut gate = EventVoiceCategoryGate::new(Some(CATEGORY));
        let now = Instant::now();
        for _ in 0..2 {
            assert_eq!(
                gate.preflight(&reader, &alerts, GUILD, now).await,
                PreflightDecision::Waiting { reason: expected }
            );
        }
        assert_eq!(mock.requests().len(), 1, "no hidden HTTP retry ({status})");
        let expected_alerts = usize::from(matches!(status, 403 | 404));
        assert_eq!(alerts.0.lock().unwrap().len(), expected_alerts);
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn runtime_category_preserves_retry_after_and_missing_channel_status() {
    for (status, expected) in [
        (
            429,
            RoomHttpError::RateLimited {
                retry_after_ms: 180_000,
                global: false,
            },
        ),
        (404, RoomHttpError::NotFound),
        (403, RoomHttpError::AccessDenied),
    ] {
        let mock = MockRest::start(
            vec![],
            ScriptedResponse::json(status, json!({"retry_after":180})),
        )
        .await;
        assert_eq!(
            executor(&mock)
                .read_event_voice_channel(GUILD, CATEGORY)
                .await,
            Err(expected)
        );
        mock.shutdown().await;
    }
}
