//! Protected acceptance: published announcements settings must reach the real
//! CommandRuntime::on_interaction entry, not on_interaction_with_live.
//! Mutant: remove/replace the production entry's live reader with None.
//!
//! Builder seam: instance-local set_live_settings(&self, LiveSettings). The
//! no-op baseline adapter leaves the real entry boot-only; inherent methods
//! replace it. No environment mutation or process-global settings installation.

#![cfg(test)]

use super::*;
use serde_json::{json, Value};
use two_bot_core::settings::LiveSettings;

trait BaselineCommandSettings {
    fn set_live_settings(&self, live: LiveSettings);
}
impl BaselineCommandSettings for CommandRuntime {
    fn set_live_settings(&self, _live: LiveSettings) {}
}

async fn fixture(boot: bool) -> (Arc<CommandRuntime>, MockRest) {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let options = PgConnectOptions::new()
        .host("agent-testdb")
        .port(5432)
        .username("agent_test")
        .password("")
        .database("two_bot_test_live_settings_acceptance");
    let pool = PgPoolOptions::new().connect_lazy_with(options);
    pool.close().await;
    let executor = ActionExecutor::with_proxy("fixture-token".into(), Some(origin))
        .expect("loopback executor");
    (
        CommandRuntime::new(
            pool,
            executor,
            router_with_commands(gates(true, boot)),
            GUILD,
            true,
        ),
        mock,
    )
}

#[tokio::test]
async fn real_feed_dispatch_observes_off_on_delete_and_foreign_snapshots() {
    let (runtime, mock) = fixture(true).await;
    let (mut writer, live) = live_channel();
    runtime.set_live_settings(live);
    let mut observed = Vec::new();
    for (revision, guild, value) in [
        (1, GUILD_S, Some(json!(0))),
        (2, GUILD_S, Some(json!(1))),
        (3, GUILD_S, None),
        (4, GUILD_S, Some(json!(0))),
        (5, "9999", Some(json!(0))),
    ] {
        writer.publish(&SettingsSnapshot {
            revision,
            rows: value
                .map(|value| SettingRow {
                    guild_id: guild.into(),
                    key: ANNOUNCEMENTS_KEY.into(),
                    value,
                    version: revision,
                })
                .into_iter()
                .collect(),
        });
        for name in ["feed-add", "feed-remove", "feed-list"] {
            let before = mock.requests().len();
            let mut command = slash(name, Some(CHANNEL), Vec::new());
            command.id = Id::new(10_000 + revision as u64 * 10 + observed.len() as u64);
            runtime.on_interaction(&command).await;
            let requests = mock.requests();
            let emitted = &requests[before..];
            let callbacks: Vec<_> = emitted
                .iter()
                .filter(|r| r.path.ends_with("/callback"))
                .collect();
            assert_eq!(
                callbacks.len(),
                1,
                "revision {revision}, {name}: exactly one initial reply"
            );
            let body: Value = serde_json::from_slice(&callbacks[0].body).expect("callback JSON");
            let disabled =
                body["type"] == 4 && body["data"]["content"] == ANNOUNCEMENTS_DISABLED_REPLY;
            if disabled {
                assert_eq!(
                    emitted.len(),
                    1,
                    "disabled: no defer, completion, or other REST effect"
                );
                assert_eq!(body["data"]["flags"], 64);
                assert_eq!(body["data"]["allowed_mentions"]["parse"], json!([]));
            } else {
                assert_eq!(body["type"], 5, "enabled consumer reaches defer");
                assert_eq!(body["data"]["flags"], 64);
            }
            observed.push(disabled);
        }
    }
    assert_eq!(
        observed,
        vec![
            true, true, true, false, false, false, false, false, false, true, true, true, false,
            false, false,
        ],
        "all three real feed commands must see each settings publication"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn real_feed_dispatch_never_enables_a_boot_disabled_surface() {
    let (runtime, mock) = fixture(false).await;
    let (mut writer, live) = live_channel();
    runtime.set_live_settings(live);
    writer.publish(&SettingsSnapshot {
        revision: 1,
        rows: vec![announcements_row(GUILD_S, json!(1))],
    });
    runtime
        .on_interaction(&slash("feed-list", Some(CHANNEL), Vec::new()))
        .await;
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    let body: Value = serde_json::from_slice(&requests[0].body).expect("callback JSON");
    assert_eq!(body["type"], 4);
    assert_eq!(body["data"]["content"], ANNOUNCEMENTS_DISABLED_REPLY);
    mock.shutdown().await;
}

#[tokio::test]
async fn real_feed_dispatch_keeps_foreign_interactions_silent() {
    let (runtime, mock) = fixture(true).await;
    let (_writer, live) = live_channel();
    runtime.set_live_settings(live);
    let mut foreign = slash("feed-list", Some(CHANNEL), Vec::new());
    foreign.guild_id = Some(Id::new(GUILD + 1));
    runtime.on_interaction(&foreign).await;
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
}

#[tokio::test]
async fn explicit_baseline_command_adapter_keeps_boot_dispatch_callable() {
    let (runtime, mock) = fixture(true).await;
    let (_writer, live) = live_channel();
    <CommandRuntime as BaselineCommandSettings>::set_live_settings(&runtime, live);
    runtime
        .on_interaction(&slash("feed-add", Some(CHANNEL), Vec::new()))
        .await;
    let callbacks = mock.posts_to("/callback").await;
    assert_eq!(callbacks.len(), 1);
    let body: Value = serde_json::from_slice(&callbacks[0].body).expect("callback JSON");
    assert_eq!(body["type"], 5);
    mock.shutdown().await;
}
