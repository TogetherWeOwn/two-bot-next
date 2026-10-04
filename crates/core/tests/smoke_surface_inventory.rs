//! Staging-smoke surface inventory: the fixture pins the full built-in set.
//!
//! The all-gates-on `publish_set` output (names, descriptions, permission
//! bitfields, registry bounds, choices, DM availability) must match
//! `fixtures/smoke_surface_inventory.json` exactly, in publish order. The
//! fixture additionally marks the five smoke-covered commands (one per
//! routing family, matching `crates/discord/tests/top5_reply_fixtures.rs`)
//! versus the 22 explicitly deferred, with a reason per deferral.
//!
//! Offline only: no staging guild, no live Discord, no network, no database.

use serde_json::Value;
use two_bot_core::{
    HandlerId, InteractionRouter, ModerationAction, RouterGates, SlashContext, SlashOutcome,
};

fn all_on_router() -> InteractionRouter {
    InteractionRouter::new(RouterGates {
        configured_guild: Some(1),
        scorecard: true,
        automations: true,
        announcements: true,
        moderation: true,
        voice: true,
        tickets: true,
        self_roles: true,
        onboarding_picker: true,
        session_picker: true,
    })
}

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/smoke_surface_inventory.json"))
        .expect("smoke surface inventory fixture parses")
}

fn option_type_name(kind: u8) -> &'static str {
    match kind {
        3 => "string",
        4 => "integer",
        6 => "user",
        other => panic!("document new command option type {other}"),
    }
}

#[test]
fn inventory_matches_registry_output_in_publish_order() {
    let published = all_on_router().publish_set(&[]).expect("publish set");
    let fixture = fixture();
    assert_eq!(fixture["count"], 29, "29 built-ins");
    assert_eq!(
        fixture["dm_permission_all_false"], true,
        "fixture claims guild-only"
    );
    let rows = fixture["commands"].as_array().expect("command rows");
    assert_eq!(published.len(), rows.len(), "registry row count matches");
    assert_eq!(published.len(), 29, "29 built-ins in the registry");

    for (definition, row) in published.iter().zip(rows.iter()) {
        let name = row["name"].as_str().expect("row name");
        assert_eq!(definition.name, name, "publish order row");
        assert_eq!(
            definition.description.as_str(),
            row["description"].as_str().expect("row description"),
            "/{name} description"
        );
        assert!(
            !definition.dm_permission,
            "/{name} is guild-only (DMs false)"
        );
        assert_eq!(row["dm_permission"], false, "/{name} fixture DMs false");
        let expected_perms = row["default_member_permissions"].clone();
        let actual_perms = definition
            .default_member_permissions
            .as_ref()
            .map(|bits| Value::String(bits.clone()))
            .unwrap_or(Value::Null);
        assert_eq!(actual_perms, expected_perms, "/{name} permission bits");

        let expected_options = row["options"].as_array().expect("row options");
        assert_eq!(
            definition.options.len(),
            expected_options.len(),
            "/{name} option count"
        );
        for (option, expected) in definition.options.iter().zip(expected_options.iter()) {
            let option_name = expected["name"].as_str().expect("option name");
            assert_eq!(option.name, option_name, "/{name} option name");
            assert_eq!(
                option.description.as_str(),
                expected["description"]
                    .as_str()
                    .expect("option description"),
                "/{name} option {option_name} description"
            );
            assert_eq!(
                option_type_name(option.kind),
                expected["type"].as_str().expect("option type"),
                "/{name} option {option_name} type"
            );
            assert_eq!(
                option.required.unwrap_or(false),
                expected["required"].as_bool().expect("option required"),
                "/{name} option {option_name} required"
            );
            let num = |v: &Value| v.as_i64();
            assert_eq!(num(&expected["min_value"]), option.min_value);
            assert_eq!(num(&expected["max_value"]), option.max_value);
            assert_eq!(
                expected["max_length"].as_u64().map(|v| v as u32),
                option.max_length,
                "/{name} option {option_name} max_length"
            );
            let expected_choices = expected["choices"].as_array().expect("choices");
            assert_eq!(
                option.choices.len(),
                expected_choices.len(),
                "/{name} option {option_name} choice count"
            );
            for (choice, expected_choice) in option.choices.iter().zip(expected_choices.iter()) {
                assert_eq!(
                    choice.name.as_str(),
                    expected_choice["name"].as_str().expect("choice name"),
                    "/{name} option {option_name} choice name"
                );
                assert_eq!(
                    choice.value.as_str(),
                    expected_choice["value"].as_str().expect("choice value"),
                    "/{name} option {option_name} choice value"
                );
            }
        }
    }
}

#[test]
fn exactly_five_covered_with_response_links_rest_deferred_with_reasons() {
    let rows = fixture()["commands"]
        .as_array()
        .expect("command rows")
        .clone();
    let covered: Vec<&Value> = rows.iter().filter(|r| r["smoke"] == "covered").collect();
    let deferred: Vec<&Value> = rows.iter().filter(|r| r["smoke"] == "deferred").collect();
    assert_eq!(covered.len(), 5, "exactly the top five");
    assert_eq!(covered.len() + deferred.len(), 29, "every row marked");

    let mut covered_names: Vec<&str> = covered
        .iter()
        .map(|r| r["name"].as_str().expect("covered name"))
        .collect();
    covered_names.sort_unstable();
    assert_eq!(
        covered_names,
        ["ban", "leaderboard", "lfg", "rank", "rsvp"],
        "the smoke five, one per routing family"
    );

    for row in &covered {
        let name = row["name"].as_str().unwrap();
        let link = row["expected_response"].as_str().unwrap_or("");
        assert!(
            link.contains("docs/smoke-expected-responses.md"),
            "/{name} links its expected responses"
        );
        assert!(
            link.contains("docs/smoke-run-record.md"),
            "/{name} links its run-record row"
        );
        assert!(
            row["deferral_reason"].is_null(),
            "/{name} covered means no deferral reason"
        );
    }
    for row in &deferred {
        let name = row["name"].as_str().unwrap();
        let reason = row["deferral_reason"].as_str().unwrap_or("");
        assert!(
            !reason.is_empty(),
            "/{name} deferred with an explicit reason"
        );
        assert!(
            row["expected_response"].is_null(),
            "/{name} deferred means no expected-response link"
        );
    }
}

#[test]
fn covered_five_route_to_their_handlers() {
    let router = all_on_router();
    let expected: &[(&str, HandlerId)] = &[
        ("rank", HandlerId::Rank),
        ("leaderboard", HandlerId::Leaderboard),
        ("rsvp", HandlerId::Rsvp),
        ("lfg", HandlerId::Lfg),
        ("ban", HandlerId::Moderation(ModerationAction::Ban)),
    ];
    for &(name, handler) in expected {
        let outcome = router.route_slash(&SlashContext {
            name,
            guild_id: Some(1),
            actor_permissions: Some(u64::MAX),
            custom_row: None,
        });
        assert_eq!(
            outcome,
            SlashOutcome::Handled { handler },
            "/{name} routes to its handler"
        );
    }
}
