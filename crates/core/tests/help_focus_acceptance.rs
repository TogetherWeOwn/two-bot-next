//! Protected focused-branch help contract. No implementation belongs here.
//!
//! `CommandDefinition::help_branch_text()` is the renderer seam for the Builder:
//! resolve one space-separated subcommand/group path against the confirmed
//! definition and render that branch with the existing definition-driven
//! vocabulary. The compatibility trait keeps this target executable on the old
//! whole-command renderer: the fallback ignores the path and returns the full
//! detail, so every focused assertion below fails until the Builder's inherent
//! method takes precedence without test edits. Runtime acceptance separately
//! requires real `/help command:... subcommand:...` dispatch.

use two_bot_core::commands::{CommandChoice, CommandDefinition, CommandOption, CommandOptionType};

trait LegacyHelpFocus {
    fn help_branch_text(&self, path: &str) -> Option<String>;
}

impl LegacyHelpFocus for CommandDefinition {
    fn help_branch_text(&self, path: &str) -> Option<String> {
        let _ = path;
        Some(self.help_detail_text())
    }
}

fn require(text: &str, tokens: &[&str]) {
    let lower = text.to_lowercase();
    for token in tokens {
        assert!(
            lower.contains(&token.to_lowercase()),
            "missing {token:?} in {text:?}"
        );
    }
}

fn option_line<'a>(text: &'a str, name: &str) -> &'a str {
    text.lines()
        .find(|line| {
            line.contains(name)
                && line.split(|c: char| !c.is_ascii_alphabetic()).any(|word| {
                    word.eq_ignore_ascii_case("required") || word.eq_ignore_ascii_case("optional")
                })
        })
        .unwrap_or_else(|| panic!("no argument detail/requiredness for {name}: {text}"))
}

fn bounded(text: &str) {
    assert!(
        text.encode_utf16().count() <= 6000,
        "one Discord message embed budget"
    );
}

fn focused(def: &CommandDefinition, path: &str) -> String {
    def.help_branch_text(path)
        .unwrap_or_else(|| panic!("focused path {path:?} must resolve: {}", def.name))
}

/// Thirteen single-line subcommands: the whole-command 12-line budget shows
/// the first twelve and omits `zeta`, so only a focused branch render can
/// explain it. `zeta` carries the typed arguments and choices the tests pin.
fn capped_fixture() -> CommandDefinition {
    let mut options: Vec<CommandOption> = (0..12)
        .map(|n| {
            CommandOption::new(
                &format!("filler{n:02}"),
                &format!("Filler branch {n:02}"),
                CommandOptionType::SubCommand,
            )
        })
        .collect();
    options.push(
        CommandOption::new(
            "zeta",
            "Zeta branch beyond the parent cap",
            CommandOptionType::SubCommand,
        )
        .sub_options(vec![
            CommandOption::new("member", "Who to inspect", CommandOptionType::User).required(),
            CommandOption::new("mode", "Delivery mode", CommandOptionType::String).choices(vec![
                CommandChoice {
                    name: "Slow lane".into(),
                    value: "slow_value".into(),
                },
                CommandChoice {
                    name: "Fast lane".into(),
                    value: "fast_value".into(),
                },
            ]),
        ]),
    );
    CommandDefinition::new("project", "Project tools").options(options)
}

fn group_fixture() -> CommandDefinition {
    serde_json::from_value(serde_json::json!({
        "name":"project", "description":"Project tools", "dm_permission":false,
        "options":[
            {"name":"team", "description":"Team tools", "type":2,
                "options":[
                    {"name":"add", "description":"Add a teammate", "type":1, "options":[
                        {"name":"member", "description":"Who to add", "type":6, "required":true},
                        {"name":"note", "description":"Optional note", "type":3}]},
                    {"name":"list", "description":"List teammates", "type":1}]},
            {"name":"billing", "description":"Billing tools", "type":2,
                "options":[
                    {"name":"invoice", "description":"Show the invoice", "type":1}]}
        ]
    }))
    .expect("nested wire definition")
}

#[test]
fn control_compatibility_adapter_ignores_the_path_until_the_builder_lands() {
    let def = capped_fixture();
    assert_eq!(
        LegacyHelpFocus::help_branch_text(&def, "zeta"),
        Some(def.help_detail_text()),
        "fallback is the old whole-command detail"
    );
}

