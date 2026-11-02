//! Protected real shared-command acceptance. REST is loopback; the closed pool
//! makes any new help store dependency fail rather than silently use a database.
#![cfg(test)]

use super::*;
use two_bot_core::{CommandDefinition, CustomCommand};

async fn closed_runtime(
    router_gates: RouterGates,
    origin: String,
    custom: Vec<CustomCommand>,
) -> Arc<CommandRuntime> {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("lazy pool never connects");
    pool.close().await;
    let executor =
        ActionExecutor::with_proxy("test-token".into(), Some(origin)).expect("loopback executor");
    let leveling = two_bot_discord::LevelingRuntime::new(
        pool.clone(),
        Arc::new(executor.clone()),
        GUILD,
        two_bot_core::OnboardingGates {
            mode: two_bot_core::OnboardingMode::Legacy,
            dry_run: false,
        },
    );
    let runtime = CommandRuntime::build(
        pool,
        executor,
        router_with_commands(router_gates),
        None,
        leveling,
        GUILD,
        Some(custom),
        None,
        None,
    );
    runtime.set_identity(1111, 1111);
    runtime
}

fn invocation(selector: Option<&str>) -> Interaction {
    let options = selector
        .map(|name| vec![option("command", CommandOptionValue::String(name.into()))])
        .unwrap_or_default();
    let mut interaction = slash("help", Some(CHANNEL), options);
    interaction.member.as_mut().expect("member").permissions = Some(Permissions::empty());
    interaction
}

fn utf16(text: &str) -> usize {
    text.encode_utf16().count()
}

fn message_text(data: &serde_json::Value) -> String {
    let mut sections = vec![];
    if let Some(content) = data["content"].as_str() {
        sections.push(content.to_owned());
    }
    if let Some(embeds) = data["embeds"].as_array() {
        assert!(embeds.len() <= 10);
        let mut units = 0;
        for embed in embeds {
            for (key, bound) in [("title", 256), ("description", 4096)] {
                if let Some(text) = embed[key].as_str() {
                    assert!(utf16(text) <= bound, "embed {key} limit");
                    units += utf16(text);
                    sections.push(text.to_owned());
                }
            }
            if let Some(fields) = embed["fields"].as_array() {
                assert!(fields.len() <= 25);
                for field in fields {
                    for (key, bound) in [("name", 256), ("value", 1024)] {
                        let text = field[key].as_str().expect("field text");
                        assert!(utf16(text) <= bound);
                        units += utf16(text);
                        sections.push(text.into());
                    }
                }
            }
            for (key, field, bound) in [("footer", "text", 2048), ("author", "name", 256)] {
                if let Some(text) = embed[key][field].as_str() {
                    assert!(utf16(text) <= bound);
                    units += utf16(text);
                    sections.push(text.into());
                }
            }
            for key in ["url", "image", "thumbnail", "video", "provider"] {
                assert!(
                    embed.get(key).is_none() || embed[key].is_null(),
                    "help must not activate untrusted external media/links"
                );
            }
        }
        assert!(units <= 6000, "aggregate embed text limit");
    }
    assert!(utf16(data["content"].as_str().unwrap_or("")) <= 2000);
    sections.join("\n")
}

async fn answer(runtime: &Arc<CommandRuntime>, mock: &MockRest, input: Interaction) -> String {
    let before = mock.requests().len();
    tokio::time::timeout(Duration::from_millis(750), runtime.on_interaction(&input))
        .await
        .expect("help cannot wait on a store or remote discovery");
    let requests = mock.requests();
    assert_eq!(requests.len(), before + 1, "only the initial help callback: no GET, republish, execution, audit REST, edit or followup");
    let callback = &requests[before];
    assert_eq!(callback.method, "POST");
    assert!(callback.path.ends_with("/callback"));
    let reply: serde_json::Value = serde_json::from_slice(&callback.body).expect("reply JSON");
    assert_eq!(
        reply["type"], 4,
        "immediate complete acknowledgement, never defer"
    );
    assert_eq!(reply["data"]["flags"], 64, "private help");
    assert_eq!(
        reply["data"]["allowed_mentions"]["parse"],
        serde_json::json!([])
    );
    for key in ["users", "roles"] {
        assert!(reply["data"]["allowed_mentions"][key]
            .as_array()
            .is_none_or(Vec::is_empty));
    }
    message_text(&reply["data"])
}

fn contains(text: &str, tokens: &[&str]) {
    for token in tokens {
        assert!(
            text.to_lowercase().contains(&token.to_lowercase()),
            "missing {token:?}: {text}"
        );
    }
}

