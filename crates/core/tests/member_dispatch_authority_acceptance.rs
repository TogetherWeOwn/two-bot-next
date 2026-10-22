//! Protected acceptance controls for dispatch-authority hardening.
//! Runtime REST freshness is exercised by the paired bot test module. These
//! controls catch regressions in the real service/ledger, not a second resolver.

use std::collections::HashSet;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use two_bot_core::member_moderation::{
    DiscordCall, DiscordError, MemMemberStore, MemberError, MemberExecution,
    MemberModerationService, MemberModerationStore, MockMemberDiscord,
};
use two_bot_core::{ModerationAction, ModerationActor, ModerationPolicy, ModerationTarget};

const GUILD: &str = "2222";
const TARGET: &str = "3333";
const NOW: i64 = 1_700_000_000_000;

fn policy() -> ModerationPolicy {
    ModerationPolicy {
        owen_user_id: "1".into(),
        protected_role_ids: HashSet::from(["protected".into()]),
        bot_user_id: Some("4444".into()),
    }
}

fn execution(action: ModerationAction, key: &str) -> MemberExecution {
    MemberExecution {
        action,
        guild_id: GUILD.into(),
        actor: ModerationActor {
            user_id: "88".into(),
            role_ids: vec!["moderator".into()],
            highest_role_position: 50,
            permissions: u64::MAX,
        },
        target: Some(ModerationTarget {
            user_id: TARGET.into(),
            role_ids: vec![],
            highest_role_position: 0,
            is_bot: false,
            is_guild_owner: false,
        }),
        bot_highest_role_position: 100,
        reason: "acceptance fixture".into(),
        duration_seconds: Some(60),
        request_id: key.into(),
        idempotency_key: key.into(),
    }
}

async fn hold_member(store: MemMemberStore) -> (Arc<Notify>, tokio::task::JoinHandle<()>) {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (inside, gate) = (entered.clone(), release.clone());
    let task = tokio::spawn(async move {
        store
            .serialize_member(GUILD, TARGET, || async {
                inside.notify_one();
                gate.notified().await;
            })
            .await;
    });
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("fixture owns the real member lane");
    (release, task)
}

#[tokio::test]
async fn ban_and_tempban_still_wait_on_the_member_lane_without_blocking_another_member() {
    for action in [ModerationAction::Ban, ModerationAction::TempBan] {
        let store = MemMemberStore::new();
        let discord = MockMemberDiscord::new();
        let service =
            MemberModerationService::new(discord.clone(), store.clone(), policy(), || NOW);
        let (release, holder) = hold_member(store.clone()).await;
        let queued = execution(action, "queued");
        let mut waiting = Box::pin(service.execute(&queued));
        let pending = tokio::time::timeout(Duration::from_millis(30), &mut waiting)
            .await
            .is_err();
        let before = discord.calls();
        let mut independent = execution(ModerationAction::Ban, "independent");
        independent.target.as_mut().expect("target").user_id = "7777".into();
        let other =
            tokio::time::timeout(Duration::from_secs(5), service.execute(&independent)).await;
        release.notify_one();
        holder.await.expect("lane holder");
        let result = waiting.await;
        assert!(pending, "serialized verbs must not bypass an occupied lane");
        assert!(before.is_empty(), "no mutation while waiting");
        assert!(other.expect("other member stays independent").is_ok());
        assert!(result.is_ok());
        assert_eq!(discord.call_count("ban"), 2);
        assert_eq!(store.audits().len(), 2);
        assert!(store.ban_attempt("queued").is_some());
        assert_eq!(
            store.unban_state("queued").as_deref(),
            (action == ModerationAction::TempBan).then_some("pending")
        );
    }
}

#[tokio::test]
async fn completed_replay_never_dispatches_or_takes_expiry_ownership_again() {
    let store = MemMemberStore::new();
    let discord = MockMemberDiscord::new();
    let service = MemberModerationService::new(discord.clone(), store.clone(), policy(), || NOW);
    let request = execution(ModerationAction::TempBan, "completed");
    let first = service
        .execute(&request)
        .await
        .expect("accepted temporary ban");
    let attempt = store.ban_attempt("completed");
    discord.fail_with("ban", DiscordError::Timeout);
    let replay = service.execute(&request).await.expect("completed replay");
    assert!(!first.replayed);
    assert!(replay.replayed);
    assert_eq!(replay.outcome, first.outcome);
    assert_eq!(discord.call_count("ban"), 1);
    assert_eq!(store.ban_attempt("completed"), attempt);
    assert_eq!(store.unban_state("completed").as_deref(), Some("pending"));
    assert_eq!(store.audits().len(), 1);
}

