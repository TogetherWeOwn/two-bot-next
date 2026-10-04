//! Moderation timeout-path offline pins.
//!
//! Pins the `/timeout` slice end to end without Discord, the network, or a
//! database: the router permission gate and its denial copy, the
//! policy/duration validation bounds, and the executor's Discord call plus
//! audit-row shape (including the failure edges that must not write audit
//! rows). Synthetic fixtures only: literal permission bits, synthetic
//! snowflake ids, and an in-memory store/double.

use two_bot_core::command_permissions::{command_permission, PolicyHook};
use two_bot_core::commands::PERM_MODERATE_MEMBERS;
use two_bot_core::member_moderation::{
    validate_member_request, DiscordCall, DiscordError, MemMemberStore, MemberError,
    MemberExecution, MemberModerationService, MemberOutcome, MockMemberDiscord,
};
use two_bot_core::moderation::{
    ModerationAction, ModerationActor, ModerationPolicy, ModerationTarget,
};
use two_bot_core::{
    HandlerId, InteractionRouter, PolicyError, RouterGates, RouterRefusal, SlashContext,
    SlashOutcome,
};

const GUILD_U64: u64 = 2222;
const GUILD: &str = "100000000000000001";
const ACTOR_ID: &str = "111111111111111111";
const TARGET_ID: &str = "333333333333333333";
const OWEN_ID: &str = "123456789012345678";
const BOT_ID: &str = "555555555555555555";
const STAFF_ROLE: &str = "444444444444444444";
const NOW_MS: i64 = 1_700_000_000_000;
const UNTIL_ISO: &str = "2023-11-14T23:13:20.000Z";
const TIMEOUT_DENIAL: &str = "You need the Moderate Members permission to use /timeout. Ask a server moderator or admin to grant it.";

fn gates(moderation: bool) -> RouterGates {
    RouterGates {
        configured_guild: Some(GUILD_U64),
        scorecard: false,
        automations: false,
        announcements: false,
        moderation,
        voice: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    }
}

fn policy() -> ModerationPolicy {
    ModerationPolicy {
        owen_user_id: OWEN_ID.to_owned(),
        protected_role_ids: [STAFF_ROLE.to_owned()].into_iter().collect(),
        bot_user_id: Some(BOT_ID.to_owned()),
    }
}

fn actor() -> ModerationActor {
    ModerationActor {
        user_id: ACTOR_ID.to_owned(),
        role_ids: Vec::new(),
        highest_role_position: 50,
        permissions: PERM_MODERATE_MEMBERS,
    }
}

fn target() -> ModerationTarget {
    ModerationTarget {
        user_id: TARGET_ID.to_owned(),
        role_ids: Vec::new(),
        highest_role_position: 10,
        is_bot: false,
        is_guild_owner: false,
    }
}

fn execution(request_id: &str) -> MemberExecution {
    MemberExecution {
        action: ModerationAction::Timeout,
        guild_id: GUILD.to_owned(),
        actor: actor(),
        target: Some(target()),
        bot_highest_role_position: Some(100),
        reason: "  spam in #general  ".to_owned(),
        duration_seconds: Some(3600),
        request_id: request_id.to_owned(),
        idempotency_key: request_id.to_owned(),
    }
}

fn service(
    discord: MockMemberDiscord,
    store: MemMemberStore,
) -> MemberModerationService<MockMemberDiscord, MemMemberStore, impl Fn() -> i64 + Send + Sync> {
    MemberModerationService::new(discord, store, policy(), || NOW_MS)
}

#[test]
fn timeout_permission_row_pins_moderate_members() {
    let row = command_permission("timeout").expect("timeout has a permission row");
    assert_eq!(row.required_permissions, PERM_MODERATE_MEMBERS);
    assert_eq!(row.required_permissions, 1 << 40);
    assert_eq!(row.policy_hook, Some(PolicyHook::MemberModeration));
    assert_eq!(
        ModerationAction::Timeout.required_permission(),
        PERM_MODERATE_MEMBERS
    );
    assert!(ModerationAction::Timeout.targets_member());
    assert_eq!(ModerationAction::Timeout.command_name(), "timeout");
    assert_eq!(
        ModerationAction::Timeout.action_name(),
        "moderation.timeout"
    );
    assert_eq!(
        ModerationAction::Timeout.discord_permission_name(),
        "Moderate Members"
    );
}

