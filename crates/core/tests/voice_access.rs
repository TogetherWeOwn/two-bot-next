//! Hermetic V10b acceptance cases against the public access-gate API.

use std::collections::BTreeMap;

use proptest::prelude::*;
use two_bot_core::voice_access::{
    is_voice_command, may_create_room, may_use_command, validate_access_controls, AccessControls,
    AccessDecision, AccessDenyReason, AccessError, AccessMember, VOICE_COMMANDS,
};

fn controls(
    room_creation_enabled: bool,
    required_role: Option<u64>,
    command_roles: &[(&str, &[u64])],
) -> AccessControls {
    AccessControls {
        room_creation_enabled,
        required_role,
        command_roles: command_roles
            .iter()
            .map(|(command, roles)| ((*command).to_owned(), (*roles).to_vec()))
            .collect(),
    }
}

fn member(is_admin: bool, roles: &[u64]) -> AccessMember {
    AccessMember {
        is_admin,
        roles: roles.to_vec(),
    }
}

fn open() -> AccessControls {
    controls(true, None, &[])
}

// ---- table tests: every decision branch ----

#[test]
fn admin_is_always_allowed() {
    let gated = controls(true, Some(7), &[("limit", &[7]), ("kick", &[8])]);
    // Missing the required role, missing every command role, even facing an
    // empty (deny-all) list: admins still pass.
    let empty_list = controls(true, Some(7), &[("limit", &[])]);
    for (gates, roles, command) in [
        (&gated, vec![], "limit"),
        (&gated, vec![7], "kick"),
        (&gated, vec![9], "transfer"),
        (&empty_list, vec![], "limit"),
        (&open(), vec![], "limit"),
    ] {
        assert_eq!(
            may_use_command(gates, &member(true, &roles), command),
            AccessDecision::Allow,
            "admin with roles {roles:?} on {command}"
        );
    }
}

#[test]
fn guild_role_gate_comes_before_command_restrictions() {
    let gates = controls(true, Some(7), &[("limit", &[7])]);
    // Lacks the guild-wide role on an unrestricted command: RequiredRole, not
    // CommandRestricted. The guild-wide gate is evaluated first.
    assert_eq!(
        may_use_command(&gates, &member(false, &[8]), "unrestricted-command-name"),
        AccessDecision::Deny(AccessDenyReason::RequiredRole)
    );
    assert_eq!(
        may_use_command(&gates, &member(false, &[]), "limit"),
        AccessDecision::Deny(AccessDenyReason::RequiredRole)
    );
    // Guild gate satisfied, command unrestricted: allowed.
    assert_eq!(
        may_use_command(&gates, &member(false, &[7]), "transfer"),
        AccessDecision::Allow
    );
}

#[test]
fn per_command_restriction_needs_one_matching_role() {
    let gates = controls(true, None, &[("kick", &[7, 8])]);
    assert_eq!(
        may_use_command(&gates, &member(false, &[8, 9]), "kick"),
        AccessDecision::Allow
    );
    assert_eq!(
        may_use_command(&gates, &member(false, &[9]), "kick"),
        AccessDecision::Deny(AccessDenyReason::CommandRestricted)
    );
    assert_eq!(
        may_use_command(&gates, &member(false, &[]), "kick"),
        AccessDecision::Deny(AccessDenyReason::CommandRestricted)
    );
    // A restriction on one command leaves the others alone.
    assert_eq!(
        may_use_command(&gates, &member(false, &[]), "limit"),
        AccessDecision::Allow
    );
}

#[test]
fn present_but_empty_role_list_denies_every_non_admin() {
    // An empty list is fail-closed: it restricts the command to nobody.
    // Remove the entry to lift the restriction.
    let gates = controls(true, None, &[("limit", &[])]);
    assert_eq!(
        may_use_command(&gates, &member(false, &[7]), "limit"),
        AccessDecision::Deny(AccessDenyReason::CommandRestricted)
    );
    assert_eq!(
        may_use_command(&open(), &member(false, &[7]), "limit"),
        AccessDecision::Allow
    );
    assert_eq!(
        may_use_command(&gates, &member(true, &[]), "limit"),
        AccessDecision::Allow
    );
}

#[test]
fn creation_switch_never_changes_command_decisions() {
    let gated = controls(false, Some(7), &[("kick", &[8])]);
    assert!(!may_create_room(&gated));
    assert!(may_create_room(&controls(true, Some(7), &[("kick", &[8])])));
    // Commands on existing rooms are still evaluated by the same gate.
    let cases = [
        (
            vec![],
            "limit",
            AccessDecision::Deny(AccessDenyReason::RequiredRole),
        ),
        (
            vec![7],
            "kick",
            AccessDecision::Deny(AccessDenyReason::CommandRestricted),
        ),
        (vec![7, 8], "kick", AccessDecision::Allow),
    ];
    for (roles, command, expected) in cases {
        assert_eq!(
            may_use_command(&gated, &member(false, &roles), command),
            expected,
            "creation off, roles {roles:?} on {command}"
        );
    }
}

