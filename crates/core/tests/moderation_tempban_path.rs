//! Moderation tempban-path pins (Owen parity).
//!
//! Pins the `/tempban` slice end to end at the two offline enforcement layers,
//! plus the ledger and unban-queue shape a successful tempban leaves behind:
//! - router (`InteractionRouter::route_slash`): the `Ban Members` bit routes
//!   to the member-moderation handler; anything without it refuses with the
//!   actionable copy (Discord permission name, slash command, granter).
//! - duration: `duration_seconds` accepts the inclusive edges 60 and
//!   `MAX_TEMPBAN_SECONDS` (365 days) and refuses 59 and 365 days + 1 before
//!   any Discord call, audit row or staged unban. The command-layer validator
//!   edges live in `moderation_caps.rs::tempban_caps_at_365_days`; this file
//!   pins the executor's own copy of the same bounds and that the two agree.
//! - policy/executor (`MemberModerationService::execute`): the permission bit,
//!   target protection (self, guild owner, Owen, the bot, other bots, staff
//!   roles) and bot-then-actor hierarchy, each refusal carrying its exact
//!   user-facing copy. A refused tempban never touches Discord, never writes an
//!   audit row and never stages an unban.
//! - audit + unban queue: a successful tempban writes exactly one
//!   `moderation_audit` row (`moderation.tempban` / `temporarily_banned`, trimmed
//!   reason, duration and first ban-attempt generation in the metadata),
//!   issues one ban PUT, stages the unban BEFORE that PUT and activates it
//!   after, due at exactly `now + duration_seconds`.
//! - replay: retrying under the same idempotency key replays the stored
//!   outcome without a second Discord call, audit row or unban job.
//!
//! Synthetic fixtures only: literal permission bits, synthetic snowflake ids,
//! in-memory Discord double and store. No Discord, no network, no database,
//! no guild dependency.

