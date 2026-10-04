//! Moderation warn-path pins (Owen parity).
//!
//! Pins the `/warn` slice end to end at the two offline enforcement layers,
//! plus the ledger shape a successful warning leaves behind:
//! - router (`InteractionRouter::route_slash`): `Moderate Members` (shared with
//!   `/timeout`) routes to the member-moderation handler; anything without it
//!   refuses with the actionable copy (Discord permission name, slash command,
//!   granter), never the internal action id.
//! - policy/executor (`MemberModerationService::execute`): the verb-agnostic
//!   member policy order (permission, target present, self-target, protection,
//!   bot hierarchy, actor hierarchy), then the audit reason, each refusal
//!   carrying its exact user-facing copy. A refused warn records nothing and
//!   never claims the idempotency key.
//! - recording: a successful warn writes exactly one warning row and one
//!   `moderation_audit` row shaped like the kick one (`moderation.warn` /
//!   `warned`), and makes zero Discord calls. Warn is the one member verb
//!   whose effect is the store row itself; `carry_out` never touches Discord.
//!
//! Warn still passes the bot-hierarchy step: the policy is verb-agnostic, so a
//! warn against a target at or above Owen's role refuses even though a warning
//! never calls Discord (see `docs/moderation-hierarchy-order.md`).
//!
//! Synthetic fixtures only: literal permission bits, synthetic snowflake ids,
//! in-memory Discord double and store. No Discord, no network, no database,
//! no guild dependency. The DB-backed warning claim tests live in
//! `member_moderation_db.rs`; this file is the layer above them.

use std::collections::HashSet;