#[test]
fn unknown_invoked_commands_match_no_restriction_entry() {
    // Restriction keys are validated to known commands, so a routed unknown
    // name can never accidentally match one; only the guild-wide gate applies.
    let gates = controls(true, None, &[("limit", &[7])]);
    assert_eq!(
        may_use_command(&gates, &member(false, &[]), "bogus"),
        AccessDecision::Allow
    );
    let gated = controls(true, Some(7), &[("limit", &[7])]);
    assert_eq!(
        may_use_command(&gated, &member(false, &[]), "bogus"),
        AccessDecision::Deny(AccessDenyReason::RequiredRole)
    );
}

#[test]
fn command_list_covers_every_spec_command() {
    for command in [
        "alias",
        "alwaysprivate",
        "channelinfo",
        "create",
        "defaultlimit",
        "export",
        "group",
        "import",
        "inheritpermissions",
        "invite",
        "kick",
        "limit",
        "logging",
        "name",
        "nick",
        "ping",
        "position",
        "private",
        "public",
        "reclaim",
        "setup",
        "template",
        "templateassistant",
        "textchannels",
        "transfer",
        "unlimit",
    ] {
        assert!(
            is_voice_command(command),
            "{command} must be a known voice command"
        );
    }
    assert!(!is_voice_command("bogus"));
    assert!(!is_voice_command("Limit"));
    assert!(!is_voice_command(""));
}

#[test]
fn validation_refuses_unknown_commands_and_zero_roles() {
    assert_eq!(
        validate_access_controls(&controls(true, None, &[("bogus", &[7])])),
        Err(AccessError::UnknownCommand("bogus".to_owned()))
    );
    assert_eq!(
        validate_access_controls(&controls(true, None, &[("Limit", &[7])])),
        Err(AccessError::UnknownCommand("Limit".to_owned()))
    );
    assert!(validate_access_controls(&open()).is_ok());
    // Empty lists pass validation (fail-closed decision, not a typo), but
    // zero role IDs never do.
    assert!(validate_access_controls(&controls(true, None, &[("limit", &[])])).is_ok());
    assert_eq!(
        validate_access_controls(&controls(true, Some(0), &[])),
        Err(AccessError::InvalidRoleId)
    );
    assert_eq!(
        validate_access_controls(&controls(true, None, &[("limit", &[0])])),
        Err(AccessError::InvalidRoleId)
    );
}

// ---- property tests ----

fn role_vec() -> impl Strategy<Value = Vec<u64>> {
    proptest::collection::vec(1u64..=5, 0..4)
}

fn controls_strategy() -> impl Strategy<Value = AccessControls> {
    (
        any::<bool>(),
        proptest::option::of(1u64..=5),
        proptest::collection::vec(
            (
                proptest::sample::select(VOICE_COMMANDS.to_vec()),
                proptest::collection::vec(1u64..=5, 0..3),
            ),
            0..3,
        ),
    )
        .prop_map(|(room_creation_enabled, required_role, entries)| {
            let mut command_roles = BTreeMap::new();
            for (command, roles) in entries {
                command_roles.insert((*command).to_owned(), roles);
            }
            AccessControls {
                room_creation_enabled,
                required_role,
                command_roles,
            }
        })
}

fn command_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        proptest::sample::select(VOICE_COMMANDS.to_vec()).prop_map(|c| (*c).to_owned()),
        Just("bogus-command".to_owned()),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn property_admin_is_never_denied(
        gates in controls_strategy(),
        roles in role_vec(),
        command in command_strategy(),
    ) {
        prop_assert_eq!(
            may_use_command(&gates, &member(true, &roles), &command),
            AccessDecision::Allow
        );
    }

    #[test]
    fn property_removing_a_restriction_never_denies(
        mut gates in controls_strategy(),
        roles in role_vec(),
        command in command_strategy(),
    ) {
        let viewer = member(false, &roles);
        prop_assert!(validate_access_controls(&gates).is_ok());
        if may_use_command(&gates, &viewer, &command) != AccessDecision::Allow {
            return Ok(());
        }
        // Lifting the guild-wide gate keeps every prior Allow.
        gates.required_role = None;
        prop_assert_eq!(
            may_use_command(&gates, &viewer, &command),
            AccessDecision::Allow
        );
        // Deleting a per-command entry (the supported way to unrestrict)
        // keeps every prior Allow, including into an empty map.
        gates.command_roles.remove(&command);
        prop_assert_eq!(
            may_use_command(&gates, &viewer, &command),
            AccessDecision::Allow
        );
        gates.command_roles.clear();
        prop_assert_eq!(
            may_use_command(&gates, &viewer, &command),
            AccessDecision::Allow
        );
    }
}