use std::collections::HashSet;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use two_bot_core::commands::{
    PERM_BAN_MEMBERS, PERM_KICK_MEMBERS, PERM_MODERATE_MEMBERS, TEMPBAN_DURATION_MAX_SECONDS,
    TEMPBAN_DURATION_MIN_SECONDS,
};
use two_bot_core::member_moderation::{
    DiscordCall, DiscordError, MemMemberStore, MemberDiscord, MemberError, MemberExecution,
    MemberModerationService, MemberOutcome, MockMemberDiscord, MAX_TEMPBAN_SECONDS,
    MIN_DURATION_SECONDS,
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
const REQUEST_ID: &str = "req-tempban";

/// The pinned service clock: 2023-11-14T22:13:20Z.
const NOW_MS: i64 = 1_700_000_000_000;
const HOUR_SECONDS: i64 = 3600;

const TEMPBAN_DENIED_COPY: &str = "You need the Ban Members permission to use /tempban. Ask a server moderator or admin to grant it.";
const TEMPBAN_POLICY_DENIED_COPY: &str = "Missing required permission for moderation.tempban";
const BOT_HIERARCHY_COPY: &str = "The target is equal to or above Owen's highest role";
const ACTOR_HIERARCHY_COPY: &str = "The target is equal to or above your highest role";
const DURATION_COPY: &str =
    "malformed duration_seconds: \"duration_seconds\" must be an integer between 60 and 31536000";

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

fn tempban_execution(duration_seconds: i64) -> MemberExecution {
    MemberExecution {
        action: ModerationAction::TempBan,
        guild_id: GUILD_ID.to_owned(),
        actor: actor_with(PERM_BAN_MEMBERS, 50),
        target: Some(plain_target(10)),
        bot_highest_role_position: Some(100),
        reason: "spam in #general".to_owned(),
        duration_seconds: Some(duration_seconds),
        request_id: REQUEST_ID.to_owned(),
        idempotency_key: REQUEST_ID.to_owned(),
    }
}

fn service_with(
    discord: MockMemberDiscord,
    store: MemMemberStore,
) -> MemberModerationService<MockMemberDiscord, MemMemberStore, fn() -> i64> {
    MemberModerationService::new(discord, store, policy(), || NOW_MS)
}

/// Nothing a refused tempban may leave behind: no Discord call, no audit row,
/// no staged (or any other) unban job for the request.
fn assert_no_side_effects(discord: &MockMemberDiscord, store: &MemMemberStore, request_id: &str) {
    assert!(discord.calls().is_empty(), "refusal reached Discord");
    assert!(store.audits().is_empty(), "refusal wrote an audit row");
    assert_eq!(
        store.unban_state(request_id),
        None,
        "refusal staged an unban"
    );
    assert_eq!(
        store.ban_attempt(request_id),
        None,
        "refusal prepared a ban attempt"
    );
}

#[test]
fn tempban_gates_exactly_the_ban_members_bit() {
    assert_eq!(
        ModerationAction::TempBan.required_permission(),
        PERM_BAN_MEMBERS
    );
    assert_eq!(PERM_BAN_MEMBERS, 1 << 2);
    // Same gate as /ban, never the neighbouring member bits.
    assert_eq!(
        ModerationAction::TempBan.required_permission(),
        ModerationAction::Ban.required_permission()
    );
    assert_ne!(
        ModerationAction::TempBan.required_permission(),
        PERM_KICK_MEMBERS
    );
    assert_ne!(
        ModerationAction::TempBan.required_permission(),
        PERM_MODERATE_MEMBERS
    );
}

#[test]
fn tempban_with_the_bit_routes_to_member_moderation() {
    for permissions in [PERM_BAN_MEMBERS, PERM_BAN_MEMBERS | PERM_KICK_MEMBERS] {
        let outcome = router().route_slash(&SlashContext {
            name: "tempban",
            guild_id: Some(GUILD),
            actor_permissions: Some(permissions),
            custom_row: None,
        });
        assert_eq!(
            outcome,
            SlashOutcome::Handled {
                handler: HandlerId::Moderation(ModerationAction::TempBan),
            },
            "permissions {permissions:#x}"
        );
    }
}

#[test]
fn tempban_without_the_bit_refuses_with_actionable_copy() {
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
            name: "tempban",
            guild_id: Some(GUILD),
            actor_permissions: permissions,
            custom_row: None,
        });
        let refusal = match outcome {
            SlashOutcome::Refuse { refusal } => refusal,
            other => panic!("tempban with {permissions:?} must refuse, got {other:?}"),
        };
        assert_eq!(
            refusal,
            RouterRefusal::ModerationPermission(ModerationAction::TempBan),
            "tempban with {permissions:?}"
        );
        assert_eq!(
            refusal.message(),
            TEMPBAN_DENIED_COPY,
            "tempban with {permissions:?}"
        );
        assert!(
            !refusal.message().contains("moderation."),
            "router copy must not leak the internal id"
        );
    }
}

#[test]
fn tempban_executor_bounds_agree_with_the_command_layer() {
    // Cites `moderation_caps.rs::tempban_caps_at_365_days` for the validator
    // edges; here only the executor constants are pinned against them.
    assert_eq!(MIN_DURATION_SECONDS, 60);
    assert_eq!(MIN_DURATION_SECONDS, TEMPBAN_DURATION_MIN_SECONDS);
    assert_eq!(MAX_TEMPBAN_SECONDS, 365 * 24 * 60 * 60);
    assert_eq!(MAX_TEMPBAN_SECONDS, 31_536_000);
    assert_eq!(MAX_TEMPBAN_SECONDS, TEMPBAN_DURATION_MAX_SECONDS);
}

#[tokio::test]
async fn tempban_duration_accepts_the_inclusive_edges() {
    for seconds in [MIN_DURATION_SECONDS, MAX_TEMPBAN_SECONDS] {
        let discord = MockMemberDiscord::new();
        let store = MemMemberStore::new();
        let svc = service_with(discord.clone(), store.clone());
        let result = svc
            .execute(&tempban_execution(seconds))
            .await
            .unwrap_or_else(|err| panic!("{seconds}s tempban must execute, got {err:?}"));
        assert_eq!(
            result.outcome,
            MemberOutcome::TemporarilyBanned,
            "{seconds}s"
        );
        assert!(!result.replayed, "{seconds}s");
        assert_eq!(discord.call_count("ban"), 1, "{seconds}s");
        assert_eq!(store.audits().len(), 1, "{seconds}s");
        assert_eq!(
            store.unban_state(REQUEST_ID).as_deref(),
            Some("pending"),
            "{seconds}s"
        );
        let metadata: serde_json::Value =
            serde_json::from_str(&store.audits()[0].metadata_json).expect("audit metadata is JSON");
        assert_eq!(metadata["duration_seconds"], seconds, "{seconds}s");
    }
}

