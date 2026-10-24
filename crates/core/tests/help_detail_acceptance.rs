//! Protected definition-driven help contract. No implementation belongs here.
//!
//! `CommandDefinition::help_detail_text()` is the renderer seam for the Builder.
//! The compatibility trait keeps this target executable on the old name-only
//! renderer. An inherent production method takes precedence without test edits.
//! Runtime acceptance separately requires real `/help command:...` dispatch.

use two_bot_core::commands::{
    core_commands, CommandChoice, CommandDefinition, CommandOption, CommandOptionType,
};

trait LegacyHelpDetail {
    fn help_detail_text(&self) -> String;
}

impl LegacyHelpDetail for CommandDefinition {
    fn help_detail_text(&self) -> String {
        two_bot_core::help_text(std::slice::from_ref(self))
    }
}

#[test]
fn control_compatibility_adapter_keeps_the_existing_grouped_index_separate() {
    let def = CommandDefinition::new("fixture", "Not an index description");
    let text = LegacyHelpDetail::help_detail_text(&def);
    assert!(text.contains("**Server commands**"));
    assert!(text.contains("/fixture"));
    assert!(!text.contains("Not an index description"));
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

fn continuation(text: &str) {
    let lower = text.to_lowercase();
    assert!(
        [
            "truncat",
            "omitted",
            "more options",
            "continued",
            "continuation"
        ]
        .iter()
        .any(|word| lower.contains(word)),
        "truncation must be explicit: {text}"
    );
}

#[test]
fn optional_command_selector_is_published_without_breaking_index_invocation() {
    let defs = core_commands();
    let help = defs
        .iter()
        .find(|def| def.name == "help")
        .expect("help remains registered");
    assert!(!help.dm_permission);
    assert_eq!(
        help.default_member_permissions, None,
        "discovery is not admin-only"
    );
    assert_eq!(help.options.len(), 1, "one optional command selector");
    let selector = &help.options[0];
    assert_eq!(selector.name, "command");
    assert_eq!(selector.kind, 3);
    assert!(
        !selector.required.unwrap_or(false),
        "omission preserves grouped index"
    );
    assert_eq!(selector.max_length, Some(32));
    assert!(
        selector.choices.is_empty(),
        "custom names cannot be frozen choices"
    );
}

#[test]
fn simple_definition_supplies_description_and_formable_usage() {
    let text = CommandDefinition::new("hello", "A confirmed greeting, not a guessed action")
        .help_detail_text();
    require(
        &text,
        &[
            "/hello",
            "A confirmed greeting, not a guessed action",
            "usage",
        ],
    );
    assert!(!text.contains("<member>"), "do not invent arguments");
    bounded(&text);
}

#[test]
fn required_and_optional_types_are_attached_to_the_correct_arguments() {
    let def = CommandDefinition::new("fixture", "Typed arguments").options(vec![
        CommandOption::new("subject", "The person to inspect", CommandOptionType::User).required(),
        CommandOption::new("quiet", "Do not announce", CommandOptionType::Boolean),
        CommandOption::new("badge", "A role label", CommandOptionType::Role),
        CommandOption::new(
            "evidence",
            "Uploaded evidence",
            CommandOptionType::Attachment,
        ),
    ]);
    let text = def.help_detail_text();
    require(
        option_line(&text, "subject"),
        &["required", "user", "The person to inspect"],
    );
    for (name, kind, description) in [
        ("quiet", "boolean", "Do not announce"),
        ("badge", "role", "A role label"),
        ("evidence", "attachment", "Uploaded evidence"),
    ] {
        require(option_line(&text, name), &["optional", kind, description]);
    }
    require(&text, &["<subject>", "[quiet]", "[badge]", "[evidence]"]);
}

#[test]
fn numeric_length_and_channel_constraints_come_from_the_definition() {
    let def: CommandDefinition = serde_json::from_value(serde_json::json!({
        "name":"bounds", "description":"Non-default fixture limits", "dm_permission":false,
        "options":[
            {"name":"count", "description":"How many", "type":4, "required":true, "min_value":-7, "max_value":19},
            {"name":"label", "description":"Short text", "type":3, "min_length":2, "max_length":37},
            {"name":"destination", "description":"Where", "type":7, "channel_types":[0,2]},
            {"name":"ratio", "description":"Numeric weight", "type":10, "min_value":1, "max_value":9}
        ]
    })).expect("valid confirmed command fixture");
    let text = def.help_detail_text();
    require(option_line(&text, "count"), &["integer", "-7", "19"]);
    require(option_line(&text, "label"), &["string", "2", "37"]);
    require(
        option_line(&text, "destination"),
        &["channel", "text", "voice"],
    );
    require(option_line(&text, "ratio"), &["number", "1", "9"]);
    assert!(!text.contains("525600"), "no hardcoded schedule limits");
}

#[test]
fn bounded_choices_preserve_labels_values_and_definition_order() {
    let def =
        CommandDefinition::new("choose", "Pick a fixture value").options(vec![CommandOption::new(
            "mode",
            "Delivery mode",
            CommandOptionType::String,
        )
        .choices(vec![
            CommandChoice {
                name: "Slow lane".into(),
                value: "slow_value".into(),
            },
            CommandChoice {
                name: "Fast lane".into(),
                value: "fast_value".into(),
            },
        ])]);
    let text = def.help_detail_text();
    require(
        &text,
        &["Slow lane", "slow_value", "Fast lane", "fast_value"],
    );
    assert!(
        text.find("Slow lane") < text.find("Fast lane"),
        "preserve choice order"
    );
}

#[test]
fn nested_subcommands_and_groups_have_complete_usage_paths() {
    let def: CommandDefinition = serde_json::from_value(serde_json::json!({
        "name":"project", "description":"Project tools", "dm_permission":false,
        "options":[{"name":"team", "description":"Team tools", "type":2,
            "options":[
                {"name":"add", "description":"Add a teammate", "type":1, "options":[
                    {"name":"member", "description":"Who to add", "type":6, "required":true},
                    {"name":"note", "description":"Optional note", "type":3}]},
                {"name":"list", "description":"List teammates", "type":1}
            ]}]
    }))
    .expect("nested wire definition");
    let text = def.help_detail_text();
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
    assert!(text.find("Add a teammate") < text.find("List teammates"));
    bounded(&text);
}

#[test]
fn custom_description_and_edited_builtin_description_are_not_duplicate_prose() {
    for name in ["customfixture", "rank"] {
        let text = CommandDefinition::new(name, "Confirmed revision zeta; not compiled prose")
            .options(vec![CommandOption::new(
                "fresh",
                "Only this revision has this input",
                CommandOptionType::String,
            )
            .required()])
            .help_detail_text();
        require(
            &text,
            &[
                "Confirmed revision zeta",
                "fresh",
                "Only this revision has this input",
            ],
        );
        assert!(
            !text.contains("Show your XP"),
            "definitions, not a per-name duplicate table"
        );
        assert!(
            !text.contains("member]"),
            "do not add compiled rank options"
        );
    }
}

#[test]
fn permissions_are_definition_driven_and_do_not_promise_full_authorization() {
    let text = CommandDefinition::new("customfixture", "A gated custom definition")
        .permissions(32)
        .help_detail_text();
    require(&text, &["Manage Server", "permission"]);
    let lower = text.to_lowercase();
    assert!(
        lower.contains("picker"),
        "preserve picker permission caveat"
    );
    assert!(
        lower.contains("not")
            && (lower.contains("evaluat")
                || lower.contains("guarantee")
                || lower.contains("check")),
        "not a full effective-permission evaluation: {text}"
    );
    assert!(!lower.contains("you can use this"));
    let admin = CommandDefinition::new("adminfixture", "Explicit zero permission bits")
        .permissions(0)
        .help_detail_text();
    require(&admin, &["administrator"]);
}

#[test]
fn unknown_permission_bits_are_not_reported_as_unrestricted() {
    let text = CommandDefinition::new("futurepermission", "Future permission")
        .permissions(1u64 << 60)
        .help_detail_text();
    assert!(!text.to_lowercase().contains("no permission required"));
    assert!(!text.to_lowercase().contains("everyone can"));
    require(&text, &["permission", "1152921504606846976"]);
}

#[test]
fn excessive_choices_are_bounded_and_omission_is_explicit() {
    let def = CommandDefinition::new("manychoices", "Large choice fixture").options(vec![
        CommandOption::new("mode", "Choose mode", CommandOptionType::String).choices(
            (0..80)
                .map(|n| CommandChoice {
                    name: format!("Choice {n:02} {}", "😀".repeat(80)),
                    value: format!("value_{n:02}"),
                })
                .collect(),
        ),
    ]);
    let text = def.help_detail_text();
    bounded(&text);
    require(&text, &["Choice 00", "value_00"]);
    continuation(&text);
    assert_eq!(
        text,
        def.help_detail_text(),
        "deterministic pagination/truncation"
    );
}

#[test]
fn valid_maximum_option_and_choice_counts_have_explicit_bounded_continuation() {
    let options = (0..25)
        .map(|n| {
            CommandOption::new(
                &format!("option{n:02}"),
                &format!("Argument {n:02}"),
                CommandOptionType::String,
            )
            .choices(
                (0..25)
                    .map(|c| CommandChoice {
                        name: format!("Label {c:02} {}", "😀".repeat(40)),
                        value: format!("value_{c:02}"),
                    })
                    .collect(),
            )
        })
        .collect();
    let def = CommandDefinition::new("maximum", "Valid wide definition").options(options);
    let text = def.help_detail_text();
    bounded(&text);
    require(&text, &["option00", "Label 00", "value_00"]);
    continuation(&text);
    assert_eq!(text, def.help_detail_text());
}

#[test]
fn hostile_option_and_choice_text_is_inert_without_discarding_the_explanation() {
    let def = CommandDefinition::new("safe", "Description **bold** <@123>").options(vec![
        CommandOption::new("mode", "Choose `mode` @everyone", CommandOptionType::String).choices(
            vec![CommandChoice {
                name: "[label](https://invalid.example/x)".into(),
                value: "<@&456>".into(),
            }],
        ),
    ]);
    let text = def.help_detail_text();
    require(&text, &["Description", "bold", "Choose", "label"]);
    for raw in [
        "**bold**",
        "<@123>",
        "`mode`",
        "[label](https://invalid.example/x)",
        "<@&456>",
    ] {
        assert!(
            !text.contains(raw),
            "untrusted definition text must be inert: {text}"
        );
    }
}

#[test]
fn excessive_depth_and_total_text_cannot_hide_silent_loss_or_overflow() {
    let mut nested =
        CommandOption::new("leaf", "Leaf description", CommandOptionType::String).required();
    for n in (0..80).rev() {
        nested = CommandOption::new(
            &format!("level{n}"),
            &format!("Depth {n}"),
            CommandOptionType::SubCommand,
        )
        .sub_options(vec![nested]);
    }
    let def =
        CommandDefinition::new("deep", &"😀long-description".repeat(1000)).options(vec![nested]);
    let text = def.help_detail_text();
    bounded(&text);
    require(&text, &["/deep"]);
    continuation(&text);
    assert_eq!(text, def.help_detail_text());
}

#[test]
fn unsupported_option_type_is_truthful_not_a_fabricated_string() {
    let mut unknown = CommandOption::new("future", "Future field", CommandOptionType::String);
    unknown.kind = 255;
    let text = CommandDefinition::new("futuretype", "Unknown shape")
        .options(vec![unknown])
        .help_detail_text();
    require(&text, &["future", "255"]);
    let lower = text.to_lowercase();
    assert!(
        lower.contains("unknown") || lower.contains("unsupported"),
        "unknown type must be explicit: {text}"
    );
}

#[test]
fn missing_required_flag_is_optional_not_a_guessed_requirement() {
    let mut explicit =
        CommandOption::new("explicit", "Explicitly optional", CommandOptionType::String);
    explicit.required = Some(false);
    let text = CommandDefinition::new("optionalfixture", "Optional inputs")
        .options(vec![
            CommandOption::new("implicit", "Omitted flag", CommandOptionType::String),
            explicit,
        ])
        .help_detail_text();
    require(option_line(&text, "implicit"), &["optional"]);
    require(option_line(&text, "explicit"), &["optional"]);
    require(&text, &["[implicit]", "[explicit]"]);
}