use two_bot_core::command_permissions::{command_permission, PolicyHook};
use two_bot_core::commands::{PERM_BAN_MEMBERS, PERM_KICK_MEMBERS, PERM_MODERATE_MEMBERS};
use two_bot_core::member_moderation::{
    MemMemberStore, MemberError, MemberExecution, MemberModerationService, MemberOutcome,
    MockMemberDiscord,
};
use two_bot_core::moderation::{
    ModerationAction, ModerationActor, ModerationPolicy, ModerationTarget, PolicyError, ReasonError,
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

const WARN_DENIED_COPY: &str = "You need the Moderate Members permission to use /warn. Ask a server moderator or admin to grant it.";
const WARN_POLICY_DENIED_COPY: &str = "Missing required permission for moderation.warn";
const MISSING_TARGET_COPY: &str = "This moderation action requires a target";
const SELF_COPY: &str = "You cannot moderate yourself";
const GUILD_OWNER_COPY: &str = "The guild owner is protected";
const OWEN_COPY: &str = "Owen is protected";
const BOT_COPY: &str = "Bots are protected";
const STAFF_COPY: &str = "Staff roles are protected";
const BOT_HIERARCHY_COPY: &str = "The target is equal to or above Owen's highest role";
const ACTOR_HIERARCHY_COPY: &str = "The target is equal to or above your highest role";
const EMPTY_REASON_COPY: &str = "\"reason\" must be a non-empty string";
const LONG_REASON_COPY: &str = "\"reason\" is longer than 512 characters";

fn router() -> InteractionRouter {
    InteractionRouter::new(RouterGates {
        configured_guild: Some(GUILD),
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

fn warn_execution() -> MemberExecution {
    warn_execution_keyed("req-warn")
}

fn warn_execution_keyed(key: &str) -> MemberExecution {
    MemberExecution {
        action: ModerationAction::Warn,
        guild_id: GUILD_ID.to_owned(),
        actor: actor_with(PERM_MODERATE_MEMBERS, 50),
        target: Some(plain_target(10)),
        bot_highest_role_position: Some(100),
        reason: "  repeated spam in #general  ".to_owned(),
        duration_seconds: None,
        request_id: key.to_owned(),
        idempotency_key: key.to_owned(),
    }
}

fn service_with(
    discord: MockMemberDiscord,
    store: MemMemberStore,
) -> MemberModerationService<MockMemberDiscord, MemMemberStore, fn() -> i64> {
    MemberModerationService::new(discord, store, policy(), || 1_700_000_000_000)
}

/// Nothing was recorded and Discord was never called.
fn assert_untouched(discord: &MockMemberDiscord, store: &MemMemberStore) {
    assert!(discord.calls().is_empty(), "warn never calls Discord");
    assert!(store.warnings().is_empty(), "no warning row recorded");
    assert!(store.audits().is_empty(), "no audit row recorded");
}

#[test]
fn warn_gates_exactly_the_moderate_members_bit_shared_with_timeout() {
    assert_eq!(
        ModerationAction::Warn.required_permission(),
        PERM_MODERATE_MEMBERS
    );
    assert_eq!(PERM_MODERATE_MEMBERS, 1 << 40);
    assert_eq!(
        ModerationAction::Warn.required_permission(),
        ModerationAction::Timeout.required_permission(),
        "warn shares the timeout gate"
    );
    assert!(ModerationAction::Warn.targets_member());
    assert_eq!(ModerationAction::Warn.command_name(), "warn");
    assert_eq!(ModerationAction::Warn.action_name(), "moderation.warn");
    assert_eq!(
        ModerationAction::Warn.discord_permission_name(),
        "Moderate Members"
    );

    let row = command_permission("warn").expect("warn has a permission row");
    assert_eq!(row.required_permissions, PERM_MODERATE_MEMBERS);
    assert_eq!(row.policy_hook, Some(PolicyHook::MemberModeration));
}

#[test]
fn warn_with_the_bit_routes_to_member_moderation() {
    // The bit alone and every-bit administrators both route; the handler is
    // the member-moderation one, not a channel or custom surface.
    for permissions in [PERM_MODERATE_MEMBERS, u64::MAX] {
        let outcome = router().route_slash(&SlashContext {
            name: "warn",
            guild_id: Some(GUILD),
            actor_permissions: Some(permissions),
            custom_row: None,
        });
        assert_eq!(
            outcome,
            SlashOutcome::Handled {
                handler: HandlerId::Moderation(ModerationAction::Warn),
            },
            "permissions {permissions:#x} must route /warn"
        );
    }
}

#[test]
fn warn_without_the_bit_refuses_with_actionable_copy() {
    // No bits, the other member-moderation bits (Kick/Ban are not the warn
    // bit), and missing bits (DM-style) all refuse; the copy names the Discord
    // permission and who grants it, never an internal id.
    for permissions in [
        Some(0),
        Some(PERM_KICK_MEMBERS),
        Some(PERM_BAN_MEMBERS),
        Some(PERM_KICK_MEMBERS | PERM_BAN_MEMBERS),
        None,
    ] {
        let outcome = router().route_slash(&SlashContext {
            name: "warn",
            guild_id: Some(GUILD),
            actor_permissions: permissions,
            custom_row: None,
        });
        let refusal = match outcome {
            SlashOutcome::Refuse { refusal } => refusal,
            other => panic!("warn with {permissions:?} must refuse, got {other:?}"),
        };
        assert_eq!(
            refusal,
            RouterRefusal::ModerationPermission(ModerationAction::Warn),
            "warn with {permissions:?}"
        );
        assert_eq!(
            refusal.message(),
            WARN_DENIED_COPY,
            "warn with {permissions:?}"
        );
        assert!(
            !refusal.message().contains("moderation."),
            "router copy must not leak the internal id"
        );
    }
}

#[test]
fn warn_guild_fence_and_disabled_gate_refuse_before_the_permission() {
    // Matrix row 7 lists `GuildRestricted` and `ModerationDisabled`: both
    // refuse a fully-permissioned invoker, so neither depends on the bit.
    let foreign = router().route_slash(&SlashContext {
        name: "warn",
        guild_id: Some(9999),
        actor_permissions: Some(u64::MAX),
        custom_row: None,
    });
    assert_eq!(
        foreign,
        SlashOutcome::Refuse {
            refusal: RouterRefusal::GuildRestricted,
        }
    );

    let off = InteractionRouter::new(RouterGates {
        moderation: false,
        configured_guild: Some(GUILD),
        scorecard: false,
        automations: false,
        announcements: false,
        voice: false,
        voice_assistant: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    });
    let disabled = off.route_slash(&SlashContext {
        name: "warn",
        guild_id: Some(GUILD),
        actor_permissions: Some(u64::MAX),
        custom_row: None,
    });
    assert_eq!(
        disabled,
        SlashOutcome::Refuse {
            refusal: RouterRefusal::ModerationDisabled,
        }
    );
}

#[tokio::test]
async fn warn_executor_refuses_missing_permission_with_legacy_copy() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    for permissions in [0, PERM_KICK_MEMBERS, PERM_BAN_MEMBERS] {
        let mut exec = warn_execution();
        exec.actor.permissions = permissions;
        let err = svc
            .execute(&exec)
            .await
            .expect_err("warn without the bit must refuse");
        assert_eq!(
            err,
            MemberError::Policy(PolicyError::ActorMissingPermission(ModerationAction::Warn)),
            "permissions {permissions:#x}"
        );
        assert_eq!(err.to_string(), WARN_POLICY_DENIED_COPY);
    }
    assert_untouched(&discord, &store);
}

#[tokio::test]
async fn warn_policy_order_peels_one_refusal_at_a_time() {
    // Every refusal condition holds at once; each step repairs only the
    // refusal just asserted, so the next error proves the implemented order:
    // permission -> target present -> self-target -> protection (guild owner,
    // Owen, bot, staff role) -> bot hierarchy -> actor hierarchy -> reason.
    // Policy is verb-agnostic (`assert_moderation_allowed`), so warn runs the
    // bot-hierarchy step although it never calls Discord.
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());

    let mut exec = warn_execution();
    exec.actor.permissions = 0;
    exec.target = None;
    exec.bot_highest_role_position = Some(10);
    exec.reason = "   ".to_owned();

    let expect = |label: &str, err: MemberError, want: MemberError, copy: &str| {
        assert_eq!(err, want, "{label}");
        assert_eq!(err.to_string(), copy, "{label}");
    };

    // 1. Permission beats a missing target, a failing hierarchy and a blank reason.
    let err = svc.execute(&exec).await.expect_err("permission first");
    expect(
        "permission",
        err,
        MemberError::Policy(PolicyError::ActorMissingPermission(ModerationAction::Warn)),
        WARN_POLICY_DENIED_COPY,
    );

    // 2. Target presence.
    exec.actor.permissions = PERM_MODERATE_MEMBERS;
    let err = svc.execute(&exec).await.expect_err("target presence");
    expect(
        "target presence",
        err,
        MemberError::Policy(PolicyError::MissingTarget),
        MISSING_TARGET_COPY,
    );

    // 3. Self-target beats every protection flag on the same target.
    exec.target = Some(ModerationTarget {
        user_id: ACTOR_ID.to_owned(),
        role_ids: vec![STAFF_ROLE_ID.to_owned()],
        highest_role_position: 60,
        is_bot: true,
        is_guild_owner: true,
    });
    let err = svc.execute(&exec).await.expect_err("self-target");
    expect(
        "self-target",
        err,
        MemberError::Policy(PolicyError::TargetSelf),
        SELF_COPY,
    );

    // 4. Guild owner beats Owen, bot and staff protection.
    let target = exec.target.as_mut().expect("target");
    target.user_id = OWEN_ID.to_owned();
    let err = svc.execute(&exec).await.expect_err("guild owner");
    expect(
        "guild owner",
        err,
        MemberError::Policy(PolicyError::TargetGuildOwner),
        GUILD_OWNER_COPY,
    );

    // 5. Owen beats the bot flag and staff role; the bot's own user id is
    //    protected as Owen too.
    exec.target.as_mut().expect("target").is_guild_owner = false;
    let err = svc.execute(&exec).await.expect_err("Owen");
    expect(
        "Owen",
        err,
        MemberError::Policy(PolicyError::TargetOwen),
        OWEN_COPY,
    );
    exec.target.as_mut().expect("target").user_id = BOT_ID.to_owned();
    let err = svc.execute(&exec).await.expect_err("bot user id");
    expect(
        "bot user id",
        err,
        MemberError::Policy(PolicyError::TargetOwen),
        OWEN_COPY,
    );

    // 6. Another bot account beats the staff role.
    exec.target.as_mut().expect("target").user_id = TARGET_ID.to_owned();
    let err = svc.execute(&exec).await.expect_err("bot target");
    expect(
        "bot target",
        err,
        MemberError::Policy(PolicyError::TargetBot),
        BOT_COPY,
    );

    // 7. Staff role.
    exec.target.as_mut().expect("target").is_bot = false;
    let err = svc.execute(&exec).await.expect_err("staff role");
    expect(
        "staff role",
        err,
        MemberError::Policy(PolicyError::TargetStaffRole),
        STAFF_COPY,
    );

    // 8. Bot hierarchy (target 60, Owen 10, invoker 50: both sides fail, the
    //    bot side is named).
    exec.target.as_mut().expect("target").role_ids.clear();
    let err = svc.execute(&exec).await.expect_err("bot hierarchy");
    expect(
        "bot hierarchy",
        err,
        MemberError::Policy(PolicyError::BotHierarchy),
        BOT_HIERARCHY_COPY,
    );

    // 9. Actor hierarchy (Owen now clears the target, the invoker does not).
    exec.bot_highest_role_position = Some(100);
    let err = svc.execute(&exec).await.expect_err("actor hierarchy");
    expect(
        "actor hierarchy",
        err,
        MemberError::Policy(PolicyError::ActorHierarchy),
        ACTOR_HIERARCHY_COPY,
    );

    // 10. Reason is checked after the whole policy gate.
    exec.target.as_mut().expect("target").highest_role_position = 10;
    let err = svc.execute(&exec).await.expect_err("reason");
    expect(
        "reason",
        err,
        MemberError::Reason(ReasonError::Empty),
        EMPTY_REASON_COPY,
    );

    // None of the ten refusals recorded, called Discord, or claimed the key.
    assert_untouched(&discord, &store);

    // 11. Repaired: the same key now executes for real (not `InFlight`, not
    //     `KeyMismatch`, not a replay).
    exec.reason = "repeated spam".to_owned();
    let result = svc.execute(&exec).await.expect("repaired warn executes");
    assert_eq!(result.outcome, MemberOutcome::Warned);
    assert!(!result.replayed);
}

#[tokio::test]
async fn warn_bot_hierarchy_applies_only_when_owens_role_is_known() {
    // Warn never calls Discord, yet the shared policy still compares the
    // target with Owen's highest role when it is known and skips the compare
    // when it is not.
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());

    let mut known = warn_execution_keyed("req-warn-known");
    known.bot_highest_role_position = Some(10);
    let err = svc
        .execute(&known)
        .await
        .expect_err("target at Owen's role refuses");
    assert_eq!(err, MemberError::Policy(PolicyError::BotHierarchy));
    assert_eq!(err.to_string(), BOT_HIERARCHY_COPY);
    assert_untouched(&discord, &store);

    let mut unknown = warn_execution_keyed("req-warn-unknown");
    unknown.bot_highest_role_position = None;
    let result = svc
        .execute(&unknown)
        .await
        .expect("unknown Owen role skips the bot compare");
    assert_eq!(result.outcome, MemberOutcome::Warned);
    assert_eq!(store.warnings().len(), 1);
    assert!(discord.calls().is_empty());
}

#[tokio::test]
async fn warn_success_records_one_warning_and_one_audit_with_no_discord_call() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let result = svc
        .execute(&warn_execution())
        .await
        .expect("eligible warn executes");
    assert_eq!(result.outcome, MemberOutcome::Warned);
    assert!(!result.replayed);

    // Zero Discord calls of any kind.
    assert!(discord.calls().is_empty());
    for method in ["ban", "unban", "kick", "timeout"] {
        assert_eq!(discord.call_count(method), 0, "{method}");
    }

    // Exactly one warning row: (warning_id, guild, user, actor, reason,
    // request_id). The reason is trimmed.
    let warnings = store.warnings();
    assert_eq!(warnings.len(), 1);
    assert_eq!(
        warnings[0],
        (
            "req-warn".to_owned(),
            GUILD_ID.to_owned(),
            TARGET_ID.to_owned(),
            ACTOR_ID.to_owned(),
            "repeated spam in #general".to_owned(),
            "req-warn".to_owned(),
        )
    );

    // Exactly one audit row, shaped like the kick one.
    let audits = store.audits();
    assert_eq!(audits.len(), 1);
    let row = &audits[0];
    assert_eq!(row.action, ModerationAction::Warn.action_name());
    assert_eq!(row.action, "moderation.warn");
    assert_eq!(row.outcome, MemberOutcome::Warned.as_str());
    assert_eq!(row.outcome, "warned");
    assert_eq!(row.guild_id, GUILD_ID);
    assert_eq!(row.actor_id, ACTOR_ID);
    assert_eq!(row.target_id.as_deref(), Some(TARGET_ID));
    assert_eq!(row.request_id, "req-warn");
    assert_eq!(row.idempotency_key, "req-warn");
    assert_eq!(row.reason, "repeated spam in #general");
    let metadata: serde_json::Value =
        serde_json::from_str(&row.metadata_json).expect("audit metadata is JSON");
    assert_eq!(
        metadata,
        serde_json::json!({ "duration_seconds": null }),
        "warn carries no duration"
    );
}