#[tokio::test]
async fn tempban_duration_outside_the_bounds_refuses_before_any_side_effect() {
    // 59 and 365 days + 1 are the adjacent refusals; zero, negative and
    // missing durations are malformed the same way.
    let outside = [
        Some(MIN_DURATION_SECONDS - 1),
        Some(MAX_TEMPBAN_SECONDS + 1),
        Some(0),
        Some(-1),
        None,
    ];
    for bad in outside {
        let discord = MockMemberDiscord::new();
        let store = MemMemberStore::new();
        let svc = service_with(discord.clone(), store.clone());
        let mut exec = tempban_execution(HOUR_SECONDS);
        exec.duration_seconds = bad;
        let err = svc
            .execute(&exec)
            .await
            .expect_err("out-of-range duration must refuse");
        assert!(
            matches!(
                err,
                MemberError::Malformed {
                    field: "duration_seconds",
                    ..
                }
            ),
            "{bad:?} must refuse as malformed duration_seconds, got {err:?}"
        );
        assert_eq!(err.to_string(), DURATION_COPY, "{bad:?}");
        assert_no_side_effects(&discord, &store, REQUEST_ID);

        // The refusal happened before any idempotency claim, so the same key
        // succeeds once the caller presents a valid duration.
        exec.duration_seconds = Some(HOUR_SECONDS);
        let result = svc.execute(&exec).await.expect("retry after fix");
        assert_eq!(result.outcome, MemberOutcome::TemporarilyBanned, "{bad:?}");
        assert!(!result.replayed, "{bad:?}");
    }
}

#[tokio::test]
async fn tempban_permission_refusal_wins_over_a_bad_duration() {
    // Policy runs first and the duration bound last, so an unprivileged caller
    // learns about the permission and not about the duration.
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let mut exec = tempban_execution(MIN_DURATION_SECONDS - 1);
    exec.actor.permissions = 0;
    let err = svc
        .execute(&exec)
        .await
        .expect_err("unprivileged tempban must refuse");
    assert_eq!(
        err,
        MemberError::Policy(PolicyError::ActorMissingPermission(
            ModerationAction::TempBan
        ))
    );
    assert_eq!(err.to_string(), TEMPBAN_POLICY_DENIED_COPY);
    assert_no_side_effects(&discord, &store, REQUEST_ID);
}

/// One refusal case: mutate the request, then the expected error and its
/// literal copy.
type RefusalCase = (
    &'static str,
    fn(&mut MemberExecution),
    PolicyError,
    &'static str,
);

