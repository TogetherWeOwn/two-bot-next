//! Moderation permission-gate matrix (Owen parity).
//!
//! Pins who may invoke each member moderation verb (`ban`, `tempban`, `kick`,
//! `timeout`, `warn`) per invoker role, at both enforcement layers, with the
//! exact denied-path copy each layer returns.
//!
//! Layers:
//! - router (`InteractionRouter::route_slash`): resolved Discord permission
//!   bits against the parity §1 table. Denied copy is the actionable
//!   `RouterRefusal::ModerationPermission(action).message()` (Discord
//!   permission name, slash command, and granter — never the internal id).
//! - policy (`assert_moderation_allowed`): same bits plus target/hierarchy
//!   checks. Denied copy is the `PolicyError` display text (legacy
//!   `Missing required permission for moderation.*`).
//!
//! Synthetic fixtures only: literal permission bits, synthetic snowflake ids,
//! no Discord, no network, no database, no guild dependency.

use std::collections::HashSet;

use two_bot_core::commands::{PERM_BAN_MEMBERS, PERM_KICK_MEMBERS, PERM_MODERATE_MEMBERS};
use two_bot_core::moderation::{
    assert_moderation_allowed, ModerationAction, ModerationActor, ModerationPolicy,
    ModerationRequest, ModerationTarget, PolicyError,
};
use two_bot_core::{HandlerId, InteractionRouter, RouterGates, SlashContext, SlashOutcome};

const GUILD: u64 = 2222;
const OWEN_ID: &str = "123456789012345678";
const BOT_ID: &str = "555555555555555555";
const STAFF_ROLE_ID: &str = "444444444444444444";
const ACTOR_ID: &str = "111111111111111111";
const TARGET_ID: &str = "333333333333333333";

/// Invoker roles as resolved Discord permission bits (parity §1 gates).
struct Role {
    name: &'static str,
    permissions: u64,
}

fn roles() -> Vec<Role> {
    vec![
        Role {
            name: "admin",
            permissions: PERM_BAN_MEMBERS | PERM_KICK_MEMBERS | PERM_MODERATE_MEMBERS,
        },
        Role {
            name: "moderator",
            permissions: PERM_KICK_MEMBERS | PERM_MODERATE_MEMBERS,
        },
        Role {
            name: "helper",
            permissions: PERM_MODERATE_MEMBERS,
        },
        Role {
            name: "member",
            permissions: 0,
        },
    ]
}

/// The five member-targeted verbs under test (channel verbs skip the policy).
fn member_actions() -> [ModerationAction; 5] {
    [
        ModerationAction::Ban,
        ModerationAction::TempBan,
        ModerationAction::Kick,
        ModerationAction::Timeout,
        ModerationAction::Warn,
    ]
}

/// Whether `role` may invoke `action` on permission bits alone.
fn role_may_invoke(role: &Role, action: ModerationAction) -> bool {
    let required = action.required_permission();
    role.permissions & required == required
}

