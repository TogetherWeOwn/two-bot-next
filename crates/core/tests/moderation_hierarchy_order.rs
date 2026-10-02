//! Moderation hierarchy refusal-order acceptance (TOG-12626).
//!
//! Parity: the moderation policy gate (`assert_moderation_allowed`, legacy
//! `assertModerationAllowed`; see `docs/parity.md` and
//! `docs/moderation-hierarchy-order.md`).
//!
//! The gate refuses in a fixed order: actor permission first, then — for
//! member-targeted verbs only — target presence, self-moderation, target
//! protection, bot hierarchy, and actor hierarchy last. Every case below sets
//! up the *simultaneous* condition (both hierarchy comparisons failing, or a
//! pre-hierarchy refusal combined with failing hierarchy positions), so the
//! asserted error pins the order rather than a lone failure. The
//! single-failure rows stay pinned inline in `moderation.rs`
//! (`policy_refusals_match_legacy`) and are deliberately not repeated here.
//!
//! Synthetic fixtures only: no Discord, network, or database.

use std::collections::HashSet;

use two_bot_core::commands::{
    PERM_BAN_MEMBERS, PERM_KICK_MEMBERS, PERM_MANAGE_CHANNELS, PERM_MANAGE_MESSAGES,
    PERM_MODERATE_MEMBERS,
};
use two_bot_core::moderation::{
    assert_moderation_allowed, moderation_target_protection, ModerationAction, ModerationActor,
    ModerationPolicy, ModerationRequest, ModerationTarget, PolicyError, TargetProtection,
};

const OWEN_ID: &str = "123456789012345678";
const BOT_ID: &str = "555555555555555555";
const STAFF_ROLE_ID: &str = "444444444444444444";
const ACTOR_ID: &str = "111111111111111111";
const TARGET_ID: &str = "333333333333333333";

fn full_permissions() -> u64 {
    PERM_BAN_MEMBERS
        | PERM_KICK_MEMBERS
        | PERM_MODERATE_MEMBERS
        | PERM_MANAGE_MESSAGES
        | PERM_MANAGE_CHANNELS
}

fn policy() -> ModerationPolicy {
    ModerationPolicy {
        owen_user_id: OWEN_ID.to_owned(),
        protected_role_ids: HashSet::from([STAFF_ROLE_ID.to_owned()]),
        bot_user_id: Some(BOT_ID.to_owned()),
    }
}

fn actor(position: i64) -> ModerationActor {
    ModerationActor {
        user_id: ACTOR_ID.to_owned(),
        role_ids: vec![],
        highest_role_position: position,
        permissions: full_permissions(),
    }
}

fn unprotected_target(position: i64) -> ModerationTarget {
    ModerationTarget {
        user_id: TARGET_ID.to_owned(),
        role_ids: vec![],
        highest_role_position: position,
        is_bot: false,
        is_guild_owner: false,
    }
}

/// Member-targeted (`ban`) request against one target with explicit hierarchy
/// positions on both sides.
fn ban_against(
    target: ModerationTarget,
    actor_pos: i64,
    bot_pos: Option<i64>,
) -> ModerationRequest {
    ModerationRequest {
        action: ModerationAction::Ban,
        actor: actor(actor_pos),
        target: Some(target),
        bot_highest_role_position: bot_pos,
        reason: "hierarchy order acceptance".to_owned(),
        duration_seconds: None,
        count: None,
        seconds: None,
    }
}

/// Member-targeted (`ban`) request whose bot AND actor positions both sit at
/// or below the target, so both hierarchy comparisons fail together.
fn ban_with_positions(actor_pos: i64, target_pos: i64, bot_pos: Option<i64>) -> ModerationRequest {
    ban_against(unprotected_target(target_pos), actor_pos, bot_pos)
}

fn channel_request(
    action: ModerationAction,
    target: Option<ModerationTarget>,
) -> ModerationRequest {
    ModerationRequest {
        action,
        actor: actor(1),
        target,
        bot_highest_role_position: Some(1),
        reason: "hierarchy order acceptance".to_owned(),
        duration_seconds: None,
        count: None,
        seconds: None,
    }
}

#[test]
fn both_hierarchies_failing_reports_bot_first() {
    // The bot side is evaluated before the actor side (`moderation.rs`
    // order), so the margin on either side must not change the winner.
    for (actor_pos, target_pos, bot_pos) in
        [(15, 20, Some(10)), (1, 20, Some(19)), (19, 20, Some(1))]
    {
        assert_eq!(
            assert_moderation_allowed(
                &ban_with_positions(actor_pos, target_pos, bot_pos),
                &policy()
            ),
            Err(PolicyError::BotHierarchy),
            "actor {actor_pos} vs target {target_pos} with bot {bot_pos:?}: bot refusal must win",
        );
    }
}

