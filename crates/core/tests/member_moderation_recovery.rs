use std::collections::HashSet;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use two_bot_core::member_moderation::{
    DiscordCall, DiscordError, MemMemberStore, MemberDiscord, MemberExecution,
    MemberModerationService, MemberModerationStore, MockMemberDiscord,
};
use two_bot_core::{ModerationAction, ModerationActor, ModerationPolicy, ModerationTarget};

const GUILD: &str = "100000000000000001";
const NOW: &str = "2023-11-14T22:13:20.000Z";
const DUE: &str = "2023-11-14T23:13:20.000Z";

fn policy() -> ModerationPolicy {
    ModerationPolicy {
        owen_user_id: "123456789012345678".into(),
        bot_user_id: Some("555555555555555555".into()),
        protected_role_ids: HashSet::new(),
    }
}

fn execution(action: ModerationAction, id: &str) -> MemberExecution {
    MemberExecution {
        action,
        guild_id: GUILD.into(),
        actor: ModerationActor {
            user_id: "111111111111111111".into(),
            role_ids: vec![],
            highest_role_position: 50,
            permissions: u64::MAX,
        },
        target: Some(ModerationTarget {
            user_id: "333333333333333333".into(),
            role_ids: vec![],
            highest_role_position: 10,
            is_bot: false,
            is_guild_owner: false,
        }),
        bot_highest_role_position: Some(100),
        reason: "spam".into(),
        duration_seconds: Some(3600),
        request_id: id.into(),
        idempotency_key: id.into(),
    }
}

#[tokio::test]
async fn permanent_ban_supersedes_expiry_but_rejected_ban_preserves_it() {
    for rejected in [false, true] {
        let clock = AtomicI64::new(1_700_000_000_000);
        let discord = MockMemberDiscord::new();
        let store = MemMemberStore::new();
        let svc = MemberModerationService::new(discord.clone(), store.clone(), policy(), || {
            clock.load(Ordering::SeqCst)
        });
        svc.execute(&execution(ModerationAction::TempBan, "temporary"))
            .await
            .expect("tempban");
        if rejected {
            discord.fail_with("ban", DiscordError::Rejected("no permission".into()));
        }
        let result = svc
            .execute(&execution(ModerationAction::Ban, "permanent"))
            .await;
        assert_eq!(result.is_err(), rejected);
        clock.store(1_700_003_600_000, Ordering::SeqCst);
        assert_eq!(
            svc.run_due_unbans(GUILD).await.expect("sweep"),
            usize::from(rejected)
        );
        assert_eq!(discord.call_count("unban"), usize::from(rejected));
    }
}

#[tokio::test]
async fn generated_expiry_reason_is_bounded_and_unicode_safe() {
    // Reasons are bounded by 512 UTF-16 units (JS-length parity): 512 BMP
    // chars, or 256 astral chars at 2 units each, are the valid maxima.
    for (text, repeats) in [("x", 512), ("🦀", 256)] {
        let discord = MockMemberDiscord::new();
        let store = MemMemberStore::new();
        let svc =
            MemberModerationService::new(discord, store.clone(), policy(), || 1_700_000_000_000);
        let mut req = execution(ModerationAction::TempBan, "long-reason");
        req.reason = text.repeat(repeats);
        svc.execute(&req).await.expect("valid max reason");
        let jobs = store.claim_due_unbans(GUILD, DUE, 1).await.expect("expiry");
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].reason.encode_utf16().count() <= 512);
        assert!(jobs[0].reason.starts_with("Temporary ban expired: "));
        assert!(jobs[0].reason.ends_with(text));
    }
    // The BMP maximum fills the 512-char expiry budget exactly.
    {
        let discord = MockMemberDiscord::new();
        let store = MemMemberStore::new();
        let svc =
            MemberModerationService::new(discord, store.clone(), policy(), || 1_700_000_000_000);
        let mut req = execution(ModerationAction::TempBan, "long-reason");
        req.reason = "x".repeat(512);
        svc.execute(&req).await.expect("valid max reason");
        let jobs = store.claim_due_unbans(GUILD, DUE, 1).await.expect("expiry");
        assert_eq!(jobs[0].reason.chars().count(), 512);
    }
}

#[tokio::test]
async fn recovery_uses_execution_order_not_timestamp_or_request_id() {
    let store = MemMemberStore::new();
    for (user, second_time) in [("tied", NOW), ("backwards", "2023-11-14T22:13:19.000Z")] {
        let old = format!("z-old-{user}");
        let new = format!("a-new-{user}");
        store
            .stage_unban(GUILD, user, NOW, "old", &old, NOW)
            .await
            .expect("old");
        store
            .confirm_ban_attempt(
                GUILD,
                user,
                &old,
                store.ban_attempt(&old).expect("fixture attempt"),
                NOW,
            )
            .await
            .expect("old accepted");
        store
            .stage_unban(GUILD, user, DUE, "new", &new, second_time)
            .await
            .expect("new");
        store
            .confirm_ban_attempt(
                GUILD,
                user,
                &new,
                store.ban_attempt(&new).expect("fixture attempt"),
                second_time,
            )
            .await
            .expect("new accepted");
    }
    assert!(store
        .claim_due_unbans(GUILD, NOW, 25)
        .await
        .expect("not due")
        .is_empty());
    let jobs = store.claim_due_unbans(GUILD, DUE, 25).await.expect("later");
    assert_eq!(jobs.len(), 2);
    assert!(jobs.iter().all(|job| job.request_id.starts_with("a-new")));
}

