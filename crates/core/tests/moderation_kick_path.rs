//! Moderation kick-path pins (Owen parity).
//!
//! Pins the `/kick` slice end to end at the two offline enforcement layers,
//! plus the ledger shape a successful kick leaves behind:
//! - router (`InteractionRouter::route_slash`): the `Kick Members` bit routes
//!   to the member-moderation handler; anything without it refuses with the
//!   actionable copy (Discord permission name, slash command, granter).
//! - policy/executor (`MemberModerationService::execute`): the same bit plus
//!   bot-then-actor hierarchy, each refusal carrying its exact user-facing
//!   copy. A policy-refused kick writes a denied audit without touching Discord
//!   or claiming the idempotency key.
//! - audit: a successful kick writes exactly one `moderation_audit` row shaped
//!   like legacy (`moderation.kick` / `kicked`, guild/actor/target/request and
//!   idempotency binding, trimmed reason, duration-free metadata).
//!
//! Synthetic fixtures only: literal permission bits, synthetic snowflake ids,
//! in-memory Discord double and store. No Discord, no network, no database,
//! no guild dependency. Member `kick` keeps the pre-existing moderation path;
//! the router answers every `/kick` and delegates to the voice vote only after
//! moderation refuses an invoker who shares the target's room.

use std::collections::HashSet;

