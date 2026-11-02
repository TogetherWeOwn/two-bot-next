//! Protected acceptance: audit every member-moderation terminal outcome.
//!
//! Contract for the paired build card: reason/duration refusals, claim
//! mismatches and Discord failures each leave their audit row. Today only
//! success (plus policy/authority refusals) writes a row, so the `*_writes_*`
//! tests below are RED on the pre-fix head (missing row) and GREEN after the
//! fix. The replay and byte-identical success controls are GREEN before and
//! after. The builder must satisfy this file without editing it.
//!
//! Synthetic fixtures only: literal permission bits, synthetic snowflake ids,
//! in-memory Discord double and store. No Discord, no network, no database.

use std::collections::HashSet;

use two_bot_core::commands::{PERM_BAN_MEMBERS, PERM_KICK_MEMBERS, PERM_MODERATE_MEMBERS};
use two_bot_core::member_moderation::{
    AuditRow, DiscordError, MemMemberStore, MemberError, MemberExecution, MemberModerationService,
    MemberOutcome, MockMemberDiscord,
};
use two_bot_core::moderation::{
    ModerationAction, ModerationActor, ModerationPolicy, ModerationTarget, ReasonError,
};

const GUILD_ID: &str = "100000000000000001";
const OWEN_ID: &str = "123456789012345678";
const BOT_ID: &str = "555555555555555555";
const ACTOR_ID: &str = "111111111111111111";
const TARGET_ID: &str = "333333333333333333";
const STAFF_ROLE_ID: &str = "444444444444444444";

const REASON: &str = "spam in #general";

fn policy() -> ModerationPolicy {
    ModerationPolicy {
        owen_user_id: OWEN_ID.to_owned(),
        protected_role_ids: HashSet::from([STAFF_ROLE_ID.to_owned()]),
        bot_user_id: Some(BOT_ID.to_owned()),
    }
}

fn actor_with(permissions: u64) -> ModerationActor {
    ModerationActor {
        user_id: ACTOR_ID.to_owned(),
        role_ids: vec![],
        highest_role_position: 50,
        permissions,
    }
}

fn plain_target() -> ModerationTarget {
    ModerationTarget {
        user_id: TARGET_ID.to_owned(),
        role_ids: vec![],
        highest_role_position: 10,
        is_bot: false,
        is_guild_owner: false,
    }
}

fn execution(
    action: ModerationAction,
    permissions: u64,
    reason: &str,
    duration_seconds: Option<i64>,
    request_id: &str,
) -> MemberExecution {
    MemberExecution {
        action,
        guild_id: GUILD_ID.to_owned(),
        actor: actor_with(permissions),
        target: Some(plain_target()),
        bot_highest_role_position: 100,
        reason: reason.to_owned(),
        duration_seconds,
        request_id: request_id.to_owned(),
        idempotency_key: request_id.to_owned(),
    }
}

fn service_with(
    discord: MockMemberDiscord,
    store: MemMemberStore,
) -> MemberModerationService<MockMemberDiscord, MemMemberStore, fn() -> i64> {
    MemberModerationService::new(discord, store, policy(), || 1_700_000_000_000)
}

/// The one denied row a validation refusal must leave: bound to the request
/// and key, staged as `validation`, and suffixed so a later accepted retry
/// keeps its own plain request id.
fn assert_validation_denial(store: &MemMemberStore, action: ModerationAction, request_id: &str) {
    let audits = store.audits();
    assert_eq!(audits.len(), 1, "one denied row for {action:?}");
    let row = &audits[0];
    assert_eq!(row.action, action.action_name());
    assert_eq!(row.outcome, "denied");
    assert_eq!(row.request_id, format!("{request_id}:denied"));
    assert_eq!(row.guild_id, GUILD_ID);
    assert_eq!(row.actor_id, ACTOR_ID);
    assert_eq!(row.target_id.as_deref(), Some(TARGET_ID));
    assert_eq!(row.idempotency_key, request_id);
    assert!(!row.reason.is_empty(), "denial carries a reason");
    let metadata: serde_json::Value =
        serde_json::from_str(&row.metadata_json).expect("audit metadata is JSON");
    assert_eq!(
        metadata,
        serde_json::json!({ "stage": "validation", "request_id": request_id })
    );
}