#[test]
fn mutant_old_whole_detail_ignores_the_branch_path() {
    let def = capped_fixture();
    assert_ne!(
        def.help_branch_text("zeta"),
        Some(def.help_detail_text()),
        "a focused branch is not the whole-command detail"
    );
}

#[test]
fn focused_branch_after_parent_cap_shows_full_usage_arguments_and_choices() {
    let def = capped_fixture();
    let whole = def.help_detail_text();
    assert!(
        !whole.contains("Zeta branch beyond the parent cap"),
        "fixture check: the parent cap omits zeta: {whole}"
    );
    let text = focused(&def, "zeta");
    require(
        &text,
        &[
            "/project zeta",
            "Zeta branch beyond the parent cap",
            "Usage: /project zeta <member> [mode]",
        ],
    );
    require(
        option_line(&text, "member"),
        &["required", "user", "Who to inspect"],
    );
    require(
        option_line(&text, "mode"),
        &["optional", "string", "Delivery mode"],
    );
    require(
        &text,
        &["Slow lane", "slow_value", "Fast lane", "fast_value"],
    );
    assert!(
        !text.contains("more options, omitted"),
        "the focused branch owns its own option budget: {text}"
    );
    assert!(
        !text.contains("filler00"),
        "parent siblings do not leak into the focused branch: {text}"
    );
    bounded(&text);
    assert_eq!(text, focused(&def, "zeta"), "deterministic render");
}

#[test]
fn group_path_shows_children_without_unrelated_branches() {
    let def = group_fixture();
    let text = focused(&def, "team");
    let first = text.lines().next().expect("header line");
    assert!(
        first.contains("/project team"),
        "header names the focused path: {text}"
    );
    require(
        &text,
        &[
            "Team tools",
            "Add a teammate",
            "List teammates",
            "/project team add",
            "/project team list",
            "<member>",
            "[note]",
        ],
    );
    for absent in ["billing", "invoice", "/project billing invoice"] {
        assert!(
            !text.contains(absent),
            "unrelated branch must not consume the focused budget: {absent}: {text}"
        );
    }
    bounded(&text);
}

#[test]
fn leaf_subcommand_path_shows_single_usage_and_typed_arguments() {
    let def = group_fixture();
    let text = focused(&def, "team add");
    require(&text, &["Usage: /project team add <member> [note]"]);
    require(
        option_line(&text, "member"),
        &["required", "user", "Who to add"],
    );
    require(
        option_line(&text, "note"),
        &["optional", "string", "Optional note"],
    );
    for absent in ["List teammates", "/project team list", "Billing tools"] {
        assert!(
            !text.contains(absent),
            "sibling paths stay out of the focused branch: {absent}: {text}"
        );
    }
    bounded(&text);
}

#[test]
fn path_segments_are_exact_trimmed_and_ascii_case_insensitive() {
    let def = group_fixture();
    for path in ["TEAM ADD", "  team   add  "] {
        let text = focused(&def, path);
        require(&text, &["/project team add", "Add a teammate"]);
        assert!(
            !text.contains("List teammates"),
            "canonicalized path still focuses: {path}: {text}"
        );
    }
    // Prefixes, aliases-by-accident, leaf names, the root name itself and
    // over-deep or empty paths resolve to nothing: the runtime answers the
    // private correction for each.
    for path in [
        "tea",
        "team ada",
        "team/add",
        "member",
        "project",
        "team add extra",
        "does-not-exist",
        "",
        "   ",
    ] {
        assert!(
            def.help_branch_text(path).is_none(),
            "unresolvable path must be None: {path:?}"
        );
    }
}

