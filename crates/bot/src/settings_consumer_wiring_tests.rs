#![cfg(test)]

//! Each probe owns a process-global production reader in a fresh subprocess.
//! No test replaces a runtime reader or relies on another test's publication.
use super::*;
use crate::{
    activation::fixtures,
    feed_jobs,
    jobs::JobAction,
    self_role_handlers::{self, SelfRoleResult, SelfRoleService},
    self_role_runtime::SelfRoleRuntime,
};
use serde_json::json;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::sync::atomic::{AtomicUsize, Ordering};
use twilight_model::id::Id;
use two_bot_core::{
    self_roles::{parse_self_role_panels, SelfRoleGates, SelfRolePanel},
    settings::SettingRow,
    FeatureGates,
};
use two_bot_cutover::self_role_store::SelfRoleStore;
use two_bot_discord::ActionExecutor;

async fn isolated(test: &'static str) -> bool {
    const CHILD: &str = "LIVE_CONSUMER_PROBE_MODE";
    if std::env::var(CHILD).as_deref() == Ok(test) {
        return false;
    }
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([test, "--exact", "--nocapture"])
            .env(CHILD, test)
            // Inherit LLVM_PROFILE_FILE so the child contributes coverage.
            .output()
            .expect("isolated live consumer probe")
    })
    .await
    .expect("child task");
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "isolated consumer probe failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    true
}

fn install_live() -> LiveSettingsWriter {
    let (writer, live) = live_channel();
    assert!(
        LIVE.set(live).is_ok(),
        "fresh child owns the production reader"
    );
    writer
}

fn publish(
    writer: &mut LiveSettingsWriter,
    revision: i64,
    guild: &str,
    key: &str,
    value: Option<serde_json::Value>,
) {
    writer.publish(&SettingsSnapshot {
        revision,
        rows: value
            .into_iter()
            .map(|value| SettingRow {
                guild_id: guild.to_owned(),
                key: key.to_owned(),
                value,
                version: revision,
            })
            .collect(),
    });
}

#[tokio::test(start_paused = true)]
async fn fenced_feed_registration_follows_live_announcements_across_ticks() {
    if isolated("settings_jobs::consumer_wiring_tests::fenced_feed_registration_follows_live_announcements_across_ticks").await {
        return;
    }
    let mut writer = install_live();
    let guild = two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID;
    let gates = FeatureGates::from_map(&std::collections::HashMap::from([
        ("TWO_ANNOUNCEMENTS".to_owned(), "1".to_owned()),
        ("TWO_FEED_POLL_SECONDS".to_owned(), "60".to_owned()),
    ]))
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let action: JobAction = Arc::new({
        let calls = Arc::clone(&calls);
        move || {
            calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    });
    // This is the production registration chain, not the injectable gate helper.
    let job =
        feed_jobs::register_fenced(gates, &fixtures::staging(), action, guild.to_owned()).unwrap();
    for (revision, stored, expected) in [
        (1, Some(json!(1)), 1),
        (2, Some(json!(0)), 1),
        (3, Some(json!(1)), 2),
        (4, None, 3),
    ] {
        publish(&mut writer, revision, guild, "TWO_ANNOUNCEMENTS", stored);
        (job.action)()
            .await
            .expect("tick succeeds even while disabled");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            expected,
            "publication {revision}"
        );
        tokio::time::advance(Duration::from_secs(60)).await;
    }
}

fn panels(message: &str, reaction_message: &str) -> Vec<SelfRolePanel> {
    parse_self_role_panels(&json!([
        {
            "id":"games", "channelId":"100000000000000007", "messageId":message,
            "mode":"button", "exclusive":true,
            "options":[{"key":"new", "label":"New", "roleId":"100000000000000004", "permissions":"0"}]
        },
        {
            "id":"reaction-games", "channelId":"100000000000000007", "messageId":reaction_message,
            "mode":"reaction", "exclusive":false,
            "options":[{"key":"new", "label":"New", "roleId":"100000000000000005", "permissions":"0", "emoji":"b"}]
        }
    ]).to_string()).unwrap()
}