use two_bot_core::commands::{PERM_KICK_MEMBERS, PERM_MODERATE_MEMBERS};
use two_bot_core::member_moderation::{
    MemMemberStore, MemberError, MemberExecution, MemberModerationService, MemberOutcome,
    MockMemberDiscord,
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

const KICK_DENIED_COPY: &str = "You need the Kick Members permission to use /kick. Ask a server moderator or admin to grant it.";
const KICK_POLICY_DENIED_COPY: &str = "Missing required permission for moderation.kick";
const BOT_HIERARCHY_COPY: &str = "The target is equal to or above Owen's highest role";
const ACTOR_HIERARCHY_COPY: &str = "The target is equal to or above your highest role";

fn router() -> InteractionRouter {
    InteractionRouter::new(RouterGates {
        configured_guild: Some(GUILD),
        scorecard: true,
        automations: true,
        announcements: true,
        moderation: true,
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

fn kick_execution() -> MemberExecution {
    MemberExecution {
        action: ModerationAction::Kick,
        guild_id: GUILD_ID.to_owned(),
        actor: actor_with(PERM_KICK_MEMBERS, 50),
        target: Some(plain_target(10)),
        bot_highest_role_position: 100,
        reason: "spam in #general".to_owned(),
        duration_seconds: None,
        request_id: "req-kick".to_owned(),
        idempotency_key: "req-kick".to_owned(),
    }
}

fn service_with(
    discord: MockMemberDiscord,
    store: MemMemberStore,
) -> MemberModerationService<MockMemberDiscord, MemMemberStore, fn() -> i64> {
    MemberModerationService::new(discord, store, policy(), || 1_700_000_000_000)
}

fn assert_policy_denial(store: &MemMemberStore) {
    let audits = store.audits();
    assert_eq!(audits.len(), 1);
    let row = &audits[0];
    assert_eq!(row.action, "moderation.kick");
    assert_eq!(row.outcome, "denied");
    assert_eq!(row.request_id, "req-kick:denied");
    assert_eq!(row.idempotency_key, "req-kick");
    assert_eq!(row.reason, "Member moderation policy refused the request");
    let metadata: serde_json::Value =
        serde_json::from_str(&row.metadata_json).expect("audit metadata is JSON");
    assert_eq!(
        metadata,
        serde_json::json!({ "stage": "policy", "request_id": "req-kick" })
    );
}

#[test]
fn kick_gates_exactly_the_kick_members_bit() {
    assert_eq!(
        ModerationAction::Kick.required_permission(),
        PERM_KICK_MEMBERS
    );
    assert_eq!(PERM_KICK_MEMBERS, 1 << 1);
}

#[test]
fn kick_with_the_bit_routes_to_member_moderation() {
    let outcome = router().route_slash(&SlashContext {
        name: "kick",
        guild_id: Some(GUILD),
        actor_permissions: Some(PERM_KICK_MEMBERS),
        custom_row: None,
    });
    assert_eq!(
        outcome,
        SlashOutcome::Handled {
            handler: HandlerId::Moderation(ModerationAction::Kick),
        }
    );
}

#[test]
fn kick_without_the_bit_refuses_with_actionable_copy() {
    // No bits, helper bits (Moderate Members only — the timeout/warn bit is
    // not the kick bit), and missing bits (DM-style) all refuse; the copy
    // names the Discord permission and who grants it, never an internal id.
    for permissions in [Some(0), Some(PERM_MODERATE_MEMBERS), None] {
        let outcome = router().route_slash(&SlashContext {
            name: "kick",
            guild_id: Some(GUILD),
            actor_permissions: permissions,
            custom_row: None,
        });
        let refusal = match outcome {
            SlashOutcome::Refuse { refusal } => refusal,
            other => panic!("kick with {permissions:?} must refuse, got {other:?}"),
        };
        assert_eq!(
            refusal,
            RouterRefusal::ModerationPermission(ModerationAction::Kick),
            "kick with {permissions:?}"
        );
        assert_eq!(
            refusal.message(),
            KICK_DENIED_COPY,
            "kick with {permissions:?}"
        );
        assert!(
            !refusal.message().contains("moderation."),
            "router copy must not leak the internal id"
        );
    }
}

#[tokio::test]
async fn kick_executor_refuses_missing_permission_with_legacy_copy() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let mut exec = kick_execution();
    exec.actor.permissions = 0;
    let err = svc
        .execute(&exec)
        .await
        .expect_err("kick without the bit must refuse");
    assert_eq!(
        err,
        MemberError::Policy(PolicyError::ActorMissingPermission(ModerationAction::Kick))
    );
    assert_eq!(err.to_string(), KICK_POLICY_DENIED_COPY);
    assert_eq!(discord.call_count("kick"), 0);
    assert_policy_denial(&store);
}

#[tokio::test]
async fn kick_hierarchy_denials_name_the_rank_with_exact_copy() {
    // Bot side fails: Owen's own role does not clear the target.
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let mut exec = kick_execution();
    exec.bot_highest_role_position = 10;
    let err = svc
        .execute(&exec)
        .await
        .expect_err("kick above Owen's role must refuse");
    assert_eq!(err, MemberError::Policy(PolicyError::BotHierarchy));
    assert_eq!(err.to_string(), BOT_HIERARCHY_COPY);

    // Actor side fails: the invoker's role does not clear the target.
    let mut exec = kick_execution();
    exec.target
        .as_mut()
        .expect("kick target")
        .highest_role_position = 50;
    let err = svc
        .execute(&exec)
        .await
        .expect_err("kick above the invoker's role must refuse");
    assert_eq!(err, MemberError::Policy(PolicyError::ActorHierarchy));
    assert_eq!(err.to_string(), ACTOR_HIERARCHY_COPY);

    // Both sides fail at once: the bot refusal wins (policy order).
    let mut exec = kick_execution();
    exec.bot_highest_role_position = 10;
    exec.target
        .as_mut()
        .expect("kick target")
        .highest_role_position = 60;
    let err = svc
        .execute(&exec)
        .await
        .expect_err("double hierarchy failure must refuse");
    assert_eq!(err, MemberError::Policy(PolicyError::BotHierarchy));
    assert_eq!(err.to_string(), BOT_HIERARCHY_COPY);

    // Repeated policy refusals never mutate and share one denied audit row.
    assert_eq!(discord.call_count("kick"), 0);
    assert_policy_denial(&store);
}

#[tokio::test]
async fn kick_success_writes_exact_audit_event() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let result = svc
        .execute(&kick_execution())
        .await
        .expect("eligible kick executes");
    assert_eq!(result.outcome, MemberOutcome::Kicked);
    assert!(!result.replayed);

    assert_eq!(discord.call_count("kick"), 1);
    assert_eq!(discord.calls().len(), 1);

    let audits = store.audits();
    assert_eq!(audits.len(), 1);
    let row = &audits[0];
    assert_eq!(row.action, ModerationAction::Kick.action_name());
    assert_eq!(row.action, "moderation.kick");
    assert_eq!(row.outcome, MemberOutcome::Kicked.as_str());
    assert_eq!(row.outcome, "kicked");
    assert_eq!(row.guild_id, GUILD_ID);
    assert_eq!(row.actor_id, ACTOR_ID);
    assert_eq!(row.target_id.as_deref(), Some(TARGET_ID));
    assert_eq!(row.request_id, "req-kick");
    assert_eq!(row.idempotency_key, "req-kick");
    assert_eq!(row.reason, "spam in #general");
    let metadata: serde_json::Value =
        serde_json::from_str(&row.metadata_json).expect("audit metadata is JSON");
    assert_eq!(
        metadata,
        serde_json::json!({ "duration_seconds": null }),
        "kick carries no duration"
    );
}

#[tokio::test]
async fn kick_same_key_retry_replays_without_second_discord_call() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let exec = kick_execution();
    let first = svc.execute(&exec).await.expect("eligible kick executes");
    assert_eq!(first.outcome, MemberOutcome::Kicked);
    assert!(!first.replayed);

    // Same request retried under the same key: the stored outcome replays,
    // Discord sees no second DELETE and the ledger keeps its single row.
    let second = svc.execute(&exec).await.expect("retry replays");
    assert_eq!(second.outcome, MemberOutcome::Kicked);
    assert!(second.replayed);

    assert_eq!(discord.call_count("kick"), 1);
    assert_eq!(store.audits().len(), 1);
}

#[tokio::test]
async fn kick_fresh_key_second_kick_still_completes() {
    // Legacy accepts 200/204/404 for the member DELETE, so kicking an
    // already-removed member completes as a no-op rather than failing. The
    // mock double always completes, standing in for that 404-accepting
    // DELETE; a genuinely new request (fresh key and request id) therefore
    // executes and records again instead of replaying or refusing.
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let first = svc
        .execute(&kick_execution())
        .await
        .expect("eligible kick executes");
    assert_eq!(first.outcome, MemberOutcome::Kicked);
    assert!(!first.replayed);

    let mut again = kick_execution();
    again.request_id = "req-kick-again".to_owned();
    again.idempotency_key = "req-kick-again".to_owned();
    let second = svc.execute(&again).await.expect("re-kick completes");
    assert_eq!(second.outcome, MemberOutcome::Kicked);
    assert!(!second.replayed);

    assert_eq!(discord.call_count("kick"), 2);
    assert_eq!(store.audits().len(), 2);
}