#[tokio::test]
async fn ban_empty_reason_writes_validation_denied_row() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let exec = execution(
        ModerationAction::Ban,
        PERM_BAN_MEMBERS,
        "   ",
        None,
        "req-ban",
    );
    let err = svc
        .execute(&exec)
        .await
        .expect_err("blank reason must refuse");
    assert_eq!(err, MemberError::Reason(ReasonError::Empty));
    assert!(discord.calls().is_empty(), "no Discord before validation");
    assert_validation_denial(&store, ModerationAction::Ban, "req-ban");
}

#[tokio::test]
async fn tempban_empty_reason_writes_validation_denied_row() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let exec = execution(
        ModerationAction::TempBan,
        PERM_BAN_MEMBERS,
        "",
        None,
        "req-tempban",
    );
    let err = svc
        .execute(&exec)
        .await
        .expect_err("empty reason must refuse");
    assert_eq!(err, MemberError::Reason(ReasonError::Empty));
    assert!(discord.calls().is_empty(), "no Discord before validation");
    assert_validation_denial(&store, ModerationAction::TempBan, "req-tempban");
}

#[tokio::test]
async fn kick_empty_reason_writes_validation_denied_row() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let exec = execution(
        ModerationAction::Kick,
        PERM_KICK_MEMBERS,
        "",
        None,
        "req-kick",
    );
    let err = svc
        .execute(&exec)
        .await
        .expect_err("empty reason must refuse");
    assert_eq!(err, MemberError::Reason(ReasonError::Empty));
    assert!(discord.calls().is_empty(), "no Discord before validation");
    assert_validation_denial(&store, ModerationAction::Kick, "req-kick");
}

#[tokio::test]
async fn timeout_empty_reason_writes_validation_denied_row() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let exec = execution(
        ModerationAction::Timeout,
        PERM_MODERATE_MEMBERS,
        "",
        None,
        "req-timeout",
    );
    let err = svc
        .execute(&exec)
        .await
        .expect_err("empty reason must refuse");
    assert_eq!(err, MemberError::Reason(ReasonError::Empty));
    assert!(discord.calls().is_empty(), "no Discord before validation");
    assert_validation_denial(&store, ModerationAction::Timeout, "req-timeout");
}

#[tokio::test]
async fn warn_empty_reason_writes_validation_denied_row() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let exec = execution(
        ModerationAction::Warn,
        PERM_MODERATE_MEMBERS,
        "",
        None,
        "req-warn",
    );
    let err = svc
        .execute(&exec)
        .await
        .expect_err("empty reason must refuse");
    assert_eq!(err, MemberError::Reason(ReasonError::Empty));
    assert!(discord.calls().is_empty(), "no Discord before validation");
    assert_validation_denial(&store, ModerationAction::Warn, "req-warn");
}

#[tokio::test]
async fn tempban_out_of_range_duration_writes_validation_denied_row() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let exec = execution(
        ModerationAction::TempBan,
        PERM_BAN_MEMBERS,
        REASON,
        Some(0),
        "req-tempban-duration",
    );
    let err = svc
        .execute(&exec)
        .await
        .expect_err("zero duration must refuse");
    match &err {
        MemberError::Malformed { field, message } => {
            assert_eq!(*field, "duration_seconds");
            assert!(
                message.contains("between 60 and"),
                "bounds named, got {message}"
            );
        }
        other => panic!("expected Malformed duration, got {other:?}"),
    }
    assert!(discord.calls().is_empty(), "no Discord before validation");
    assert_validation_denial(&store, ModerationAction::TempBan, "req-tempban-duration");
}

#[tokio::test]
async fn timeout_out_of_range_duration_writes_validation_denied_row() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let exec = execution(
        ModerationAction::Timeout,
        PERM_MODERATE_MEMBERS,
        REASON,
        Some(28 * 24 * 60 * 60 + 1),
        "req-timeout-duration",
    );
    let err = svc
        .execute(&exec)
        .await
        .expect_err("over-long duration must refuse");
    match &err {
        MemberError::Malformed { field, message } => {
            assert_eq!(*field, "duration_seconds");
            assert!(
                message.contains("between 60 and"),
                "bounds named, got {message}"
            );
        }
        other => panic!("expected Malformed duration, got {other:?}"),
    }
    assert!(discord.calls().is_empty(), "no Discord before validation");
    assert_validation_denial(&store, ModerationAction::Timeout, "req-timeout-duration");
}