#[test]
fn equal_positions_refuse_bot_first() {
    // Equal-or-above refuses on both sides; with both sides equal at once the
    // bot side still wins.
    assert_eq!(
        assert_moderation_allowed(&ban_with_positions(25, 25, Some(25)), &policy()),
        Err(PolicyError::BotHierarchy),
    );
    // An equal actor paired with a failing bot is the same simultaneous
    // condition from the other margin: still the bot refusal.
    assert_eq!(
        assert_moderation_allowed(&ban_with_positions(30, 30, Some(5)), &policy()),
        Err(PolicyError::BotHierarchy),
    );
}

#[test]
fn missing_permission_precedes_both_hierarchies() {
    let mut request = ban_with_positions(15, 20, Some(10));
    request.actor.permissions = 0;
    assert_eq!(
        assert_moderation_allowed(&request, &policy()),
        Err(PolicyError::ActorMissingPermission(ModerationAction::Ban)),
    );
}

#[test]
fn missing_target_precedes_hierarchy() {
    // No member target on a member-targeted verb refuses before either
    // hierarchy comparison can run, even with hierarchy-failing positions.
    let request = ModerationRequest {
        action: ModerationAction::Ban,
        actor: actor(1),
        target: None,
        bot_highest_role_position: Some(1),
        reason: "hierarchy order acceptance".to_owned(),
        duration_seconds: None,
        count: None,
        seconds: None,
    };
    assert_eq!(
        assert_moderation_allowed(&request, &policy()),
        Err(PolicyError::MissingTarget),
    );
}

#[test]
fn self_target_precedes_both_hierarchies() {
    let mut request = ban_with_positions(15, 20, Some(10));
    request.target.as_mut().expect("target").user_id = ACTOR_ID.to_owned();
    assert_eq!(
        assert_moderation_allowed(&request, &policy()),
        Err(PolicyError::TargetSelf),
    );
}

#[test]
fn guild_owner_protection_precedes_both_hierarchies() {
    let mut target = unprotected_target(20);
    target.is_guild_owner = true;
    assert_eq!(
        moderation_target_protection(&target, &policy()),
        Some(TargetProtection::GuildOwner),
    );
    assert_eq!(
        assert_moderation_allowed(&ban_against(target, 15, Some(10)), &policy()),
        Err(PolicyError::TargetGuildOwner),
    );
}

#[test]
fn owen_protection_precedes_both_hierarchies() {
    let mut target = unprotected_target(20);
    target.user_id = OWEN_ID.to_owned();
    assert_eq!(
        moderation_target_protection(&target, &policy()),
        Some(TargetProtection::Owen),
    );
    assert_eq!(
        assert_moderation_allowed(&ban_against(target, 15, Some(10)), &policy()),
        Err(PolicyError::TargetOwen),
    );
}

#[test]
fn bot_target_protection_precedes_both_hierarchies() {
    let mut target = unprotected_target(20);
    target.is_bot = true;
    assert_eq!(
        moderation_target_protection(&target, &policy()),
        Some(TargetProtection::Bot),
    );
    assert_eq!(
        assert_moderation_allowed(&ban_against(target, 15, Some(10)), &policy()),
        Err(PolicyError::TargetBot),
    );
}

#[test]
fn staff_role_protection_precedes_both_hierarchies() {
    let mut target = unprotected_target(20);
    target.role_ids = vec![STAFF_ROLE_ID.to_owned()];
    assert_eq!(
        moderation_target_protection(&target, &policy()),
        Some(TargetProtection::StaffRole),
    );
    assert_eq!(
        assert_moderation_allowed(&ban_against(target, 15, Some(10)), &policy()),
        Err(PolicyError::TargetStaffRole),
    );
}

#[test]
fn channel_verbs_skip_hierarchy_entirely() {
    // Purge with no target passes even though every hierarchy position would
    // refuse a member verb.
    assert!(
        assert_moderation_allowed(&channel_request(ModerationAction::Purge, None), &policy())
            .is_ok(),
    );
    // A present target — even a protected, higher-ranked one — is ignored
    // entirely by channel verbs.
    let mut target = unprotected_target(999);
    target.is_guild_owner = true;
    assert!(assert_moderation_allowed(
        &channel_request(ModerationAction::Purge, Some(target)),
        &policy(),
    )
    .is_ok(),);
    // Slowmode behaves the same way.
    assert!(assert_moderation_allowed(
        &channel_request(ModerationAction::Slowmode, None),
        &policy()
    )
    .is_ok(),);
}
