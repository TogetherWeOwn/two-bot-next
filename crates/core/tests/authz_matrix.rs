//! Authorization matrix: role x route x expected outcome, fixture-driven.
//!
//! `tests/fixtures/authz_matrix.json` holds the table; this file only drives
//! the production seams and compares. Two layers:
//!
//! 1. `InteractionRouter::route_slash` (every feature gate on) for each
//!    retained builtin slash command under each named role, plus the guild
//!    fence and unresolved-permission contexts.
//! 2. `assert_moderation_allowed` for the member-moderation policy order
//!    (permission, target presence, self, protection, bot hierarchy, actor
//!    hierarchy).
//!
//! The fixture's permission bits and expectations are literals, not derived
//! from the production constants, so a silent edit to either side fails.
//! A route added to `COMMAND_PERMISSIONS` without a fixture row fails too.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde_json::Value;
use two_bot_core::command_permissions::{CommandSurface, COMMAND_PERMISSIONS};
use two_bot_core::moderation::{
    assert_moderation_allowed, ModerationActor, ModerationPolicy, ModerationRequest,
    ModerationTarget,
};
use two_bot_core::{
    InteractionRouter, ModerationAction, RouterGates, RouterRefusal, SlashContext, SlashOutcome,
};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/authz_matrix.json")).expect("authz fixture parses")
}

fn guild(fixture: &Value) -> u64 {
    fixture["guild_id"].as_u64().expect("guild_id")
}

fn router(guild_id: u64) -> InteractionRouter {
    InteractionRouter::new(RouterGates {
        configured_guild: Some(guild_id),
        scorecard: true,
        automations: true,
        announcements: true,
        moderation: true,
        voice: false,
        voice_assistant: false,
        tickets: true,
        self_roles: true,
        onboarding_picker: true,
        session_picker: true,
    })
}

fn roles(fixture: &Value) -> BTreeMap<String, u64> {
    fixture["roles"]
        .as_object()
        .expect("roles object")
        .iter()
        .map(|(name, bits)| (name.clone(), bits.as_u64().expect("role bits")))
        .collect()
}

/// Stable text for an outcome: `Handled`, `Ignore`, `Unknown` or the refusal.
fn outcome_text(outcome: SlashOutcome) -> String {
    match outcome {
        SlashOutcome::Handled { .. } => "Handled".to_owned(),
        SlashOutcome::Ignore => "Ignore".to_owned(),
        SlashOutcome::Unknown => "Unknown".to_owned(),
        SlashOutcome::Refuse { refusal } => match refusal {
            RouterRefusal::ModerationPermission(action) => {
                format!("ModerationPermission({action:?})")
            }
            other => format!("{other:?}"),
        },
    }
}

fn route(router: &InteractionRouter, name: &str, guild: Option<u64>, bits: Option<u64>) -> String {
    outcome_text(router.route_slash(&SlashContext {
        name,
        guild_id: guild,
        actor_permissions: bits,
        // A same-named enabled custom row must never widen a builtin.
        custom_row: Some(true),
    }))
}

#[test]
fn fixture_routes_equal_the_retained_builtin_set() {
    let fixture = fixture();
    let in_fixture: BTreeSet<&str> = fixture["routes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|route| route["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        in_fixture.len(),
        fixture["routes"].as_array().unwrap().len(),
        "duplicate route row in the authz fixture"
    );
    let in_table: BTreeSet<&str> = COMMAND_PERMISSIONS
        .iter()
        .filter(|row| row.surface == CommandSurface::BuiltinSlash)
        .map(|row| row.command)
        .collect();
    assert_eq!(
        in_fixture, in_table,
        "every retained builtin slash command needs exactly one authz matrix row"
    );
}

