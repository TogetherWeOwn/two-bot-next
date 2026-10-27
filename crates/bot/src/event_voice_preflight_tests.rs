//! Builder-owned orchestration tests: preflight must guard the entire live tick.
use super::*;
use crate::discord_test_common::{MockRest, RestRequest, ScriptedResponse};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use twilight_model::{channel::Channel, guild::Permissions};
use two_bot::voice_rooms::{BotAccess, GuildSnapshot, LiveGuild};

const GUILD: u64 = 100;
const CATEGORY: u64 = 200;

fn category() -> Value {
    json!({"id":"200", "guild_id":"100", "type":4, "name":"events", "permission_overwrites":[]})
}

pub(super) fn live() -> LiveGuild {
    let live = LiveGuild::new(GUILD);
    assert!(live.publish(GuildSnapshot {
        channels: vec![serde_json::from_value::<Channel>(category()).unwrap()],
        members: vec![], member_roles: HashMap::new(),
        bot: BotAccess {
            member_id: 999, guild_owner_id: 998, system_channel_id: None,
            member_roles: vec![],
            roles: vec![serde_json::from_value(json!({
                "id":"100", "name":"everyone", "color":0, "hoist":false,
                "managed":false, "mentionable":false, "position":0, "flags":0,
                "permissions":(Permissions::VIEW_CHANNEL | Permissions::MANAGE_CHANNELS).bits().to_string(),
                "colors":{"primary_color":0,"secondary_color":null,"tertiary_color":null}
            })).unwrap()],
        },
    }));
    live
}

fn rest(mock: &MockRest) -> ActionExecutor {
    crate::gateway::ensure_crypto_provider();
    ActionExecutor::with_proxy("synthetic-preflight-token".into(), Some(mock.origin())).unwrap()
}

fn settings(category: Option<u64>) -> event_voice_preflight::Settings {
    event_voice_preflight::Settings {
        category,
        staff_channel: Some("300".into()),
    }
}

async fn discord(category_status: u16, post_status: u16) -> MockRest {
    MockRest::with_responder(move |request: &RestRequest| {
        let path = request.path.strip_prefix("/api/v10").unwrap();
        match (request.method.as_str(), path) {
            ("GET", "/channels/200") => ScriptedResponse::json(category_status, category()),
            ("GET", "/users/@me") => {
                ScriptedResponse::json(200, json!({"id":"999","bot":true,"username":"bot"}))
            }
            ("GET", "/guilds/100") => {
                ScriptedResponse::json(200, json!({"id":"100","owner_id":"998"}))
            }
            ("GET", "/guilds/100/roles") => {
                ScriptedResponse::json(200, json!([{"id":"100","permissions":"3072"}]))
            }
            ("GET", "/guilds/100/members/999") => {
                ScriptedResponse::json(200, json!({"user":{"id":"999"},"roles":[]}))
            }
            ("GET", "/channels/300") => ScriptedResponse::json(
                200,
                json!({"id":"300","guild_id":"100","type":0,"permission_overwrites":[]}),
            ),
            ("POST", "/channels/300/messages") => {
                ScriptedResponse::json(post_status, json!({"id":"400"}))
            }
            _ => panic!("unexpected preflight route: {} {}", request.method, path),
        }
    })
    .await
}

#[tokio::test]
async fn held_tick_gate_skips_even_preflight_and_continuation() {
    let mock = discord(200, 201).await;
    let tick = event_voice_preflight::Tick::default();
    let _held = tick.gate.try_acquire().unwrap();
    let (_stop, shutdown) = watch::channel(false);
    let calls = AtomicUsize::new(0);
    tick.run(
        &rest(&mock),
        GUILD,
        Some(live()),
        settings(Some(CATEGORY)),
        &shutdown,
        |_| async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
    )
    .await
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
}