#[tokio::test]
async fn claim_mismatch_writes_claim_denied_row() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let first = execution(
        ModerationAction::Kick,
        PERM_KICK_MEMBERS,
        REASON,
        None,
        "req-mismatch",
    );
    svc.execute(&first).await.expect("eligible kick executes");

    // Same idempotency key, different content: a caller bug, answered as a
    // mismatch and audited as denied without touching Discord again.
    let mut second = execution(
        ModerationAction::Kick,
        PERM_KICK_MEMBERS,
        "harassment in #general",
        None,
        "req-mismatch-retry",
    );
    second.idempotency_key = "req-mismatch".to_owned();
    let err = svc
        .execute(&second)
        .await
        .expect_err("reused key with new content must mismatch");
    assert_eq!(err, MemberError::KeyMismatch);
    assert_eq!(discord.call_count("kick"), 1);

    let audits = store.audits();
    assert_eq!(audits.len(), 2, "success row plus mismatch denial");
    let row = &audits[1];
    assert_eq!(row.action, ModerationAction::Kick.action_name());
    assert_eq!(row.outcome, "denied");
    assert_eq!(row.request_id, "req-mismatch-retry:denied");
    assert_eq!(row.guild_id, GUILD_ID);
    assert_eq!(row.actor_id, ACTOR_ID);
    assert_eq!(row.target_id.as_deref(), Some(TARGET_ID));
    assert_eq!(row.idempotency_key, "req-mismatch");
    assert!(!row.reason.is_empty(), "denial carries a reason");
    let metadata: serde_json::Value =
        serde_json::from_str(&row.metadata_json).expect("audit metadata is JSON");
    assert_eq!(
        metadata,
        serde_json::json!({ "stage": "claim", "request_id": "req-mismatch-retry" })
    );
}

#[tokio::test]
async fn discord_rejection_writes_failed_row_and_releases_for_retry() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    discord.fail_with("kick", DiscordError::Rejected("403 Forbidden".to_owned()));
    let exec = execution(
        ModerationAction::Kick,
        PERM_KICK_MEMBERS,
        REASON,
        None,
        "req-kfail",
    );
    let err = svc.execute(&exec).await.expect_err("rejected kick fails");
    assert_eq!(
        err,
        MemberError::Discord(DiscordError::Rejected("403 Forbidden".to_owned()))
    );

    let audits = store.audits();
    assert_eq!(audits.len(), 1, "one failed row for the rejection");
    let row = &audits[0];
    assert_eq!(row.action, ModerationAction::Kick.action_name());
    assert_eq!(row.outcome, "failed");
    assert_eq!(row.request_id, "req-kfail:failed");
    assert_eq!(row.guild_id, GUILD_ID);
    assert_eq!(row.actor_id, ACTOR_ID);
    assert_eq!(row.target_id.as_deref(), Some(TARGET_ID));
    assert_eq!(row.idempotency_key, "req-kfail");
    assert!(!row.reason.is_empty(), "failure carries a reason");
    let metadata: serde_json::Value =
        serde_json::from_str(&row.metadata_json).expect("audit metadata is JSON");
    assert!(metadata.is_object(), "metadata stays an object");

    // A proven no-effect failure releases the key: a real retry executes and
    // its plain success row survives next to the suffixed failed row.
    discord.clear_failure("kick");
    let retry = svc.execute(&exec).await.expect("released key retries");
    assert_eq!(retry.outcome, MemberOutcome::Kicked);
    assert!(!retry.replayed);
    assert_eq!(discord.call_count("kick"), 2);
    let audits = store.audits();
    assert_eq!(audits.len(), 2, "failed row plus success row");
    assert_eq!(audits[1].request_id, "req-kfail");
    assert_eq!(audits[1].outcome, MemberOutcome::Kicked.as_str());
}