fn policy_refusal_cases() -> Vec<RefusalCase> {
    vec![
        (
            "missing permission",
            |exec: &mut MemberExecution| exec.actor.permissions = 0,
            PolicyError::ActorMissingPermission(ModerationAction::TempBan),
            TEMPBAN_POLICY_DENIED_COPY,
        ),
        (
            "neighbouring Kick Members bit",
            |exec: &mut MemberExecution| exec.actor.permissions = PERM_KICK_MEMBERS,
            PolicyError::ActorMissingPermission(ModerationAction::TempBan),
            TEMPBAN_POLICY_DENIED_COPY,
        ),
        (
            "missing target",
            |exec: &mut MemberExecution| exec.target = None,
            PolicyError::MissingTarget,
            "This moderation action requires a target",
        ),
        (
            "self",
            |exec: &mut MemberExecution| {
                exec.target.as_mut().expect("target").user_id = ACTOR_ID.to_owned()
            },
            PolicyError::TargetSelf,
            "You cannot moderate yourself",
        ),
        (
            "guild owner",
            |exec: &mut MemberExecution| {
                exec.target.as_mut().expect("target").is_guild_owner = true
            },
            PolicyError::TargetGuildOwner,
            "The guild owner is protected",
        ),
        (
            "Owen's user id",
            |exec: &mut MemberExecution| {
                exec.target.as_mut().expect("target").user_id = OWEN_ID.to_owned()
            },
            PolicyError::TargetOwen,
            "Owen is protected",
        ),
        (
            "the bot's own user id",
            |exec: &mut MemberExecution| {
                exec.target.as_mut().expect("target").user_id = BOT_ID.to_owned()
            },
            PolicyError::TargetOwen,
            "Owen is protected",
        ),
        (
            "another bot",
            |exec: &mut MemberExecution| exec.target.as_mut().expect("target").is_bot = true,
            PolicyError::TargetBot,
            "Bots are protected",
        ),
        (
            "staff role",
            |exec: &mut MemberExecution| {
                exec.target.as_mut().expect("target").role_ids = vec![STAFF_ROLE_ID.to_owned()]
            },
            PolicyError::TargetStaffRole,
            "Staff roles are protected",
        ),
        (
            "target at the invoker's rank",
            |exec: &mut MemberExecution| {
                exec.target.as_mut().expect("target").highest_role_position = 50
            },
            PolicyError::ActorHierarchy,
            ACTOR_HIERARCHY_COPY,
        ),
        (
            "target above the invoker's rank",
            |exec: &mut MemberExecution| {
                exec.target.as_mut().expect("target").highest_role_position = 51
            },
            PolicyError::ActorHierarchy,
            ACTOR_HIERARCHY_COPY,
        ),
        (
            "target at Owen's rank",
            |exec: &mut MemberExecution| exec.bot_highest_role_position = Some(10),
            PolicyError::BotHierarchy,
            BOT_HIERARCHY_COPY,
        ),
        (
            "both hierarchies fail: the bot refusal wins",
            |exec: &mut MemberExecution| {
                exec.bot_highest_role_position = Some(10);
                exec.target.as_mut().expect("target").highest_role_position = 60;
            },
            PolicyError::BotHierarchy,
            BOT_HIERARCHY_COPY,
        ),
        (
            "protection outranks a failing hierarchy",
            |exec: &mut MemberExecution| {
                let target = exec.target.as_mut().expect("target");
                target.is_guild_owner = true;
                target.highest_role_position = 60;
                exec.bot_highest_role_position = Some(10);
            },
            PolicyError::TargetGuildOwner,
            "The guild owner is protected",
        ),
    ]
}

#[tokio::test]
async fn tempban_policy_refusals_name_the_protection_and_leave_no_trace() {
    for (label, mutate, expect, copy) in policy_refusal_cases() {
        let discord = MockMemberDiscord::new();
        let store = MemMemberStore::new();
        let svc = service_with(discord.clone(), store.clone());
        let mut exec = tempban_execution(HOUR_SECONDS);
        mutate(&mut exec);
        let err = svc
            .execute(&exec)
            .await
            .expect_err("refused tempban must not execute");
        assert_eq!(err, MemberError::Policy(expect), "{label}");
        assert_eq!(err.to_string(), copy, "{label}");
        assert_no_side_effects(&discord, &store, REQUEST_ID);

        // The refusal happened before any claim, so the same key succeeds once
        // the request is eligible again.
        let result = svc
            .execute(&tempban_execution(HOUR_SECONDS))
            .await
            .unwrap_or_else(|err| panic!("{label}: retry after fix failed: {err:?}"));
        assert_eq!(result.outcome, MemberOutcome::TemporarilyBanned, "{label}");
        assert!(!result.replayed, "{label}");
    }
}

/// Records what the unban queue looked like at the instant the ban PUT left
/// the bot, so a test can pin "staged before the PUT, activated after".
#[derive(Clone)]
struct StateProbeDiscord {
    inner: MockMemberDiscord,
    store: MemMemberStore,
    states_at_ban: Arc<Mutex<Vec<Option<String>>>>,
}