#[tokio::test]
async fn denied_new_request_preserves_the_older_accepted_expiry_and_its_dispatch() {
    let clock = AtomicI64::new(NOW);
    let store = MemMemberStore::new();
    let discord = MockMemberDiscord::new();
    let service = MemberModerationService::new(discord.clone(), store.clone(), policy(), || {
        clock.load(Ordering::SeqCst)
    });
    service
        .execute(&execution(ModerationAction::TempBan, "accepted"))
        .await
        .expect("old accepted expiry");
    let original = store.ban_attempt("accepted");
    let mut refused = execution(ModerationAction::TempBan, "revoked");
    refused.actor.permissions = 0;
    assert!(matches!(
        service.execute(&refused).await,
        Err(MemberError::Policy(_))
    ));
    assert_eq!(discord.call_count("ban"), 1);
    assert_eq!(store.ban_attempt("accepted"), original);
    assert!(store.ban_attempt("revoked").is_none());
    assert!(store.unban_state("revoked").is_none());
    assert_eq!(store.unban_state("accepted").as_deref(), Some("pending"));
    clock.store(NOW + 60_001, Ordering::SeqCst);
    assert_eq!(
        service
            .run_due_unbans(GUILD)
            .await
            .expect("old expiry survives"),
        1
    );
    assert_eq!(discord.call_count("unban"), 1);
    assert_eq!(store.unban_state("accepted").as_deref(), Some("done"));
    assert!(store
        .audits()
        .iter()
        .any(|row| row.request_id == "revoked:denied" && row.outcome == "denied"));
}

#[tokio::test]
async fn new_denial_cannot_release_an_unknown_dispatched_put_or_its_staged_expiry() {
    let store = MemMemberStore::new();
    let discord = MockMemberDiscord::new();
    let service = MemberModerationService::new(discord.clone(), store.clone(), policy(), || NOW);
    discord.fail_with("ban", DiscordError::Timeout);
    let unknown = execution(ModerationAction::TempBan, "unknown");
    assert!(matches!(
        service.execute(&unknown).await,
        Err(MemberError::Discord(DiscordError::Timeout))
    ));
    let original = store.ban_attempt("unknown").expect("prepared PUT evidence");
    let before = store.surface_uncertain(GUILD).await.expect("uncertainty");
    let mut revoked = execution(ModerationAction::Ban, "new-denial");
    revoked.actor.permissions = 0;
    assert!(matches!(
        service.execute(&revoked).await,
        Err(MemberError::Policy(_))
    ));
    discord.clear_failure("ban");
    assert!(matches!(
        service.execute(&unknown).await,
        Err(MemberError::InFlight)
    ));
    assert!(service
        .execute(&execution(ModerationAction::Ban, "new-key"))
        .await
        .is_err());
    assert_eq!(discord.call_count("ban"), 1);
    assert_eq!(store.ban_attempt("unknown"), Some(original));
    assert_eq!(store.unban_state("unknown").as_deref(), Some("staged"));
    assert_eq!(
        store.surface_uncertain(GUILD).await.expect("still fenced"),
        before
    );
    assert_eq!(
        service.run_due_unbans(GUILD).await.expect("fenced sweep"),
        0
    );
}

#[tokio::test]
async fn current_protected_role_denials_are_audited_for_every_member_verb_without_effects() {
    for action in [
        ModerationAction::Ban,
        ModerationAction::TempBan,
        ModerationAction::Kick,
        ModerationAction::Timeout,
        ModerationAction::Warn,
    ] {
        let store = MemMemberStore::new();
        let discord = MockMemberDiscord::new();
        let service =
            MemberModerationService::new(discord.clone(), store.clone(), policy(), || NOW);
        let mut request = execution(action, "protected");
        request
            .target
            .as_mut()
            .expect("target")
            .role_ids
            .push("protected".into());
        assert!(matches!(
            service.execute(&request).await,
            Err(MemberError::Policy(_))
        ));
        assert!(discord.calls().is_empty());
        assert!(store.warnings().is_empty());
        assert!(store.ban_attempt("protected").is_none());
        assert!(store.unban_state("protected").is_none());
        let audits = store.audits();
        assert_eq!(audits.len(), 1);
        assert_eq!(audits[0].actor_id, "88");
        assert_eq!(audits[0].action, action.action_name());
        assert_eq!(audits[0].outcome, "denied");
    }
}

#[tokio::test]
async fn an_in_flight_delete_is_not_cleared_by_a_new_policy_denial() {
    let clock = AtomicI64::new(NOW);
    let store = MemMemberStore::new();
    let discord = MockMemberDiscord::new();
    let service = MemberModerationService::new(discord.clone(), store.clone(), policy(), || {
        clock.load(Ordering::SeqCst)
    });
    service
        .execute(&execution(ModerationAction::TempBan, "expiry"))
        .await
        .expect("accepted");
    clock.store(NOW + 60_001, Ordering::SeqCst);
    discord.fail_with("unban", DiscordError::Timeout);
    assert_eq!(
        service.run_due_unbans(GUILD).await.expect("timeout fenced"),
        0
    );
    let before = store
        .surface_uncertain(GUILD)
        .await
        .expect("running DELETE");
    let mut denied = execution(ModerationAction::Ban, "denied-after-delete");
    denied.actor.permissions = 0;
    assert!(service.execute(&denied).await.is_err());
    assert_eq!(
        store.surface_uncertain(GUILD).await.expect("same evidence"),
        before
    );
    assert_eq!(store.unban_state("expiry").as_deref(), Some("running"));
    assert!(service
        .execute(&execution(ModerationAction::Ban, "fresh"))
        .await
        .is_err());
    assert_eq!(
        discord
            .calls()
            .iter()
            .filter(|call| matches!(call, DiscordCall::Ban { .. }))
            .count(),
        1
    );
    assert_eq!(discord.call_count("unban"), 1);
}