#[test]
fn focused_branch_keeps_permission_caveat_without_authorizing() {
    let def = CommandDefinition::new("gated", "A gated command")
        .permissions(32)
        .options(vec![
            CommandOption::new("early", "Early branch", CommandOptionType::SubCommand),
            CommandOption::new("audit", "Audit branch", CommandOptionType::SubCommand).sub_options(
                vec![
                    CommandOption::new("target", "Who to audit", CommandOptionType::User)
                        .required(),
                ],
            ),
        ]);
    let text = focused(&def, "audit");
    require(
        &text,
        &["/gated audit", "Audit branch", "target", "required"],
    );
    require(&text, &["Manage Server", "permission"]);
    let lower = text.to_lowercase();
    assert!(
        lower.contains("picker"),
        "preserve picker permission caveat: {text}"
    );
    assert!(
        lower.contains("not")
            && (lower.contains("evaluat")
                || lower.contains("guarantee")
                || lower.contains("check")),
        "not a full effective-permission evaluation: {text}"
    );
    assert!(!lower.contains("you can use this"));
    assert!(
        !text.contains("Early branch"),
        "sibling stays out of the focused branch: {text}"
    );
}

#[test]
fn focused_branch_budget_is_local_with_explicit_omissions() {
    let wide = CommandOption::new(
        "wide",
        "Wide branch with many inputs",
        CommandOptionType::SubCommand,
    )
    .sub_options(
        (0..25)
            .map(|n| {
                CommandOption::new(
                    &format!("option{n:02}"),
                    &format!("Argument {n:02}"),
                    CommandOptionType::String,
                )
            })
            .collect(),
    );
    let def = CommandDefinition::new("tool", "Tool with one wide branch").options(vec![wide]);
    let text = focused(&def, "wide");
    bounded(&text);
    require(&text, &["option00", "Argument 00"]);
    assert!(
        text.contains("(+13 more options, omitted from this detail.)"),
        "twelve branch options shown, thirteen explicitly omitted: {text}"
    );
    assert_eq!(text, focused(&def, "wide"));
}

#[test]
fn choices_and_limits_come_from_the_focused_branch() {
    let zeta = CommandOption::new(
        "zeta",
        "Zeta branch beyond the parent cap",
        CommandOptionType::SubCommand,
    )
    .sub_options(vec![
        {
            let mut count =
                CommandOption::new("count", "How many", CommandOptionType::Integer).required();
            count.min_value = Some(-7);
            count.max_value = Some(19);
            count
        },
        CommandOption::new("mode", "Delivery mode", CommandOptionType::String).choices(
            (0..10)
                .map(|n| CommandChoice {
                    name: format!("Choice {n:02}"),
                    value: format!("value_{n:02}"),
                })
                .collect(),
        ),
    ]);
    let mut options: Vec<CommandOption> = (0..12)
        .map(|n| {
            CommandOption::new(
                &format!("filler{n:02}"),
                &format!("Filler branch {n:02}"),
                CommandOptionType::SubCommand,
            )
        })
        .collect();
    options.push(zeta);
    let def = CommandDefinition::new("project", "Project tools").options(options);
    let text = focused(&def, "zeta");
    require(option_line(&text, "count"), &["integer", "-7", "19"]);
    require(&text, &["Choice 00", "value_00"]);
    assert!(
        text.contains("(+2 more choices, omitted)"),
        "choice cap stays explicit inside the focused branch: {text}"
    );
    assert!(
        !text.contains("525600"),
        "no hardcoded limits from other commands: {text}"
    );
    bounded(&text);
}

#[test]
fn focused_branch_keeps_hostile_text_inert() {
    let mut options: Vec<CommandOption> = (0..12)
        .map(|n| {
            CommandOption::new(
                &format!("filler{n:02}"),
                &format!("Filler branch {n:02}"),
                CommandOptionType::SubCommand,
            )
        })
        .collect();
    options.push(
        CommandOption::new(
            "zeta",
            "Zeta @everyone <@123> **bold** `code`",
            CommandOptionType::SubCommand,
        )
        .sub_options(vec![CommandOption::new(
            "mode",
            "Choose [label](https://invalid.example/x) <@&456>",
            CommandOptionType::String,
        )]),
    );
    let def = CommandDefinition::new("project", "Project tools").options(options);
    let text = focused(&def, "zeta");
    require(
        &text,
        &["/project zeta", "Zeta", "bold", "code", "Choose", "label"],
    );
    for raw in [
        "<@123>",
        "<@&456>",
        "**bold**",
        "`code`",
        "[label](https://invalid.example/x)",
    ] {
        assert!(
            !text.contains(raw),
            "untrusted branch text must be inert: {text}"
        );
    }
    bounded(&text);
}