fn catalogue(panels: &[SelfRolePanel]) -> serde_json::Value {
    // The parser's public input shape is camelCase; the model is not a wire DTO.
    serde_json::Value::Array(
        panels
            .iter()
            .map(|panel| {
                json!({
                    "id":panel.id, "channelId":panel.channel_id, "messageId":panel.message_id,
                    "mode":panel.mode.as_str(), "exclusive":panel.exclusive,
                    "options":panel.options.iter().map(|option| {
                        let mut value = json!({
                            "key":option.key, "label":option.label, "roleId":option.role_id,
                            "permissions":option.permissions,
                        });
                        if let Some(emoji) = &option.emoji {
                            value["emoji"] = json!(emoji);
                        }
                        value
                    }).collect::<Vec<_>>(),
                })
            })
            .collect(),
    )
}

#[tokio::test]
async fn self_role_entrypoints_follow_live_replacement_removal_and_delete() {
    if isolated("settings_jobs::consumer_wiring_tests::self_role_entrypoints_follow_live_replacement_removal_and_delete").await {
        return;
    }
    let mut writer = install_live();
    let guild = self_role_handlers::tests::GUILD;
    let boot = panels("100000000000000008", "100000000000000009");
    let live = panels("100000000000000018", "100000000000000019");
    let options = PgConnectOptions::new()
        .host("agent-testdb")
        .port(5432)
        .username("agent_test")
        .password("")
        .database("agent_test");
    let service = SelfRoleService::new(
        SelfRoleRuntime {
            store: SelfRoleStore::new(
                PgPoolOptions::new()
                    .acquire_timeout(Duration::from_millis(50))
                    .connect_lazy_with(options),
            ),
            executor: ActionExecutor::with_proxy(
                "fixture-token".into(),
                Some("http://127.0.0.1:9".into()),
            )
            .unwrap(),
            guild_id: guild.to_string(),
            bot_id: "100000000000000003".to_owned(),
        },
        SelfRoleGates {
            panels: boot.clone(),
            dry_run: false,
        },
        &[guild.to_string()].into_iter().collect(),
    )
    .unwrap();
    let old_press = self_role_handlers::tests::component(&boot[0], 100000000000000100, vec![]);
    let new_press = self_role_handlers::tests::component(&live[0], 100000000000000101, vec![]);
    let old_input = service.component_input(&old_press).unwrap();
    let mut old_reaction = self_role_handlers::tests::reaction();
    old_reaction.message_id = Id::new(boot[1].message_id.parse().unwrap());
    let mut new_reaction = old_reaction.clone();
    new_reaction.message_id = Id::new(live[1].message_id.parse().unwrap());

    publish(
        &mut writer,
        1,
        &guild.to_string(),
        self_role_handlers::PANELS_KEY,
        Some(catalogue(&live)),
    );
    assert!(service.component_input(&old_press).is_none());
    assert!(service.stale_component(&old_press));
    let live_input = service
        .component_input(&new_press)
        .expect("replacement source is live");
    assert_eq!(live_input.panel, live[0]);
    assert!(!service.stale_component(&new_press));
    assert!(service.reaction_input(&old_reaction, false).is_none());
    assert_eq!(
        service.reaction_input(&new_reaction, false).unwrap().panel,
        live[1]
    );
    assert_eq!(service.handle(&old_input).await, SelfRoleResult::Ignored);

    publish(
        &mut writer,
        2,
        &guild.to_string(),
        self_role_handlers::PANELS_KEY,
        Some(json!([])),
    );
    assert!(service.component_input(&new_press).is_none());
    assert!(service.stale_component(&new_press));
    assert!(service.reaction_input(&new_reaction, true).is_none());
    assert_eq!(service.handle(&live_input).await, SelfRoleResult::Ignored);
    assert_eq!(
        service
            .recover_once()
            .await
            .expect("an empty live catalogue does no store I/O"),
        0,
    );

    publish(
        &mut writer,
        3,
        &guild.to_string(),
        self_role_handlers::PANELS_KEY,
        None,
    );
    assert_eq!(service.component_input(&old_press).unwrap().panel, boot[0]);
    assert!(!service.stale_component(&old_press));
    assert!(service.stale_component(&new_press));
    assert!(service.reaction_input(&old_reaction, false).is_some());
    assert!(service.reaction_input(&new_reaction, false).is_none());
}