impl MemberDiscord for StateProbeDiscord {
    async fn ban(&self, guild_id: &str, user_id: &str, reason: &str) -> Result<(), DiscordError> {
        self.states_at_ban
            .lock()
            .expect("probe lock")
            .push(self.store.unban_state(REQUEST_ID));
        self.inner.ban(guild_id, user_id, reason).await
    }

    async fn unban(&self, guild_id: &str, user_id: &str, reason: &str) -> Result<(), DiscordError> {
        self.inner.unban(guild_id, user_id, reason).await
    }

    async fn kick(&self, guild_id: &str, user_id: &str, reason: &str) -> Result<(), DiscordError> {
        self.inner.kick(guild_id, user_id, reason).await
    }

    async fn timeout(
        &self,
        guild_id: &str,
        user_id: &str,
        until_iso: &str,
        reason: &str,
    ) -> Result<(), DiscordError> {
        self.inner
            .timeout(guild_id, user_id, until_iso, reason)
            .await
    }
}

#[tokio::test]
async fn tempban_success_writes_exact_audit_event_and_one_ban() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let result = svc
        .execute(&tempban_execution(HOUR_SECONDS))
        .await
        .expect("eligible tempban executes");
    assert_eq!(result.outcome, MemberOutcome::TemporarilyBanned);
    assert!(!result.replayed);

    // Exactly one Discord call, and it is the ban PUT: the unban is the
    // sweep's job, hours later.
    assert_eq!(discord.calls().len(), 1);
    assert_eq!(discord.call_count("ban"), 1);
    assert_eq!(discord.call_count("unban"), 0);

    let audits = store.audits();
    assert_eq!(audits.len(), 1);
    let row = &audits[0];
    assert_eq!(row.action, ModerationAction::TempBan.action_name());
    assert_eq!(row.action, "moderation.tempban");
    assert_eq!(row.outcome, MemberOutcome::TemporarilyBanned.as_str());
    assert_eq!(row.outcome, "temporarily_banned");
    assert_eq!(row.guild_id, GUILD_ID);
    assert_eq!(row.actor_id, ACTOR_ID);
    assert_eq!(row.target_id.as_deref(), Some(TARGET_ID));
    assert_eq!(row.request_id, REQUEST_ID);
    assert_eq!(row.idempotency_key, REQUEST_ID);
    assert_eq!(row.reason, "spam in #general");
    let metadata: serde_json::Value =
        serde_json::from_str(&row.metadata_json).expect("audit metadata is JSON");
    assert_eq!(
        metadata,
        serde_json::json!({ "duration_seconds": 3600, "ban_attempt_generation": 1 }),
        "tempban records its duration and its first prepared attempt"
    );
}

#[tokio::test]
async fn tempban_reason_propagates_trimmed_to_discord_audit_and_unban() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let mut exec = tempban_execution(HOUR_SECONDS);
    exec.reason = "  spam in #general  ".to_owned();
    let result = svc.execute(&exec).await.expect("padded reason still bans");
    assert_eq!(result.outcome, MemberOutcome::TemporarilyBanned);

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
        other => panic!("tempban must issue a ban PUT, got {other:?}"),
    }
    let audits = store.audits();
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].reason, "spam in #general");

    // The staged expiry carries the same trimmed reason into the audit log
    // entry the sweep will leave on Discord.
    let sweep = MemberModerationService::new(discord.clone(), store.clone(), policy(), || {
        NOW_MS + HOUR_SECONDS * 1000
    });
    assert_eq!(sweep.run_due_unbans(GUILD_ID).await.expect("sweep"), 1);
    assert_eq!(
        discord.calls()[1],
        DiscordCall::Unban {
            guild_id: GUILD_ID.to_owned(),
            user_id: TARGET_ID.to_owned(),
            reason: "Temporary ban expired: spam in #general".to_owned(),
        }
    );
}