#[test]
fn router_timeout_routes_with_moderate_members() {
    let router = InteractionRouter::new(gates(true));
    for permissions in [PERM_MODERATE_MEMBERS, u64::MAX] {
        let ctx = SlashContext {
            name: "timeout",
            guild_id: Some(GUILD_U64),
            actor_permissions: Some(permissions),
            custom_row: None,
        };
        assert_eq!(
            router.route_slash(&ctx),
            SlashOutcome::Handled {
                handler: HandlerId::Moderation(ModerationAction::Timeout),
            },
            "permissions {permissions:#x} must route /timeout",
        );
    }
}

#[test]
fn router_timeout_denial_copy_names_discord_permission() {
    let router = InteractionRouter::new(gates(true));
    for permissions in [Some(0), None] {
        let ctx = SlashContext {
            name: "timeout",
            guild_id: Some(GUILD_U64),
            actor_permissions: permissions,
            custom_row: None,
        };
        let refusal = match router.route_slash(&ctx) {
            SlashOutcome::Refuse { refusal } => refusal,
            other => panic!("permissions {permissions:?} must refuse /timeout, got {other:?}"),
        };
        assert_eq!(
            refusal,
            RouterRefusal::ModerationPermission(ModerationAction::Timeout)
        );
        assert_eq!(refusal.message(), TIMEOUT_DENIAL);
        assert!(
            !refusal.message().contains("moderation."),
            "denial must not leak the internal action id: {}",
            refusal.message()
        );
    }
}

#[test]
fn router_timeout_guild_fence_and_disabled_gate() {
    let router = InteractionRouter::new(gates(true));
    let foreign = SlashContext {
        name: "timeout",
        guild_id: Some(9999),
        actor_permissions: Some(u64::MAX),
        custom_row: None,
    };
    assert_eq!(
        router.route_slash(&foreign),
        SlashOutcome::Refuse {
            refusal: RouterRefusal::GuildRestricted,
        }
    );
    assert_eq!(
        RouterRefusal::GuildRestricted.message(),
        "This command is restricted to the configured guild."
    );

    let off = InteractionRouter::new(gates(false));
    let ctx = SlashContext {
        name: "timeout",
        guild_id: Some(GUILD_U64),
        actor_permissions: Some(u64::MAX),
        custom_row: None,
    };
    assert_eq!(
        off.route_slash(&ctx),
        SlashOutcome::Refuse {
            refusal: RouterRefusal::ModerationDisabled,
        }
    );
    assert_eq!(
        RouterRefusal::ModerationDisabled.message(),
        "Moderation is not enabled on this server. Ask a server admin to enable it in the bot configuration — this is a host setting, not a Discord role."
    );

    let on_names: Vec<_> = router
        .publish_set(&[])
        .expect("publish assembles")
        .iter()
        .map(|d| d.name.clone())
        .collect();
    assert!(on_names.contains(&"timeout".to_owned()));
    let off_names: Vec<_> = off
        .publish_set(&[])
        .expect("publish assembles")
        .iter()
        .map(|d| d.name.clone())
        .collect();
    assert!(!off_names.contains(&"timeout".to_owned()));
}

#[test]
fn timeout_duration_bounds_accept_edges_refuse_outsiders() {
    for good in [Some(60), Some(61), Some(2_419_199), Some(2_419_200)] {
        assert!(
            validate_member_request(
                ModerationAction::Timeout,
                &policy(),
                &actor(),
                Some(&target()),
                Some(100),
                "spam",
                good,
            )
            .is_ok(),
            "{good:?} must validate",
        );
    }
    for bad in [None, Some(59), Some(0), Some(-5), Some(2_419_201)] {
        let err = validate_member_request(
            ModerationAction::Timeout,
            &policy(),
            &actor(),
            Some(&target()),
            Some(100),
            "spam",
            bad,
        )
        .expect_err("must refuse");
        assert!(
            matches!(
                err,
                MemberError::Malformed {
                    field: "duration_seconds",
                    ..
                }
            ),
            "{bad:?} must refuse as malformed duration_seconds, got {err:?}",
        );
    }
    let err = validate_member_request(
        ModerationAction::Timeout,
        &policy(),
        &actor(),
        Some(&target()),
        Some(100),
        "spam",
        Some(59),
    )
    .expect_err("must refuse");
    assert_eq!(
        err.to_string(),
        "malformed duration_seconds: \"duration_seconds\" must be an integer between 60 and 2419200"
    );
}

