use std::collections::HashSet;
use std::sync::atomic::{AtomicI64, Ordering};

use two_bot_core::member_moderation::{
    DiscordError, MemMemberStore, MemberError, MemberExecution, MemberModerationService,
    MemberModerationStore, MockMemberDiscord,
};
use two_bot_core::{ModerationAction, ModerationActor, ModerationPolicy, ModerationTarget};

const GUILD: &str = "100000000000000001";
const USER: &str = "333333333333333333";
const NOW: &str = "2023-11-14T22:13:20.000Z";
const DUE: &str = "2023-11-14T23:13:20.000Z";

#[tokio::test]
async fn stale_acceptance_and_refusal_cannot_resolve_a_retried_put() {
    for temporary in [false, true] {
        let store = MemMemberStore::new();
        let first = if temporary {
            store
                .stage_unban(GUILD, USER, DUE, "expiry", "retry", NOW)
                .await
        } else {
            store.stage_ban(GUILD, USER, "retry", NOW).await
        }
        .unwrap();
        // The unversioned compatibility API is not a reconciliation bypass,
        // even on the first attempt.
        assert!(store.confirm_ban(GUILD, USER, "retry", NOW).await.is_err());
        assert!(store.reject_ban(GUILD, USER, "retry", NOW).await.is_err());
        store
            .reject_ban_attempt(GUILD, USER, "retry", first, NOW)
            .await
            .unwrap();
        let second = if temporary {
            store
                .stage_unban(GUILD, USER, DUE, "retry expiry", "retry", NOW)
                .await
        } else {
            store.stage_ban(GUILD, USER, "retry", NOW).await
        }
        .unwrap();
        assert!(second.generation > first.generation);
        assert!(store
            .confirm_ban_attempt(GUILD, USER, "retry", first, DUE)
            .await
            .is_err());
        assert!(store
            .reject_ban_attempt(GUILD, USER, "retry", first, DUE)
            .await
            .is_err());
        assert_eq!(store.ban_attempt("retry"), Some(second));
        assert_eq!(
            store.unban_state("retry").as_deref(),
            temporary.then_some("staged")
        );
        assert!(store
            .claim_due_unbans(GUILD, DUE, 25)
            .await
            .unwrap()
            .is_empty());
        assert!(store.stage_ban(GUILD, USER, "fresh", DUE).await.is_err());
        store
            .confirm_ban_attempt(GUILD, USER, "retry", second, DUE)
            .await
            .unwrap();
        assert!(store
            .reject_ban_attempt(GUILD, USER, "retry", first, DUE)
            .await
            .is_err());
        let jobs = store.claim_due_unbans(GUILD, DUE, 25).await.unwrap();
        assert_eq!(jobs.len(), usize::from(temporary));
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
            user_id: USER.into(),
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
async fn service_rejection_then_timeout_retains_the_exact_retry_fence() {
    for action in [ModerationAction::Ban, ModerationAction::TempBan] {
        let clock = AtomicI64::new(1_700_000_000_000);
        let store = MemMemberStore::new();
        let discord = MockMemberDiscord::new();
        let policy = ModerationPolicy {
            owen_user_id: "123456789012345678".into(),
            bot_user_id: Some("555555555555555555".into()),
            protected_role_ids: HashSet::new(),
        };
        let svc = MemberModerationService::new(discord.clone(), store.clone(), policy, || {
            clock.load(Ordering::SeqCst)
        });
        svc.execute(&execution(ModerationAction::TempBan, "old-expiry"))
            .await
            .unwrap();
        let audit = &store.audits()[0];
        let metadata: serde_json::Value = serde_json::from_str(&audit.metadata_json).unwrap();
        assert_eq!(
            metadata["ban_attempt_generation"],
            store.ban_attempt("old-expiry").unwrap().generation
        );
        let retry = execution(action, "retry");
        discord.fail_with("ban", DiscordError::Rejected("definite refusal".into()));
        assert!(matches!(
            svc.execute(&retry).await,
            Err(MemberError::Discord(DiscordError::Rejected(_)))
        ));
        let first = store.ban_attempt("retry").unwrap();
        discord.fail_with("ban", DiscordError::Timeout);
        assert!(matches!(
            svc.execute(&retry).await,
            Err(MemberError::Discord(DiscordError::Timeout))
        ));
        let second = store.ban_attempt("retry").unwrap();
        assert!(second.generation > first.generation);
        for identity_only in [false, true] {
            let result = if identity_only {
                store.reject_ban(GUILD, USER, "retry", DUE).await
            } else {
                store
                    .reject_ban_attempt(GUILD, USER, "retry", first, DUE)
                    .await
            };
            assert!(result.is_err());
        }
        assert!(store
            .confirm_ban_attempt(GUILD, USER, "retry", first, DUE)
            .await
            .is_err());
        clock.store(1_700_003_600_000, Ordering::SeqCst);
        assert_eq!(svc.run_due_unbans(GUILD).await.unwrap(), 0);
        assert_eq!(discord.call_count("unban"), 0);
        assert!(matches!(
            svc.execute(&retry).await,
            Err(MemberError::InFlight)
        ));
        assert!(svc
            .execute(&execution(ModerationAction::Ban, "fresh"))
            .await
            .is_err());
        assert_eq!(discord.call_count("ban"), 3);
        // Only authoritative refusal of the second attempt clears this fence.
        store
            .reject_ban_attempt(GUILD, USER, "retry", second, DUE)
            .await
            .unwrap();
        assert_eq!(svc.run_due_unbans(GUILD).await.unwrap(), 1);
        assert_eq!(discord.call_count("unban"), 1);
    }
}