async fn published(
    router_gates: RouterGates,
    custom: Vec<CustomCommand>,
) -> (MockRest, Arc<CommandRuntime>) {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = closed_runtime(router_gates, origin, custom).await;
    runtime
        .publish_registry_checked(Some(1111))
        .await
        .expect("confirmed fixture publication");
    assert!(runtime.published_commands().is_some());
    (mock, runtime)
}

#[tokio::test]
async fn control_omitted_selector_preserves_grouped_index_and_order() {
    let (mock, runtime) = published(gates(true, true), vec![]).await;
    let text = answer(&runtime, &mock, invocation(None)).await;
    let defs = runtime.published_commands().expect("confirmed snapshot");
    assert_eq!(text, two_bot_core::help_text(&defs));
    for group in [
        "Leveling",
        "Discovery",
        "Privacy",
        "Automations",
        "RSVPs",
        "Looking for group",
        "Feed relays",
    ] {
        assert!(text.contains(group));
    }
    assert!(text.find("**Leveling**") < text.find("**Discovery**"));
    assert!(text.find("**Discovery**") < text.find("**Privacy**"));
    contains(&text, &["last confirmed", "picker"]);
    assert!(!text.contains("/templateassistant"));
    mock.shutdown().await;
}

#[tokio::test]
async fn selected_rank_is_detail_not_the_full_index_or_command_execution() {
    let (mock, runtime) = published(gates(false, false), vec![]).await;
    let text = answer(&runtime, &mock, invocation(Some("rank"))).await;
    contains(
        &text,
        &[
            "/rank",
            "Show your XP, level and server rank.",
            "usage",
            "member",
            "optional",
            "user",
        ],
    );
    assert!(
        !text.contains("**Server commands**"),
        "detail replaces index"
    );
    assert!(!text.contains("**Privacy**"), "no unrelated command list");
    mock.shutdown().await;
}