#[tokio::test]
async fn uncertain_ban_fences_old_expiry_until_explicit_reconciliation() {
    for action in [ModerationAction::Ban, ModerationAction::TempBan] {
        let clock = AtomicI64::new(1_700_000_000_000);
        let discord = MockMemberDiscord::new();
        let store = MemMemberStore::new();
        let svc = MemberModerationService::new(discord.clone(), store.clone(), policy(), || {
            clock.load(Ordering::SeqCst)
        });
        svc.execute(&execution(ModerationAction::TempBan, "old"))
            .await
            .expect("old ban");
        discord.fail_with("ban", DiscordError::Timeout);
        assert!(svc.execute(&execution(action, "uncertain")).await.is_err());
        clock.store(1_700_003_600_000, Ordering::SeqCst);
        assert_eq!(svc.run_due_unbans(GUILD).await.expect("fenced"), 0);
        assert_eq!(discord.call_count("unban"), 0);
        let req = execution(action, "uncertain");
        assert!(svc.execute(&req).await.is_err());
        assert_eq!(discord.call_count("ban"), 2);
        // Simulate authoritative proof of refusal, not an age-based guess.
        store
            .reject_ban_attempt(
                GUILD,
                &req.target.expect("target").user_id,
                "uncertain",
                store.ban_attempt("uncertain").expect("fixture attempt"),
                DUE,
            )
            .await
            .expect("reconciliation");
        assert_eq!(
            svc.run_due_unbans(GUILD)
                .await
                .expect("old expiry restored"),
            1
        );
    }
}

#[tokio::test]
async fn uncertain_old_put_refuses_new_bans_and_preserves_retry_until_reconciled() {
    for old_action in [ModerationAction::Ban, ModerationAction::TempBan] {
        for new_action in [ModerationAction::Ban, ModerationAction::TempBan] {
            let clock = AtomicI64::new(1_700_000_000_000);
            let discord = MockMemberDiscord::new();
            let store = MemMemberStore::new();
            let svc =
                MemberModerationService::new(discord.clone(), store.clone(), policy(), || {
                    clock.load(Ordering::SeqCst)
                });
            discord.fail_with("ban", DiscordError::Timeout);
            assert!(svc
                .execute(&execution(old_action, "old-put"))
                .await
                .is_err());
            discord.clear_failure("ban");
            let new = execution(new_action, "new-put");
            assert!(svc.execute(&new).await.is_err(), "old PUT can still land");
            assert_eq!(discord.call_count("ban"), 1, "no newer PUT was dispatched");
            clock.store(1_700_003_600_000, Ordering::SeqCst);
            assert_eq!(svc.run_due_unbans(GUILD).await.expect("fenced sweep"), 0);
            assert_eq!(discord.call_count("unban"), 0);
            // Exact-operation proof that the old PUT cannot land, not age,
            // local cancellation, or the current banned status.
            store
                .reject_ban_attempt(
                    GUILD,
                    &new.target.as_ref().expect("target").user_id,
                    "old-put",
                    store.ban_attempt("old-put").expect("fixture attempt"),
                    DUE,
                )
                .await
                .expect("authoritative refusal");
            svc.execute(&new)
                .await
                .expect("same never-dispatched key retries");
            assert_eq!(discord.call_count("ban"), 2);
            clock.store(1_700_007_200_000, Ordering::SeqCst);
            assert_eq!(
                svc.run_due_unbans(GUILD).await.expect("safe expiry"),
                usize::from(new_action == ModerationAction::TempBan)
            );
        }
    }
}

#[tokio::test]
async fn prepared_staging_is_never_activated_without_acceptance() {
    let store = MemMemberStore::new();
    store
        .stage_unban(GUILD, "user", NOW, "expiry", "request", NOW)
        .await
        .expect("stage");
    assert!(store
        .activate_staged_unban(GUILD, "user", "request", NOW)
        .await
        .is_err());
    assert!(store
        .claim_due_unbans(GUILD, DUE, 25)
        .await
        .expect("no guessed recovery")
        .is_empty());
    store
        .confirm_ban_attempt(
            GUILD,
            "user",
            "request",
            store.ban_attempt("request").expect("fixture attempt"),
            DUE,
        )
        .await
        .expect("observed acceptance");
    assert_eq!(
        store
            .claim_due_unbans("other-guild", DUE, 25)
            .await
            .expect("foreign sweep")
            .len(),
        0
    );
    assert_eq!(
        store
            .claim_due_unbans(GUILD, DUE, 25)
            .await
            .expect("confirmed recovery")
            .len(),
        1
    );
}