/// One actor/target refusal case: mutate the request, then the expected error
/// and its literal copy.
type ActorRefusalCase = (
    fn(&mut ModerationActor, &mut Option<ModerationTarget>, &mut Option<i64>),
    PolicyError,
    &'static str,
);

/// One target-shape refusal case: mutate the target, then the expected error
/// and its literal copy.
type TargetRefusalCase = (fn(&mut ModerationTarget), PolicyError, &'static str);

#[test]
fn timeout_policy_refusals_name_the_protection() {
    let cases: [ActorRefusalCase; 2] = [
        (
            |actor, _, _| actor.permissions = 0,
            PolicyError::ActorMissingPermission(ModerationAction::Timeout),
            "Missing required permission for moderation.timeout",
        ),
        (
            |_, target, _| *target = None,
            PolicyError::MissingTarget,
            "This moderation action requires a target",
        ),
    ];
    for (mutate, expect, copy) in cases {
        let mut actor = actor();
        let mut target = Some(target());
        let mut bot = Some(100);
        mutate(&mut actor, &mut target, &mut bot);
        let err = validate_member_request(
            ModerationAction::Timeout,
            &policy(),
            &actor,
            target.as_ref(),
            bot,
            "spam",
            Some(3600),
        )
        .expect_err("must refuse");
        assert_eq!(err, MemberError::Policy(expect), "{copy}");
        assert_eq!(err.to_string(), copy);
    }

    let mut target_cases: Vec<TargetRefusalCase> = vec![
        (
            |t: &mut ModerationTarget| t.user_id = ACTOR_ID.to_owned(),
            PolicyError::TargetSelf,
            "You cannot moderate yourself",
        ),
        (
            |t: &mut ModerationTarget| t.is_guild_owner = true,
            PolicyError::TargetGuildOwner,
            "The guild owner is protected",
        ),
        (
            |t: &mut ModerationTarget| t.user_id = OWEN_ID.to_owned(),
            PolicyError::TargetOwen,
            "Owen is protected",
        ),
        (
            |t: &mut ModerationTarget| t.user_id = BOT_ID.to_owned(),
            PolicyError::TargetOwen,
            "Owen is protected",
        ),
        (
            |t: &mut ModerationTarget| t.is_bot = true,
            PolicyError::TargetBot,
            "Bots are protected",
        ),
        (
            |t: &mut ModerationTarget| t.role_ids = vec![STAFF_ROLE.to_owned()],
            PolicyError::TargetStaffRole,
            "Staff roles are protected",
        ),
        (
            |t: &mut ModerationTarget| t.highest_role_position = 50,
            PolicyError::ActorHierarchy,
            "The target is equal to or above your highest role",
        ),
    ];
    for (mutate, expect, copy) in target_cases.drain(..) {
        let mut target = target();
        mutate(&mut target);
        let err = validate_member_request(
            ModerationAction::Timeout,
            &policy(),
            &actor(),
            Some(&target),
            Some(100),
            "spam",
            Some(3600),
        )
        .expect_err("must refuse");
        assert_eq!(err, MemberError::Policy(expect), "{copy}");
        assert_eq!(err.to_string(), copy);
    }

    let err = validate_member_request(
        ModerationAction::Timeout,
        &policy(),
        &actor(),
        Some(&target()),
        Some(10),
        "spam",
        Some(3600),
    )
    .expect_err("bot hierarchy must refuse");
    assert_eq!(err, MemberError::Policy(PolicyError::BotHierarchy));
    assert_eq!(
        err.to_string(),
        "The target is equal to or above Owen's highest role"
    );
}

#[tokio::test]
async fn timeout_refused_execution_makes_no_discord_call_and_writes_no_audit() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service(discord.clone(), store.clone());

    let mut exec = execution("req-timeout-refused");
    exec.actor.permissions = 0;
    let err = svc.execute(&exec).await.expect_err("must refuse");
    assert_eq!(
        err,
        MemberError::Policy(PolicyError::ActorMissingPermission(
            ModerationAction::Timeout
        ))
    );
    assert!(discord.calls().is_empty());
    assert!(store.audits().is_empty());

    // The refusal happened before any claim, so the same key succeeds once
    // the caller presents the required permission.
    exec.actor.permissions = PERM_MODERATE_MEMBERS;
    let res = svc.execute(&exec).await.expect("retry after fix");
    assert_eq!(res.outcome, MemberOutcome::TimedOut);
    assert!(!res.replayed);
}