#[tokio::test]
async fn tempban_stages_the_unban_before_the_ban_and_activates_it_after() {
    let store = MemMemberStore::new();
    let discord = StateProbeDiscord {
        inner: MockMemberDiscord::new(),
        store: store.clone(),
        states_at_ban: Arc::new(Mutex::new(Vec::new())),
    };
    let svc = MemberModerationService::new(discord.clone(), store.clone(), policy(), || NOW_MS);

    assert_eq!(store.unban_state(REQUEST_ID), None, "nothing staged yet");
    let result = svc
        .execute(&tempban_execution(HOUR_SECONDS))
        .await
        .expect("eligible tempban executes");
    assert_eq!(result.outcome, MemberOutcome::TemporarilyBanned);

    // One ban PUT, and at that instant the unban was staged but not yet live.
    assert_eq!(
        *discord.states_at_ban.lock().expect("probe lock"),
        vec![Some("staged".to_owned())],
        "the unban must be staged before the PUT leaves the bot"
    );
    // Once the PUT is accepted the staged row is activated for the sweep.
    assert_eq!(store.unban_state(REQUEST_ID).as_deref(), Some("pending"));
    assert_eq!(discord.inner.calls().len(), 1);
}

#[tokio::test]
async fn tempban_unban_is_due_at_exactly_now_plus_duration() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let clock = Arc::new(AtomicI64::new(NOW_MS));
    let svc = {
        let clock = Arc::clone(&clock);
        MemberModerationService::new(discord.clone(), store.clone(), policy(), move || {
            clock.load(Ordering::SeqCst)
        })
    };
    svc.execute(&tempban_execution(HOUR_SECONDS))
        .await
        .expect("eligible tempban executes");
    assert_eq!(store.unban_state(REQUEST_ID).as_deref(), Some("pending"));

    // One millisecond short of the expiry: nothing is due, nothing is sent.
    clock.store(NOW_MS + HOUR_SECONDS * 1000 - 1, Ordering::SeqCst);
    assert_eq!(svc.run_due_unbans(GUILD_ID).await.expect("early sweep"), 0);
    assert_eq!(discord.call_count("unban"), 0);
    assert_eq!(store.unban_state(REQUEST_ID).as_deref(), Some("pending"));

    // At the exact expiry the job fires once.
    clock.store(NOW_MS + HOUR_SECONDS * 1000, Ordering::SeqCst);
    assert_eq!(svc.run_due_unbans(GUILD_ID).await.expect("due sweep"), 1);
    assert_eq!(discord.call_count("unban"), 1);
    assert_eq!(store.unban_state(REQUEST_ID).as_deref(), Some("done"));
    assert_eq!(svc.run_due_unbans(GUILD_ID).await.expect("late sweep"), 0);
    assert_eq!(discord.call_count("unban"), 1);
}

#[tokio::test]
async fn tempban_same_key_retry_replays_without_second_effect() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let clock = Arc::new(AtomicI64::new(NOW_MS));
    let svc = {
        let clock = Arc::clone(&clock);
        MemberModerationService::new(discord.clone(), store.clone(), policy(), move || {
            clock.load(Ordering::SeqCst)
        })
    };
    let exec = tempban_execution(HOUR_SECONDS);
    let first = svc.execute(&exec).await.expect("eligible tempban executes");
    assert_eq!(first.outcome, MemberOutcome::TemporarilyBanned);
    assert!(!first.replayed);

    // Same request retried under the same key: the stored outcome replays,
    // Discord sees no second PUT and the ledger keeps its single row.
    let second = svc.execute(&exec).await.expect("retry replays");
    assert_eq!(second.outcome, MemberOutcome::TemporarilyBanned);
    assert!(second.replayed);

    assert_eq!(discord.call_count("ban"), 1);
    assert_eq!(discord.calls().len(), 1);
    assert_eq!(store.audits().len(), 1);

    // Still exactly one unban job: it fires once, at the original expiry.
    assert_eq!(store.unban_state(REQUEST_ID).as_deref(), Some("pending"));
    clock.store(NOW_MS + HOUR_SECONDS * 1000, Ordering::SeqCst);
    assert_eq!(svc.run_due_unbans(GUILD_ID).await.expect("sweep"), 1);
    assert_eq!(svc.run_due_unbans(GUILD_ID).await.expect("second sweep"), 0);
    assert_eq!(discord.call_count("unban"), 1);
}
