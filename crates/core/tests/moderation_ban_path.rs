//! Moderation ban-path pins (Owen parity).
//!
//! Pins the `/ban` slice end to end at the two offline enforcement layers,
//! plus the ledger shape a successful ban leaves behind:
//! - router (`InteractionRouter::route_slash`): the `Ban Members` bit routes
//!   to the member-moderation handler; anything without it refuses with the
//!   actionable copy (Discord permission name, slash command, granter).
//! - policy/executor (`MemberModerationService::execute`): the same bit plus
//!   bot-then-actor hierarchy, each refusal carrying its exact user-facing
//!   copy. A refused ban never touches Discord and never writes an audit.
//! - audit: a successful ban writes exactly one `moderation_audit` row shaped
//!   like legacy (`moderation.ban` / `banned`, guild/actor/target/request and
//!   idempotency binding, trimmed reason, duration-free metadata plus the
//!   prepared ban-attempt generation).
//! - reason: a padded moderator reason reaches both the Discord PUT and the
//!   audit row trimmed.
//! - replay: retrying under the same idempotency key replays the stored
//!   outcome without a second Discord call and without a second audit row.
//!
//! Synthetic fixtures only: literal permission bits, synthetic snowflake ids,
//! in-memory Discord double and store. No Discord, no network, no database,
//! no guild dependency.

use std::collections::HashSet;

use two_bot_core::commands::{PERM_BAN_MEMBERS, PERM_KICK_MEMBERS, PERM_MODERATE_MEMBERS};
use two_bot_core::member_moderation::{
    DiscordCall, MemMemberStore, MemberError, MemberExecution, MemberModerationService,
    MemberOutcome, MockMemberDiscord,
};
use two_bot_core::moderation::{
    ModerationAction, ModerationActor, ModerationPolicy, ModerationTarget, PolicyError,
};
use two_bot_core::{
    HandlerId, InteractionRouter, RouterGates, RouterRefusal, SlashContext, SlashOutcome,
};

const GUILD: u64 = 2222;
const GUILD_ID: &str = "100000000000000001";
const OWEN_ID: &str = "123456789012345678";
const BOT_ID: &str = "555555555555555555";
const ACTOR_ID: &str = "111111111111111111";
const TARGET_ID: &str = "333333333333333333";
const STAFF_ROLE_ID: &str = "444444444444444444";

const BAN_DENIED_COPY: &str =
    "You need the Ban Members permission to use /ban. Ask a server moderator or admin to grant it.";
const BAN_POLICY_DENIED_COPY: &str = "Missing required permission for moderation.ban";
const BOT_HIERARCHY_COPY: &str = "The target is equal to or above Owen's highest role";
const ACTOR_HIERARCHY_COPY: &str = "The target is equal to or above your highest role";

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

fn actor_with(permissions: u64, position: i64) -> ModerationActor {
    ModerationActor {
        user_id: ACTOR_ID.to_owned(),
        role_ids: vec![],
        highest_role_position: position,
        permissions,
    }
}

fn plain_target(position: i64) -> ModerationTarget {
    ModerationTarget {
        user_id: TARGET_ID.to_owned(),
        role_ids: vec![],
        highest_role_position: position,
        is_bot: false,
        is_guild_owner: false,
    }
}

fn ban_execution() -> MemberExecution {
    MemberExecution {
        action: ModerationAction::Ban,
        guild_id: GUILD_ID.to_owned(),
        actor: actor_with(PERM_BAN_MEMBERS, 50),
        target: Some(plain_target(10)),
        bot_highest_role_position: Some(100),
        reason: "spam in #general".to_owned(),
        duration_seconds: None,
        request_id: "req-ban".to_owned(),
        idempotency_key: "req-ban".to_owned(),
    }
}

fn service_with(
    discord: MockMemberDiscord,
    store: MemMemberStore,
) -> MemberModerationService<MockMemberDiscord, MemMemberStore, fn() -> i64> {
    MemberModerationService::new(discord, store, policy(), || 1_700_000_000_000)
}

#[test]
fn ban_gates_exactly_the_ban_members_bit() {
    assert_eq!(
        ModerationAction::Ban.required_permission(),
        PERM_BAN_MEMBERS
    );
    assert_eq!(PERM_BAN_MEMBERS, 1 << 2);
}

#[test]
fn ban_with_the_bit_routes_to_member_moderation() {
    let outcome = router().route_slash(&SlashContext {
        name: "ban",
        guild_id: Some(GUILD),
        actor_permissions: Some(PERM_BAN_MEMBERS),
        custom_row: None,
    });
    assert_eq!(
        outcome,
        SlashOutcome::Handled {
            handler: HandlerId::Moderation(ModerationAction::Ban),
        }
    );
}

#[test]
fn ban_without_the_bit_refuses_with_actionable_copy() {
    // No bits, neighbouring member bits (Kick Members, Moderate Members) and
    // missing bits (DM-style) all refuse; the copy names the Discord
    // permission and who grants it, never an internal id.
    for permissions in [
        Some(0),
        Some(PERM_KICK_MEMBERS),
        Some(PERM_MODERATE_MEMBERS),
        None,
    ] {
        let outcome = router().route_slash(&SlashContext {
            name: "ban",
            guild_id: Some(GUILD),
            actor_permissions: permissions,
            custom_row: None,
        });
        let refusal = match outcome {
            SlashOutcome::Refuse { refusal } => refusal,
            other => panic!("ban with {permissions:?} must refuse, got {other:?}"),
        };
        assert_eq!(
            refusal,
            RouterRefusal::ModerationPermission(ModerationAction::Ban),
            "ban with {permissions:?}"
        );
        assert_eq!(
            refusal.message(),
            BAN_DENIED_COPY,
            "ban with {permissions:?}"
        );
        assert!(
            !refusal.message().contains("moderation."),
            "router copy must not leak the internal id"
        );
    }
}