#[tokio::test]
async fn timeout_execute_emits_discord_call_and_audit_shape() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service(discord.clone(), store.clone());

    let res = svc
        .execute(&execution("req-timeout"))
        .await
        .expect("timeout executes");
    assert_eq!(res.outcome, MemberOutcome::TimedOut);
    assert!(!res.replayed);
    assert_eq!(MemberOutcome::TimedOut.as_str(), "timed_out");

    let calls = discord.calls();
    assert_eq!(calls.len(), 1);
    let DiscordCall::Timeout {
        guild_id,
        user_id,
        until_iso,
        reason,
    } = &calls[0]
    else {
        panic!("timeout must emit exactly one Timeout call, got {calls:?}");
    };
    assert_eq!(guild_id, GUILD);
    assert_eq!(user_id, TARGET_ID);
    assert_eq!(until_iso, UNTIL_ISO);
    assert_eq!(reason, "spam in #general");

    assert!(store.warnings().is_empty());
    let audits = store.audits();
    assert_eq!(audits.len(), 1);
    let row = &audits[0];
    assert_eq!(row.request_id, "req-timeout");
    assert_eq!(row.guild_id, GUILD);
    assert_eq!(row.actor_id, ACTOR_ID);
    assert_eq!(row.action, "moderation.timeout");
    assert_eq!(row.target_id.as_deref(), Some(TARGET_ID));
    assert_eq!(row.reason, "spam in #general");
    assert_eq!(row.outcome, "timed_out");
    assert_eq!(row.idempotency_key, "req-timeout");
    let metadata: serde_json::Value =
        serde_json::from_str(&row.metadata_json).expect("metadata is JSON");
    assert_eq!(metadata, serde_json::json!({"duration_seconds": 3600}));

    // A retry replays the stored outcome: no second Discord call, no second
    // audit row.
    let replayed = svc
        .execute(&execution("req-timeout"))
        .await
        .expect("replay");
    assert!(replayed.replayed);
    assert_eq!(replayed.outcome, MemberOutcome::TimedOut);
    assert_eq!(discord.calls().len(), 1);
    assert_eq!(store.audits().len(), 1);
}

#[tokio::test]
async fn timeout_rejected_discord_writes_no_audit_and_releases_key() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service(discord.clone(), store.clone());

    discord.fail_with("timeout", DiscordError::Rejected("unknown member".into()));
    let err = svc
        .execute(&execution("req-timeout-rejected"))
        .await
        .expect_err("must fail");
    assert!(matches!(
        err,
        MemberError::Discord(DiscordError::Rejected(_))
    ));
    assert!(store.audits().is_empty());

    discord.clear_failure("timeout");
    let res = svc
        .execute(&execution("req-timeout-rejected"))
        .await
        .expect("retry is a real second attempt");
    assert!(!res.replayed);
    assert_eq!(res.outcome, MemberOutcome::TimedOut);
    assert_eq!(discord.call_count("timeout"), 2);
    assert_eq!(store.audits().len(), 1);
}

#[tokio::test]
async fn timeout_uncertain_failure_keeps_claim_without_audit() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service(discord.clone(), store.clone());

    discord.fail_with("timeout", DiscordError::Timeout);
    let err = svc
        .execute(&execution("req-timeout-uncertain"))
        .await
        .expect_err("must fail");
    assert_eq!(err, MemberError::Discord(DiscordError::Timeout));
    assert!(store.audits().is_empty());

    discord.clear_failure("timeout");
    let err = svc
        .execute(&execution("req-timeout-uncertain"))
        .await
        .expect_err("claim stays uncertain");
    assert_eq!(err, MemberError::InFlight);
    assert_eq!(discord.call_count("timeout"), 1);
    assert!(store.audits().is_empty());
}
