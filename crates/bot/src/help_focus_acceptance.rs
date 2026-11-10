//! Protected focused-branch shared-command acceptance. REST is loopback; the
//! closed pool makes any new help store dependency fail rather than silently
//! use a database. No implementation belongs here: the old runtime ignores the
//! `subcommand` selector and repeats the whole-command detail (or the index),
//! so every focused assertion below fails until the Builder wires the new
//! selector through strict decoding, snapshot branch resolution and the
//! focused renderer.
#![cfg(test)]

use super::*;
use two_bot_core::CustomCommand;

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

fn invocation2(root: Option<&str>, path: Option<&str>) -> Interaction {
    let mut options = vec![];
    if let Some(name) = root {
        options.push(option("command", CommandOptionValue::String(name.into())));
    }
    if let Some(path) = path {
        options.push(option(
            "subcommand",
            CommandOptionValue::String(path.into()),
        ));
    }
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

fn correction(text: &str) {
    contains(text, &["/help", "confirmed"]);
    assert!(
        ["unknown", "not published", "not found", "no confirmed"]
            .iter()
            .any(|term| text.to_lowercase().contains(term)),
        "concise unknown/unpublished correction: {text}"
    );
}

fn focused_privacy_deletion_status(text: &str) {
    contains(
        text,
        &[
            "/privacy deletion-status",
            "status of your deletion request",
            "Usage: /privacy deletion-status <request-id>",
            "request-id",
            "required",
            "128",
        ],
    );
    let first = text.lines().next().expect("header line");
    assert!(
        first.contains("deletion-status"),
        "header names the focused branch: {text}"
    );
    for sibling in ["/privacy opt-out", "/privacy opt-in", "/privacy status"] {
        assert!(
            !text.contains(sibling),
            "sibling branches stay out of the focused reply: {sibling}: {text}"
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
async fn control_omitted_subcommand_preserves_root_detail_and_index() {
    let (mock, runtime) = published(gates(false, false), vec![]).await;
    let detail = answer(&runtime, &mock, invocation2(Some("rank"), None)).await;
    contains(
        &detail,
        &[
            "/rank",
            "Show your XP, level and server rank.",
            "usage",
            "member",
        ],
    );
    let index = answer(&runtime, &mock, invocation2(None, None)).await;
    let defs = runtime.published_commands().expect("confirmed snapshot");
    assert_eq!(index, two_bot_core::help_text(&defs));
    mock.shutdown().await;
}

#[tokio::test]
async fn focused_privacy_branch_shows_only_its_arguments() {
    let (mock, runtime) = published(gates(false, false), vec![]).await;
    let text = answer(
        &runtime,
        &mock,
        invocation2(Some("privacy"), Some("deletion-status")),
    )
    .await;
    focused_privacy_deletion_status(&text);
    assert!(
        !text.contains("**Server commands**"),
        "focused detail replaces the index"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn old_behavior_repeats_root_detail_when_subcommand_is_present() {
    let (mock, runtime) = published(gates(false, false), vec![]).await;
    let root = answer(&runtime, &mock, invocation2(Some("privacy"), None)).await;
    let focused = answer(
        &runtime,
        &mock,
        invocation2(Some("privacy"), Some("deletion-status")),
    )
    .await;
    assert_ne!(
        focused, root,
        "a focused branch is not the whole-command detail"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn subcommand_without_root_gets_correction() {
    let (mock, runtime) = published(gates(false, false), vec![]).await;
    let text = answer(&runtime, &mock, invocation2(None, Some("deletion-status"))).await;
    correction(&text);
    mock.shutdown().await;
}

#[tokio::test]
async fn unknown_prefix_overdeep_and_hostile_paths_get_correction_without_echo() {
    let (mock, runtime) = published(gates(false, false), vec![]).await;
    for path in [
        "does-not-exist",
        "opt",
        "deletion-status request-id extra",
        "privacy",
        "@everyone <@123> https://invalid.example/x",
        "",
    ] {
        let text = answer(&runtime, &mock, invocation2(Some("privacy"), Some(path))).await;
        correction(&text);
        assert!(!text.contains("<@123>"), "never echo hostile input: {text}");
        assert!(
            !text.contains("https://invalid.example"),
            "never echo hostile input: {text}"
        );
        assert!(
            !text.contains("templateassistant"),
            "never advertise unpublished names: {text}"
        );
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn duplicate_and_wrong_typed_subcommand_selectors_get_correction() {
    let (mock, runtime) = published(gates(false, false), vec![]).await;
    for value in [
        CommandOptionValue::Integer(1),
        CommandOptionValue::Number(1.5),
        CommandOptionValue::Boolean(true),
    ] {
        let mut interaction = slash(
            "help",
            Some(CHANNEL),
            vec![
                option("command", CommandOptionValue::String("privacy".into())),
                option("subcommand", value),
            ],
        );
        interaction.member.as_mut().expect("member").permissions = Some(Permissions::empty());
        correction(&answer(&runtime, &mock, interaction).await);
    }
    // A duplicated selector is malformed even when one copy is valid.
    for paths in [
        vec!["deletion-status", "deletion-status"],
        vec!["deletion-status", "does-not-exist"],
    ] {
        let mut options = vec![option(
            "command",
            CommandOptionValue::String("privacy".into()),
        )];
        for path in paths {
            options.push(option(
                "subcommand",
                CommandOptionValue::String(path.into()),
            ));
        }
        let mut interaction = slash("help", Some(CHANNEL), options);
        interaction.member.as_mut().expect("member").permissions = Some(Permissions::empty());
        correction(&answer(&runtime, &mock, interaction).await);
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn subcommand_on_branchless_or_custom_root_gets_correction() {
    let (mock, runtime) = published(
        gates(true, false),
        vec![CustomCommand {
            name: "faq".into(),
            description: "Confirmed FAQ revision zeta".into(),
            enabled: true,
        }],
    )
    .await;
    for (root, path) in [
        ("rank", "member"),
        ("rank", "does-not-exist"),
        ("faq", "anything"),
    ] {
        let text = answer(&runtime, &mock, invocation2(Some(root), Some(path))).await;
        correction(&text);
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn focused_path_canonicalization_matches_root_rule() {
    let (mock, runtime) = published(gates(false, false), vec![]).await;
    for path in ["DELETION-STATUS", "  deletion-status  "] {
        let text = answer(&runtime, &mock, invocation2(Some("privacy"), Some(path))).await;
        focused_privacy_deletion_status(&text);
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn hostile_custom_definition_stays_inert_under_focused_request() {
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
    let text = answer(&runtime, &mock, invocation2(Some("faq"), Some("anything"))).await;
    correction(&text);
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
async fn control_unconfirmed_snapshot_answers_refresh_for_focused_request() {
    let (mock, origin) = MockRest::start(vec![]).await;
    let runtime = closed_runtime(gates(true, true), origin, vec![]).await;
    let text = answer(
        &runtime,
        &mock,
        invocation2(Some("privacy"), Some("deletion-status")),
    )
    .await;
    assert_eq!(
        text,
        "The command list is still refreshing. Try /help again shortly."
    );
    assert!(runtime.published_commands().is_none());
    mock.shutdown().await;
}

#[tokio::test]
async fn control_failed_refresh_keeps_focused_answers_stable() {
    let (mock, origin) = MockRest::start(vec![200, 200, 403]).await;
    let runtime = closed_runtime(gates(false, false), origin, vec![]).await;
    runtime
        .publish_registry_checked(Some(1111))
        .await
        .expect("first publish");
    let before = answer(
        &runtime,
        &mock,
        invocation2(Some("privacy"), Some("deletion-status")),
    )
    .await;
    let old = runtime.published_commands().expect("confirmed");
    assert_eq!(
        runtime.publish_registry_checked(Some(1111)).await,
        Err(RegistrySyncError::Publish)
    );
    assert!(Arc::ptr_eq(
        &old,
        &runtime.published_commands().expect("still confirmed")
    ));
    let after = answer(
        &runtime,
        &mock,
        invocation2(Some("privacy"), Some("deletion-status")),
    )
    .await;
    assert_eq!(
        before, after,
        "a failed publish leaks no new paths and changes no answer"
    );
    mock.shutdown().await;
}