#[tokio::test]
async fn ban_executor_refuses_missing_permission_with_legacy_copy() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let mut exec = ban_execution();
    exec.actor.permissions = 0;
    let err = svc
        .execute(&exec)
        .await
        .expect_err("ban without the bit must refuse");
    assert_eq!(
        err,
        MemberError::Policy(PolicyError::ActorMissingPermission(ModerationAction::Ban))
    );
    assert_eq!(err.to_string(), BAN_POLICY_DENIED_COPY);
    assert_eq!(discord.call_count("ban"), 0);
    assert!(store.audits().is_empty());
}

#[tokio::test]
async fn ban_hierarchy_denials_name_the_rank_with_exact_copy() {
    // Bot side fails: Owen's own role does not clear the target.
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let mut exec = ban_execution();
    exec.bot_highest_role_position = Some(10);
    let err = svc
        .execute(&exec)
        .await
        .expect_err("ban above Owen's role must refuse");
    assert_eq!(err, MemberError::Policy(PolicyError::BotHierarchy));
    assert_eq!(err.to_string(), BOT_HIERARCHY_COPY);

    // Actor side fails: the invoker's role does not clear the target.
    let mut exec = ban_execution();
    exec.target
        .as_mut()
        .expect("ban target")
        .highest_role_position = 50;
    let err = svc
        .execute(&exec)
        .await
        .expect_err("ban above the invoker's role must refuse");
    assert_eq!(err, MemberError::Policy(PolicyError::ActorHierarchy));
    assert_eq!(err.to_string(), ACTOR_HIERARCHY_COPY);

    // Both sides fail at once: the bot refusal wins (policy order).
    let mut exec = ban_execution();
    exec.bot_highest_role_position = Some(10);
    exec.target
        .as_mut()
        .expect("ban target")
        .highest_role_position = 60;
    let err = svc
        .execute(&exec)
        .await
        .expect_err("double hierarchy failure must refuse");
    assert_eq!(err, MemberError::Policy(PolicyError::BotHierarchy));
    assert_eq!(err.to_string(), BOT_HIERARCHY_COPY);

    // No refused ban may mutate or record.
    assert_eq!(discord.call_count("ban"), 0);
    assert!(store.audits().is_empty());
}

#[tokio::test]
async fn ban_success_writes_exact_audit_event() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let result = svc
        .execute(&ban_execution())
        .await
        .expect("eligible ban executes");
    assert_eq!(result.outcome, MemberOutcome::Banned);
    assert!(!result.replayed);

    assert_eq!(discord.call_count("ban"), 1);
    assert_eq!(discord.calls().len(), 1);

    let audits = store.audits();
    assert_eq!(audits.len(), 1);
    let row = &audits[0];
    assert_eq!(row.action, ModerationAction::Ban.action_name());
    assert_eq!(row.action, "moderation.ban");
    assert_eq!(row.outcome, MemberOutcome::Banned.as_str());
    assert_eq!(row.outcome, "banned");
    assert_eq!(row.guild_id, GUILD_ID);
    assert_eq!(row.actor_id, ACTOR_ID);
    assert_eq!(row.target_id.as_deref(), Some(TARGET_ID));
    assert_eq!(row.request_id, "req-ban");
    assert_eq!(row.idempotency_key, "req-ban");
    assert_eq!(row.reason, "spam in #general");
    let metadata: serde_json::Value =
        serde_json::from_str(&row.metadata_json).expect("audit metadata is JSON");
    assert_eq!(
        metadata,
        serde_json::json!({ "duration_seconds": null, "ban_attempt_generation": 1 }),
        "ban carries no duration and records its first prepared attempt"
    );
}

#[tokio::test]
async fn ban_reason_propagates_trimmed_to_discord_and_audit() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let mut exec = ban_execution();
    exec.reason = "  spam in #general  ".to_owned();
    let result = svc.execute(&exec).await.expect("padded reason still bans");
    assert_eq!(result.outcome, MemberOutcome::Banned);

    let calls = discord.calls();
    assert_eq!(calls.len(), 1);
    match &calls[0] {
        DiscordCall::Ban {
            guild_id,
            user_id,
            reason,
        } => {
            assert_eq!(guild_id, GUILD_ID);
            assert_eq!(user_id, TARGET_ID);
            assert_eq!(reason, "spam in #general");
        }
        other => panic!("ban must issue a ban PUT, got {other:?}"),
    }

    let audits = store.audits();
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].reason, "spam in #general");
}

#[tokio::test]
async fn ban_same_key_retry_replays_without_second_discord_call() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let exec = ban_execution();
    let first = svc.execute(&exec).await.expect("eligible ban executes");
    assert_eq!(first.outcome, MemberOutcome::Banned);
    assert!(!first.replayed);

    // Same request retried under the same key: the stored outcome replays,
    // Discord sees no second PUT and the ledger keeps its single row.
    let second = svc.execute(&exec).await.expect("retry replays");
    assert_eq!(second.outcome, MemberOutcome::Banned);
    assert!(second.replayed);

    assert_eq!(discord.call_count("ban"), 1);
    assert_eq!(store.audits().len(), 1);
}