#[tokio::test]
async fn discord_timeout_writes_in_progress_row_and_holds_claim() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    discord.fail_with("kick", DiscordError::Timeout);
    let exec = execution(
        ModerationAction::Kick,
        PERM_KICK_MEMBERS,
        REASON,
        None,
        "req-ktimeout",
    );
    let err = svc.execute(&exec).await.expect_err("timed-out kick fails");
    assert_eq!(err, MemberError::Discord(DiscordError::Timeout));

    let audits = store.audits();
    assert_eq!(audits.len(), 1, "one in-progress row for the timeout");
    let row = &audits[0];
    assert_eq!(row.action, ModerationAction::Kick.action_name());
    assert_eq!(row.outcome, "in_progress");
    assert_eq!(row.request_id, "req-ktimeout:in_progress");
    assert_eq!(row.guild_id, GUILD_ID);
    assert_eq!(row.actor_id, ACTOR_ID);
    assert_eq!(row.target_id.as_deref(), Some(TARGET_ID));
    assert_eq!(row.idempotency_key, "req-ktimeout");
    assert!(!row.reason.is_empty(), "failure carries a reason");

    // Uncertain means fenced: even after Discord recovers, the same key
    // answers InFlight and writes nothing more.
    discord.clear_failure("kick");
    let err = svc
        .execute(&exec)
        .await
        .expect_err("uncertain claim stays fenced");
    assert_eq!(err, MemberError::InFlight);
    assert_eq!(discord.call_count("kick"), 1);
    assert_eq!(store.audits().len(), 1);
}

// --- controls: GREEN before and after --------------------------------------

#[tokio::test]
async fn same_key_retry_replays_without_new_row() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let exec = execution(
        ModerationAction::Kick,
        PERM_KICK_MEMBERS,
        REASON,
        None,
        "req-replay",
    );
    let first = svc.execute(&exec).await.expect("eligible kick executes");
    assert_eq!(first.outcome, MemberOutcome::Kicked);
    assert!(!first.replayed);

    let second = svc.execute(&exec).await.expect("retry replays");
    assert_eq!(second.outcome, MemberOutcome::Kicked);
    assert!(second.replayed);

    assert_eq!(discord.call_count("kick"), 1);
    assert_eq!(store.audits().len(), 1, "replay writes no new row");
}

#[tokio::test]
async fn kick_success_row_is_byte_identical() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let result = svc
        .execute(&execution(
            ModerationAction::Kick,
            PERM_KICK_MEMBERS,
            REASON,
            None,
            "req-kick",
        ))
        .await
        .expect("eligible kick executes");
    assert_eq!(result.outcome, MemberOutcome::Kicked);
    assert!(!result.replayed);

    let audits = store.audits();
    assert_eq!(audits.len(), 1);
    assert_eq!(
        audits[0],
        AuditRow {
            request_id: "req-kick".to_owned(),
            guild_id: GUILD_ID.to_owned(),
            actor_id: ACTOR_ID.to_owned(),
            action: ModerationAction::Kick.action_name(),
            target_id: Some(TARGET_ID.to_owned()),
            reason: REASON.to_owned(),
            outcome: MemberOutcome::Kicked.as_str(),
            idempotency_key: "req-kick".to_owned(),
            metadata_json: r#"{"duration_seconds":null}"#.to_owned(),
        }
    );
}

#[tokio::test]
async fn timeout_success_row_is_byte_identical() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let result = svc
        .execute(&execution(
            ModerationAction::Timeout,
            PERM_MODERATE_MEMBERS,
            REASON,
            Some(3600),
            "req-timeout-ok",
        ))
        .await
        .expect("eligible timeout executes");
    assert_eq!(result.outcome, MemberOutcome::TimedOut);
    assert!(!result.replayed);

    let audits = store.audits();
    assert_eq!(audits.len(), 1);
    assert_eq!(
        audits[0],
        AuditRow {
            request_id: "req-timeout-ok".to_owned(),
            guild_id: GUILD_ID.to_owned(),
            actor_id: ACTOR_ID.to_owned(),
            action: ModerationAction::Timeout.action_name(),
            target_id: Some(TARGET_ID.to_owned()),
            reason: REASON.to_owned(),
            outcome: MemberOutcome::TimedOut.as_str(),
            idempotency_key: "req-timeout-ok".to_owned(),
            metadata_json: r#"{"duration_seconds":3600}"#.to_owned(),
        }
    );
}