fn router() -> InteractionRouter {
    InteractionRouter::new(RouterGates {
        configured_guild: Some(GUILD),
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

fn policy() -> ModerationPolicy {
    ModerationPolicy {
        owen_user_id: OWEN_ID.to_owned(),
        protected_role_ids: HashSet::from([STAFF_ROLE_ID.to_owned()]),
        bot_user_id: Some(BOT_ID.to_owned()),
    }
}

fn eligible_target() -> ModerationTarget {
    ModerationTarget {
        user_id: TARGET_ID.to_owned(),
        role_ids: vec![],
        highest_role_position: 10,
        is_bot: false,
        is_guild_owner: false,
    }
}

fn policy_request(
    action: ModerationAction,
    permissions: u64,
    target: ModerationTarget,
) -> ModerationRequest {
    ModerationRequest {
        action,
        actor: ModerationActor {
            user_id: ACTOR_ID.to_owned(),
            role_ids: vec![],
            highest_role_position: 50,
            permissions,
        },
        target: Some(target),
        bot_highest_role_position: Some(100),
        reason: "parity gate fixture".to_owned(),
        duration_seconds: None,
        count: None,
        seconds: None,
    }
}

/// Post-425 router denial: Discord permission name, slash command, granter.
fn router_denied_copy(action: ModerationAction) -> String {
    use two_bot_core::RouterRefusal;
    RouterRefusal::ModerationPermission(action).message()
}

/// Policy-layer denial: legacy `PolicyError` display text (unchanged by #425).
fn policy_denied_copy(action: ModerationAction) -> String {
    format!("Missing required permission for {}", action.action_name())
}

#[test]
fn permission_bits_match_legacy_per_action() {
    assert_eq!(ModerationAction::Ban.required_permission(), 1 << 2);
    assert_eq!(ModerationAction::TempBan.required_permission(), 1 << 2);
    assert_eq!(ModerationAction::Kick.required_permission(), 1 << 1);
    assert_eq!(ModerationAction::Timeout.required_permission(), 1 << 40);
    assert_eq!(ModerationAction::Warn.required_permission(), 1 << 40);
    assert_eq!(PERM_BAN_MEMBERS, 1 << 2);
    assert_eq!(PERM_KICK_MEMBERS, 1 << 1);
    assert_eq!(PERM_MODERATE_MEMBERS, 1 << 40);
}

#[test]
fn role_matrix_matches_permission_gates() {
    // Documents the intended matrix so the two enforcement tests below pin it:
    // admins hold every member-moderation bit, moderators kick/timeout/warn,
    // helpers timeout/warn, members invoke nothing.
    let expectations: [(ModerationAction, [bool; 4]); 5] = [
        (ModerationAction::Ban, [true, false, false, false]),
        (ModerationAction::TempBan, [true, false, false, false]),
        (ModerationAction::Kick, [true, true, false, false]),
        (ModerationAction::Timeout, [true, true, true, false]),
        (ModerationAction::Warn, [true, true, true, false]),
    ];
    let roles = roles();
    for (action, allowed) in expectations {
        for (role, expect) in roles.iter().zip(allowed) {
            assert_eq!(
                role_may_invoke(role, action),
                expect,
                "{} may{} invoke {}",
                role.name,
                if expect { "" } else { " not" },
                action.command_name(),
            );
        }
    }
}

#[test]
fn router_allows_or_refuses_per_role_with_actionable_copy() {
    let router = router();
    let roles = roles();
    for action in member_actions() {
        for role in &roles {
            let ctx = SlashContext {
                name: action.command_name(),
                guild_id: Some(GUILD),
                actor_permissions: Some(role.permissions),
                custom_row: None,
            };
            let outcome = router.route_slash(&ctx);
            if role_may_invoke(role, action) {
                assert_eq!(
                    outcome,
                    SlashOutcome::Handled {
                        handler: HandlerId::Moderation(action),
                    },
                    "{} routes {}",
                    role.name,
                    action.command_name(),
                );
            } else {
                let refusal = match outcome {
                    SlashOutcome::Refuse { refusal } => refusal,
                    other => panic!(
                        "{} with {} must refuse, got {other:?}",
                        role.name,
                        action.command_name(),
                    ),
                };
                assert_eq!(
                    refusal.message(),
                    router_denied_copy(action),
                    "{} denied copy for {}",
                    role.name,
                    action.command_name(),
                );
                // Pin the post-425 actionable copy so a silent reword breaks
                // loudly. Never the internal `moderation.*` id.
                let literal = match action {
                    ModerationAction::Ban => {
                        "You need the Ban Members permission to use /ban. Ask a server moderator or admin to grant it."
                    }
                    ModerationAction::TempBan => {
                        "You need the Ban Members permission to use /tempban. Ask a server moderator or admin to grant it."
                    }
                    ModerationAction::Kick => {
                        "You need the Kick Members permission to use /kick. Ask a server moderator or admin to grant it."
                    }
                    ModerationAction::Timeout => {
                        "You need the Moderate Members permission to use /timeout. Ask a server moderator or admin to grant it."
                    }
                    ModerationAction::Warn => {
                        "You need the Moderate Members permission to use /warn. Ask a server moderator or admin to grant it."
                    }
                    _ => unreachable!("member verbs only"),
                };
                assert_eq!(refusal.message(), literal);
                assert!(
                    !refusal.message().contains("moderation."),
                    "router copy must not leak the internal id"
                );
            }
        }
    }
}

#[test]
fn policy_allows_or_refuses_per_role_with_legacy_copy() {
    let policy = policy();
    let roles = roles();
    for action in member_actions() {
        for role in &roles {
            let request = policy_request(action, role.permissions, eligible_target());
            let verdict = assert_moderation_allowed(&request, &policy);
            if role_may_invoke(role, action) {
                assert!(
                    verdict.is_ok(),
                    "{} may invoke {}: {verdict:?}",
                    role.name,
                    action.command_name(),
                );
            } else {
                assert_eq!(
                    verdict,
                    Err(PolicyError::ActorMissingPermission(action)),
                    "{} denied for {}",
                    role.name,
                    action.command_name(),
                );
                let copy = verdict.expect_err("denied").to_string();
                assert_eq!(
                    copy,
                    policy_denied_copy(action),
                    "{} policy copy for {}",
                    role.name,
                    action.command_name(),
                );
            }
        }
    }
}

#[test]
fn owen_target_stays_protected_for_every_role_and_action() {
    // Owen parity core: even a fully-permissioned admin cannot moderate Owen
    // (by configured id or by the bot's own id); the denied copy names the
    // protection rather than leaking hierarchy or permission internals.
    let policy = policy();
    for action in member_actions() {
        for target_id in [OWEN_ID, BOT_ID] {
            let mut target = eligible_target();
            target.user_id = target_id.to_owned();
            let request = policy_request(
                action,
                PERM_BAN_MEMBERS | PERM_KICK_MEMBERS | PERM_MODERATE_MEMBERS,
                target,
            );
            assert_eq!(
                assert_moderation_allowed(&request, &policy),
                Err(PolicyError::TargetOwen),
                "admin {target_id} denied for {}",
                action.command_name(),
            );
        }
    }
    assert_eq!(PolicyError::TargetOwen.to_string(), "Owen is protected",);
}