#[test]
fn role_x_route_matrix_matches_expected_status() {
    let fixture = fixture();
    let guild_id = guild(&fixture);
    let router = router(guild_id);
    let roles = roles(&fixture);
    let mut checked = 0_usize;
    for route_row in fixture["routes"].as_array().unwrap() {
        let name = route_row["name"].as_str().unwrap();
        let allowed: Option<HashSet<&str>> = match &route_row["allow"] {
            Value::String(star) if star == "*" => None,
            Value::Array(list) => Some(list.iter().map(|v| v.as_str().unwrap()).collect()),
            other => panic!("{name}: bad allow {other}"),
        };
        for allowed_role in allowed.iter().flatten() {
            assert!(
                roles.contains_key(*allowed_role),
                "{name}: unknown role {allowed_role}"
            );
        }
        for (role, bits) in &roles {
            let expected = match &allowed {
                None => "Handled".to_owned(),
                Some(set) if set.contains(role.as_str()) => "Handled".to_owned(),
                Some(_) => route_row["deny"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{name}: restricted row needs deny"))
                    .to_owned(),
            };
            assert_eq!(
                route(&router, name, Some(guild_id), Some(*bits)),
                expected,
                "/{name} as role {role} ({bits})"
            );
            checked += 1;
        }
    }
    // 33 routes x 10 roles: a silently emptied fixture must not pass.
    assert_eq!(checked, 330, "matrix size drifted");
}

#[test]
fn guild_fence_and_unresolved_permissions() {
    let fixture = fixture();
    let guild_id = guild(&fixture);
    let router = router(guild_id);
    for case in fixture["context_cases"].as_array().unwrap() {
        let context = case["context"].as_str().unwrap();
        let ctx_guild = case["guild_id"].as_u64();
        let bits = case["permissions"].as_u64();
        for route_row in fixture["routes"].as_array().unwrap() {
            let name = route_row["name"].as_str().unwrap();
            let is_moderation = ModerationAction::ALL
                .iter()
                .any(|action| action.command_name() == name)
                || matches!(name, "purge-preview" | "channel-inspect");
            let open = route_row["allow"] == "*";
            let expected_key = if is_moderation {
                "moderation"
            } else if open {
                "open"
            } else {
                "restricted"
            };
            let expected = case[expected_key].as_str().unwrap();
            let got = route(&router, name, ctx_guild, bits);
            let matches = match expected {
                // Unresolved bits: the route's own deny string.
                "ModerationPermission" | "RequiredPermission" => {
                    Some(got.as_str()) == route_row["deny"].as_str()
                }
                exact => got == exact,
            };
            assert!(matches, "{context}: /{name} -> {got}, expected {expected}");
        }
    }
}

fn target(spec: &Value) -> ModerationTarget {
    ModerationTarget {
        user_id: spec["id"].as_str().unwrap().to_owned(),
        role_ids: spec["roles"]
            .as_array()
            .map(|roles| {
                roles
                    .iter()
                    .map(|r| r.as_str().unwrap().to_owned())
                    .collect()
            })
            .unwrap_or_default(),
        highest_role_position: spec["pos"].as_i64().unwrap(),
        is_bot: spec["bot"].as_bool().unwrap_or(false),
        is_guild_owner: spec["owner"].as_bool().unwrap_or(false),
    }
}

#[test]
fn member_moderation_policy_order_matches_table() {
    let fixture = fixture();
    let policy_spec = &fixture["moderation_policy"];
    let policy = ModerationPolicy {
        owen_user_id: policy_spec["owen_user_id"].as_str().unwrap().to_owned(),
        protected_role_ids: policy_spec["protected_role_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|role| role.as_str().unwrap().to_owned())
            .collect(),
        bot_user_id: policy_spec["bot_user_id"].as_str().map(str::to_owned),
    };
    let cases = policy_spec["cases"].as_array().unwrap();
    assert!(cases.len() >= 20, "policy table shrank to {}", cases.len());
    let mut names = HashSet::new();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        assert!(names.insert(name), "duplicate policy case {name}");
        let action = ModerationAction::ALL
            .into_iter()
            .find(|action| format!("{action:?}") == case["action"].as_str().unwrap())
            .unwrap_or_else(|| panic!("{name}: unknown action"));
        let actor = &case["actor"];
        let request = ModerationRequest {
            action,
            actor: ModerationActor {
                user_id: actor["id"].as_str().unwrap().to_owned(),
                role_ids: Vec::new(),
                highest_role_position: actor["pos"].as_i64().unwrap(),
                permissions: actor["perms"].as_u64().unwrap(),
            },
            target: case["target"].as_object().map(|_| target(&case["target"])),
            bot_highest_role_position: case["bot_pos"].as_i64().unwrap(),
            reason: "matrix".to_owned(),
            duration_seconds: None,
            count: None,
            seconds: None,
        };
        let got = match assert_moderation_allowed(&request, &policy) {
            Ok(()) => "ok".to_owned(),
            Err(err) => format!("{err:?}"),
        };
        assert_eq!(got, case["expect"].as_str().unwrap(), "policy case {name}");
    }
}