#[tokio::test]
async fn warn_blank_reason_is_refused_and_records_nothing() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    for (i, blank) in ["", " ", "   ", "\t\n\r  "].into_iter().enumerate() {
        let mut exec = warn_execution_keyed(&format!("req-warn-blank-{i}"));
        exec.reason = blank.to_owned();
        let err = svc
            .execute(&exec)
            .await
            .expect_err("blank reason must refuse");
        assert_eq!(
            err,
            MemberError::Reason(ReasonError::Empty),
            "reason {blank:?}"
        );
        assert_eq!(err.to_string(), EMPTY_REASON_COPY);
    }
    assert_untouched(&discord, &store);
}

#[tokio::test]
async fn warn_reason_is_trimmed_and_capped_at_512_utf16_units() {
    // Accepted edges: exactly 512 ASCII units; 256 astral characters (two
    // UTF-16 units each, 512 total, only 256 chars); 512 two-byte characters
    // (one unit each); and 512 units padded with whitespace, which trim
    // removes before counting.
    let astral = "😀".repeat(256);
    let two_byte = "é".repeat(512);
    let ascii = "a".repeat(512);
    let padded = format!("  \t{ascii}\n  ");
    let accepted = [
        ("ascii-512", ascii.clone(), ascii.clone()),
        ("astral-256", astral.clone(), astral.clone()),
        ("two-byte-512", two_byte.clone(), two_byte.clone()),
        ("padded-512", padded, ascii.clone()),
    ];
    for (label, reason, stored) in accepted {
        let discord = MockMemberDiscord::new();
        let store = MemMemberStore::new();
        let svc = service_with(discord.clone(), store.clone());
        let mut exec = warn_execution_keyed(&format!("req-warn-{label}"));
        exec.reason = reason;
        let result = svc
            .execute(&exec)
            .await
            .unwrap_or_else(|err| panic!("{label} must be accepted: {err}"));
        assert_eq!(result.outcome, MemberOutcome::Warned, "{label}");
        assert_eq!(store.warnings()[0].4, stored, "{label}: warning reason");
        assert_eq!(store.audits()[0].reason, stored, "{label}: audit reason");
        assert!(discord.calls().is_empty(), "{label}");
    }

    // Refused edges: 513 ASCII units, and 257 astral characters (514 units
    // although only 257 chars, which `chars().count()` would wrongly accept).
    for (label, reason) in [
        ("ascii-513", "a".repeat(513)),
        ("astral-257", "😀".repeat(257)),
        ("padded-513", format!("  {}  ", "a".repeat(513))),
    ] {
        let discord = MockMemberDiscord::new();
        let store = MemMemberStore::new();
        let svc = service_with(discord.clone(), store.clone());
        let mut exec = warn_execution_keyed(&format!("req-warn-{label}"));
        exec.reason = reason;
        let err = svc
            .execute(&exec)
            .await
            .expect_err("over-long reason must refuse");
        assert_eq!(err, MemberError::Reason(ReasonError::TooLong), "{label}");
        assert_eq!(err.to_string(), LONG_REASON_COPY, "{label}");
        assert_untouched(&discord, &store);
    }
}