#[tokio::test]
async fn help_detail_itself_explains_optional_selection_and_examples() {
    let (mock, runtime) = published(gates(false, false), vec![]).await;
    let text = answer(&runtime, &mock, invocation(Some("help"))).await;
    contains(
        &text,
        &["/help", "command", "optional", "string", "command:rank"],
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn selector_canonical_rule_trims_ascii_case_and_one_leading_slash() {
    let (mock, runtime) = published(gates(false, false), vec![]).await;
    for selector in ["rank", "RANK", "/rank", "  /RaNk  "] {
        let text = answer(&runtime, &mock, invocation(Some(selector))).await;
        contains(&text, &["/rank", "Show your XP, level and server rank."]);
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn unknown_unpublished_prefix_alias_and_hostile_inputs_get_confirmed_index_correction() {
    let (mock, runtime) = published(gates(false, false), vec![]).await;
    for selector in [
        "does-not-exist",
        "ban",
        "templateassistant",
        "ran",
        "xp",
        "",
        "//rank",
        "@everyone <@123> https://invalid.example/x",
    ] {
        let text = answer(&runtime, &mock, invocation(Some(selector))).await;
        contains(&text, &["/help", "confirmed"]);
        assert!(
            ["unknown", "not published", "not found", "no confirmed"]
                .iter()
                .any(|term| text.to_lowercase().contains(term)),
            "concise unknown/unpublished correction: {text}"
        );
        assert!(!text.contains("Ban a"));
        assert!(
            !text.contains("templateassistant"),
            "never advertise unpublished name"
        );
        assert!(!text.contains("https://invalid.example"));
        assert!(!text.contains("<@123>"));
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn selected_gated_command_explains_requirement_without_authorizing_the_member() {
    let (mock, runtime) = published(
        RouterGates {
            moderation: true,
            ..gates(false, false)
        },
        vec![],
    )
    .await;
    let text = answer(&runtime, &mock, invocation(Some("ban"))).await;
    contains(
        &text,
        &[
            "Ban Members",
            "target",
            "required",
            "reason",
            "512",
            "picker",
        ],
    );
    assert!(!text.to_lowercase().contains("you can use this"));
    assert!(
        !text.contains("You need the Ban Members permission"),
        "explain published definition, do not execute/refuse the selected verb"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn selected_privacy_subcommands_explain_arguments_without_erasing_anything() {
    let (mock, runtime) = published(gates(false, false), vec![]).await;
    let text = answer(&runtime, &mock, invocation(Some("privacy"))).await;
    contains(
        &text,
        &[
            "/privacy deletion-status",
            "request-id",
            "required",
            "128",
            "/privacy opt-out",
            "/privacy opt-in",
        ],
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn selected_schedule_reads_requiredness_choices_and_bounds_from_snapshot() {
    let (mock, runtime) = published(gates(true, false), vec![]).await;
    let text = answer(&runtime, &mock, invocation(Some("schedule"))).await;
    contains(
        &text,
        &[
            "usage",
            "body",
            "required",
            "in-minutes",
            "integer",
            "525600",
            "every-minutes",
            "optional",
        ],
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn custom_detail_uses_confirmed_description_and_never_runs_its_template() {
    let (mock, runtime) = published(
        gates(true, false),
        vec![CustomCommand {
            name: "faq".into(),
            description: "Confirmed FAQ revision zeta".into(),
            enabled: true,
        }],
    )
    .await;
    let text = answer(&runtime, &mock, invocation(Some("faq"))).await;
    contains(&text, &["/faq", "Confirmed FAQ revision zeta", "usage"]);
    assert!(!text.contains("**Server commands**"));
    mock.shutdown().await;
}

#[tokio::test]
async fn hostile_custom_description_is_inert_and_all_mentions_are_suppressed() {
    let hostile =
        "FAQ @everyone <@123> <@&456> <#789> [click](https://invalid.example/x) **bold** `code`";
    let (mock, runtime) = published(
        gates(true, false),
        vec![CustomCommand {
            name: "faq".into(),
            description: hostile.into(),
            enabled: true,
        }],
    )
    .await;
    let text = answer(&runtime, &mock, invocation(Some("faq"))).await;
    contains(&text, &["FAQ", "bold", "code"]);
    for raw in [
        "<@123>",
        "<@&456>",
        "<#789>",
        "[click](https://invalid.example/x)",
        "**bold**",
        "`code`",
    ] {
        assert!(
            !text.contains(raw),
            "inert custom description, not active markup: {text}"
        );
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn maximum_registry_keeps_index_names_and_detail_is_bounded_deterministic() {
    let base = router_with_commands(gates(true, false))
        .publish_set(&[])
        .expect("base registry")
        .len();
    let custom = (0..100 - base)
        .map(|n| CustomCommand {
            name: format!("fixture{n:03}"),
            description: format!("Description {n:03} {}", "😀".repeat(45)),
            enabled: true,
        })
        .collect();
    let (mock, runtime) = published(gates(true, false), custom).await;
    let index = answer(&runtime, &mock, invocation(None)).await;
    for n in 0..100 - base {
        assert!(index.contains(&format!("/fixture{n:03}")));
    }
    let a = answer(&runtime, &mock, invocation(Some("fixture000"))).await;
    let b = answer(&runtime, &mock, invocation(Some("fixture000"))).await;
    assert_eq!(a, b);
    contains(&a, &["Description 000", "/fixture000"]);
    mock.shutdown().await;
}

#[tokio::test]
async fn control_unconfirmed_snapshot_never_explains_compiled_definitions() {
    let (mock, origin) = MockRest::start(vec![]).await;
    let runtime = closed_runtime(gates(true, true), origin, vec![]).await;
    for input in [
        invocation(None),
        invocation(Some("rank")),
        invocation(Some("ban")),
    ] {
        let text = answer(&runtime, &mock, input).await;
        assert_eq!(
            text,
            "The command list is still refreshing. Try /help again shortly."
        );
    }
    assert!(runtime.published_commands().is_none());
    mock.shutdown().await;
}

#[tokio::test]
async fn control_failed_refresh_retains_old_confirmed_arc_and_index() {
    let (mock, origin) = MockRest::start(vec![200, 403, 200]).await;
    let runtime = closed_runtime(gates(false, false), origin, vec![]).await;
    runtime
        .publish_registry_checked(Some(1111))
        .await
        .expect("first publish");
    let old = runtime.published_commands().expect("confirmed");
    assert_eq!(
        runtime.publish_registry_checked(Some(1111)).await,
        Err(RegistrySyncError::Publish)
    );
    assert!(Arc::ptr_eq(
        &old,
        &runtime.published_commands().expect("still confirmed")
    ));
    let text = answer(&runtime, &mock, invocation(None)).await;
    assert_eq!(text, two_bot_core::help_text(&old));
    mock.shutdown().await;
}

#[tokio::test]
async fn control_wrong_guild_or_dm_cannot_discover_details() {
    let (mock, runtime) = published(gates(false, false), vec![]).await;
    let before = mock.requests().len();
    let mut wrong = invocation(Some("rank"));
    wrong.guild_id = Some(Id::new(9999));
    runtime.on_interaction(&wrong).await;
    let mut dm = invocation(Some("rank"));
    dm.guild_id = None;
    dm.member = None;
    runtime.on_interaction(&dm).await;
    assert_eq!(
        mock.requests().len(),
        before,
        "existing guild availability fence"
    );
    mock.shutdown().await;
}

#[test]
fn control_empty_confirmed_core_index_does_not_invent_names() {
    let text = two_bot_core::help_text(&Vec::<CommandDefinition>::new());
    assert!(text.contains("0 live"));
    assert!(!text.contains("/rank"));
    assert!(!text.contains("/templateassistant"));
}