#[tokio::test]
async fn ready_tick_holds_single_flight_through_the_whole_continuation() {
    let mock = discord(200, 201).await;
    let rest = rest(&mock);
    let tick = event_voice_preflight::Tick::default();
    let (_stop, shutdown) = watch::channel(false);
    let calls = AtomicUsize::new(0);
    tick.run(
        &rest,
        GUILD,
        Some(live()),
        settings(Some(CATEGORY)),
        &shutdown,
        |parent| {
            assert_eq!(parent, CATEGORY);
            async {
                assert!(
                    tick.gate.try_acquire().is_none(),
                    "scan, claims and REST remain single-flight"
                );
                tick.run(
                    &rest,
                    GUILD,
                    Some(live()),
                    settings(Some(CATEGORY)),
                    &shutdown,
                    |_| async {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    },
                )
                .await?;
                Ok(())
            }
        },
    )
    .await
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        mock.requests().len(),
        1,
        "overlap must not even GET the category"
    );
    assert!(
        tick.gate.try_acquire().is_some(),
        "guard released after the continuation"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn missing_or_deleted_category_delivers_one_staff_alert_without_running_tick() {
    for configured in [None, Some(CATEGORY)] {
        let mock = discord(404, 201).await;
        let tick = event_voice_preflight::Tick::default();
        let (_stop, shutdown) = watch::channel(false);
        let calls = AtomicUsize::new(0);
        for _ in 0..2 {
            tick.run(
                &rest(&mock),
                GUILD,
                Some(live()),
                settings(configured),
                &shutdown,
                |_| async {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .await
            .unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let posts: Vec<_> = mock
            .requests()
            .into_iter()
            .filter(|r| r.method == "POST")
            .collect();
        assert_eq!(
            posts.len(),
            1,
            "one delivered alert per gap, not one per tick"
        );
        let body: Value = serde_json::from_slice(&posts[0].body).unwrap();
        assert!(body["content"].as_str().unwrap().contains("no_category"));
        assert_eq!(body["allowed_mentions"]["parse"], json!([]));
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn permission_denial_is_nonfatal_alerted_and_backed_off_even_if_post_fails() {
    let mock = discord(403, 403).await;
    let tick = event_voice_preflight::Tick::default();
    let (_stop, shutdown) = watch::channel(false);
    let calls = AtomicUsize::new(0);
    for _ in 0..2 {
        tick.run(
            &rest(&mock),
            GUILD,
            Some(live()),
            settings(Some(CATEGORY)),
            &shutdown,
            |_| async {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .await
        .unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let requests = mock.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.path.ends_with("/channels/200"))
            .count(),
        1
    );
    let posts: Vec<_> = requests.iter().filter(|r| r.method == "POST").collect();
    assert_eq!(
        posts.len(),
        1,
        "uncertain delivery must not create alert spam"
    );
    let body: Value = serde_json::from_slice(&posts[0].body).unwrap();
    assert!(body["content"].as_str().unwrap().contains("perm_denied"));
    mock.shutdown().await;
}

#[tokio::test]
async fn missing_live_state_never_authorizes_a_category_or_runs_the_tick() {
    let mock = discord(200, 201).await;
    let tick = event_voice_preflight::Tick::default();
    let (_stop, shutdown) = watch::channel(false);
    let calls = AtomicUsize::new(0);
    tick.run(
        &rest(&mock),
        GUILD,
        None,
        settings(Some(CATEGORY)),
        &shutdown,
        |_| async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
    )
    .await
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(
        mock.requests().is_empty(),
        "no live evidence is unavailable, not a guessed permission grant"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn shutdown_and_cancelled_ticks_release_the_gate_without_publishing() {
    let (mock, response) = MockRest::start_gated(
        vec![ScriptedResponse::json(200, category())],
        ScriptedResponse::json(200, category()),
    )
    .await;
    let rest = rest(&mock);
    let tick = event_voice_preflight::Tick::default();
    let (stop, shutdown) = watch::channel(false);
    let calls = AtomicUsize::new(0);
    let first = tick.run(
        &rest,
        GUILD,
        Some(live()),
        settings(Some(CATEGORY)),
        &shutdown,
        |_| async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
    );
    tokio::pin!(first);
    tokio::select! {
        result = &mut first => panic!("held GET unexpectedly finished: {result:?}"),
        () = response.wait_for_request() => {}
    }
    stop.send_replace(true);
    response.release();
    first.await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(tick.gate.try_acquire().is_some());
    mock.shutdown().await;
}