#[tokio::test]
async fn safe_sweep_failure_does_not_retry_or_reserve_the_next_job() {
    let store = MemMemberStore::new();
    for user in ["1", "2"] {
        store
            .stage_unban(GUILD, user, NOW, "expiry", user, NOW)
            .await
            .expect("stage");
        store
            .confirm_ban_attempt(
                GUILD,
                user,
                user,
                store.ban_attempt(user).expect("fixture attempt"),
                NOW,
            )
            .await
            .expect("confirm");
    }
    let discord = MockMemberDiscord::new();
    discord.fail_with("unban", DiscordError::Rejected("permission".into()));
    let svc = MemberModerationService::new(discord.clone(), store.clone(), policy(), || {
        1_700_000_000_000
    });
    assert!(svc.run_due_unbans(GUILD).await.is_err());
    assert_eq!(discord.call_count("unban"), 1);
    assert_eq!(store.unban_state("1").as_deref(), Some("pending"));
    assert_eq!(store.unban_state("2").as_deref(), Some("pending"));
}

#[derive(Clone)]
struct HeldUnban {
    calls: MockMemberDiscord,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

impl MemberDiscord for HeldUnban {
    async fn ban(&self, guild: &str, user: &str, reason: &str) -> Result<(), DiscordError> {
        self.calls.ban(guild, user, reason).await
    }
    async fn kick(&self, guild: &str, user: &str, reason: &str) -> Result<(), DiscordError> {
        self.calls.kick(guild, user, reason).await
    }
    async fn timeout(
        &self,
        guild: &str,
        user: &str,
        until: &str,
        reason: &str,
    ) -> Result<(), DiscordError> {
        self.calls.timeout(guild, user, until, reason).await
    }
    async fn unban(&self, guild: &str, user: &str, reason: &str) -> Result<(), DiscordError> {
        self.calls.unban(guild, user, reason).await?;
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }
}

#[tokio::test]
async fn permanent_ban_waits_for_the_same_members_live_unban() {
    let store = MemMemberStore::new();
    let target = execution(ModerationAction::Ban, "new")
        .target
        .expect("target")
        .user_id;
    store
        .stage_unban(GUILD, &target, NOW, "expiry", "old", NOW)
        .await
        .expect("stage");
    store
        .confirm_ban_attempt(
            GUILD,
            &target,
            "old",
            store.ban_attempt("old").expect("fixture attempt"),
            NOW,
        )
        .await
        .expect("accepted");
    let discord = HeldUnban {
        calls: MockMemberDiscord::new(),
        entered: Arc::new(Notify::new()),
        release: Arc::new(Notify::new()),
    };
    let svc = Arc::new(MemberModerationService::new(
        discord.clone(),
        store.clone(),
        policy(),
        || 1_700_000_000_000,
    ));
    let sweep_svc = svc.clone();
    let sweep = tokio::spawn(async move { sweep_svc.run_due_unbans(GUILD).await });
    tokio::time::timeout(Duration::from_secs(5), discord.entered.notified())
        .await
        .expect("unban entered");
    let ban_svc = svc.clone();
    let mut ban = tokio::spawn(async move {
        ban_svc
            .execute(&execution(ModerationAction::Ban, "new"))
            .await
    });
    assert!(tokio::time::timeout(Duration::from_millis(20), &mut ban)
        .await
        .is_err());
    assert_eq!(discord.calls.call_count("ban"), 0);
    discord.release.notify_one();
    assert_eq!(sweep.await.expect("sweep task").expect("sweep"), 1);
    ban.await.expect("ban task").expect("ban after unban");
    assert_eq!(discord.calls.call_count("ban"), 1);
}

#[tokio::test]
async fn cancelled_sweep_does_not_reserve_undispatched_jobs() {
    let store = MemMemberStore::new();
    for user in ["1", "2", "3"] {
        store
            .stage_unban(GUILD, user, NOW, "expiry", user, NOW)
            .await
            .expect("stage");
        store
            .confirm_ban_attempt(
                GUILD,
                user,
                user,
                store.ban_attempt(user).expect("fixture attempt"),
                NOW,
            )
            .await
            .expect("accepted");
        store
            .activate_staged_unban(GUILD, user, user, NOW)
            .await
            .expect("activate");
    }
    let discord = HeldUnban {
        calls: MockMemberDiscord::new(),
        entered: Arc::new(Notify::new()),
        release: Arc::new(Notify::new()),
    };
    let svc = MemberModerationService::new(discord.clone(), store.clone(), policy(), || {
        1_700_000_000_000
    });
    {
        let sweep = svc.run_due_unbans(GUILD);
        tokio::pin!(sweep);
        tokio::select! {
            _ = &mut sweep => panic!("held call must not finish"),
            result = tokio::time::timeout(Duration::from_secs(5), discord.entered.notified()) => { result.expect("unban entered"); }
        }
    }
    assert_eq!(
        discord
            .calls
            .calls()
            .iter()
            .filter(|c| matches!(c, DiscordCall::Unban { .. }))
            .count(),
        1
    );
    assert_eq!(
        store
            .claim_due_unbans(GUILD, NOW, 25)
            .await
            .expect("remaining")
            .len(),
        2
    );
}