#[tokio::test]
async fn warn_same_key_retry_replays_without_second_warning_or_audit() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let exec = warn_execution();
    let first = svc.execute(&exec).await.expect("eligible warn executes");
    assert_eq!(first.outcome, MemberOutcome::Warned);
    assert!(!first.replayed);

    // Same request retried under the same key: the stored outcome replays and
    // the ledger keeps its single warning row and single audit row.
    let second = svc.execute(&exec).await.expect("retry replays");
    assert_eq!(second.outcome, MemberOutcome::Warned);
    assert!(second.replayed);

    assert_eq!(store.warnings().len(), 1);
    assert_eq!(store.audits().len(), 1);
    assert!(discord.calls().is_empty());
}

#[tokio::test]
async fn warn_same_key_with_different_reason_is_a_key_mismatch_not_a_second_warning() {
    // The request hash binds the (trimmed) reason: reusing a key for a
    // different reason is a caller bug, named as such, and records nothing.
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    svc.execute(&warn_execution())
        .await
        .expect("eligible warn executes");

    let mut other = warn_execution();
    other.reason = "harassment".to_owned();
    let err = svc
        .execute(&other)
        .await
        .expect_err("key reused for a different reason must refuse");
    assert_eq!(err, MemberError::KeyMismatch);
    assert_eq!(
        err.to_string(),
        "this idempotency key was used for a different moderation request"
    );

    assert_eq!(store.warnings().len(), 1);
    assert_eq!(store.audits().len(), 1);
    assert!(discord.calls().is_empty());
}

#[tokio::test]
async fn warn_fresh_key_second_warning_records_again() {
    // Warning twice is two warnings: a genuinely new request (fresh key and
    // request id) executes and records again instead of replaying or refusing.
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let first = svc
        .execute(&warn_execution())
        .await
        .expect("eligible warn executes");
    assert_eq!(first.outcome, MemberOutcome::Warned);
    assert!(!first.replayed);

    let second = svc
        .execute(&warn_execution_keyed("req-warn-again"))
        .await
        .expect("second warn executes");
    assert_eq!(second.outcome, MemberOutcome::Warned);
    assert!(!second.replayed);

    let warnings = store.warnings();
    assert_eq!(warnings.len(), 2);
    assert_eq!(warnings[0].5, "req-warn");
    assert_eq!(warnings[1].5, "req-warn-again");
    assert!(warnings.iter().all(|w| w.2 == TARGET_ID));
    let audits = store.audits();
    assert_eq!(audits.len(), 2);
    assert!(audits.iter().all(|a| a.outcome == "warned"));
    assert!(discord.calls().is_empty());
}
