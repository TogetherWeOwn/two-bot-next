//! Resolver-boundary controls supplement the immutable acceptance modules.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::sync::Notify;
use two_bot_core::member_moderation::{
    DiscordError, MemMemberStore, MemberAuthority, MemberError, MemberExecution,
    MemberModerationService, MemberModerationStore, MockMemberDiscord,
};
use two_bot_core::{ModerationAction, ModerationActor, ModerationPolicy, ModerationTarget};

fn request(action: ModerationAction, key: &str) -> MemberExecution {
    MemberExecution {
        action,
        guild_id: "2222".into(),
        actor: ModerationActor {
            user_id: "88".into(),
            role_ids: vec![],
            highest_role_position: 50,
            permissions: u64::MAX,
        },
        target: Some(ModerationTarget {
            user_id: "3333".into(),
            role_ids: vec![],
            highest_role_position: 0,
            is_bot: false,
            is_guild_owner: false,
        }),
        bot_highest_role_position: 100,
        reason: "resolver fixture".into(),
        duration_seconds: Some(60),
        request_id: key.into(),
        idempotency_key: key.into(),
    }
}

fn authority(exec: &MemberExecution) -> MemberAuthority {
    MemberAuthority {
        actor: exec.actor.clone(),
        target: exec.target.clone(),
        bot_highest_role_position: exec.bot_highest_role_position,
    }
}

fn policy() -> ModerationPolicy {
    ModerationPolicy {
        owen_user_id: "1".into(),
        protected_role_ids: Default::default(),
        bot_user_id: Some("4444".into()),
    }
}

#[tokio::test]
async fn mismatched_resolver_identities_and_missing_targets_are_audited_without_preparing_effects()
{
    for action in [
        ModerationAction::Ban,
        ModerationAction::TempBan,
        ModerationAction::Kick,
        ModerationAction::Timeout,
        ModerationAction::Warn,
    ] {
        for mismatch in ["actor", "target", "missing"] {
            let store = MemMemberStore::new();
            let discord = MockMemberDiscord::new();
            let service =
                MemberModerationService::new(discord.clone(), store.clone(), policy(), || {
                    1_700_000_000_000
                });
            let exec = request(action, mismatch);
            let mut facts = authority(&exec);
            match mismatch {
                "actor" => facts.actor.user_id = "99".into(),
                "target" => facts.target.as_mut().expect("target").user_id = "99".into(),
                _ => facts.target = None,
            }
            assert_eq!(
                service
                    .execute_with_authority(&exec, || async { Ok(facts) })
                    .await,
                Err(MemberError::AuthorityUnavailable)
            );
            assert!(discord.calls().is_empty());
            assert!(store.ban_attempt(mismatch).is_none());
            assert!(store.unban_state(mismatch).is_none());
            let audits = store.audits();
            assert_eq!(audits.len(), 1);
            assert_eq!(audits[0].actor_id, "88");
            assert_eq!(audits[0].target_id.as_deref(), Some("3333"));
            assert_eq!(audits[0].outcome, "denied");
            assert!(audits[0].metadata_json.contains("authority"));
            // No claim was reserved by the refusal; same content can execute.
            assert!(
                !service
                    .execute(&exec)
                    .await
                    .expect("corrected authority")
                    .replayed
            );
        }
    }
}

#[tokio::test]
async fn resolver_failure_never_releases_an_unknown_or_completed_key() {
    for uncertain in [false, true] {
        let store = MemMemberStore::new();
        let discord = MockMemberDiscord::new();
        let service =
            MemberModerationService::new(discord.clone(), store.clone(), policy(), || {
                1_700_000_000_000
            });
        let exec = request(ModerationAction::TempBan, "original");
        if uncertain {
            discord.fail_with("ban", DiscordError::Timeout);
        }
        let first = service.execute(&exec).await;
        assert_eq!(first.is_err(), uncertain);
        let before = store.ban_attempt("original");
        let expiry = store.unban_state("original");
        assert_eq!(
            service
                .execute_with_authority(&exec, || async { Err(MemberError::AuthorityUnavailable) })
                .await,
            Err(MemberError::AuthorityUnavailable)
        );
        assert_eq!(store.ban_attempt("original"), before);
        assert_eq!(store.unban_state("original"), expiry);
        let retry = service.execute(&exec).await;
        if uncertain {
            assert_eq!(retry, Err(MemberError::InFlight));
        } else {
            assert!(retry.expect("completed replay").replayed);
        }
        assert_eq!(discord.call_count("ban"), 1);
        assert_eq!(
            store
                .audits()
                .iter()
                .filter(|a| a.outcome == "denied")
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn resolver_itself_waits_for_the_member_lane_and_another_member_is_independent() {
    for action in [ModerationAction::Ban, ModerationAction::TempBan] {
        let store = MemMemberStore::new();
        let discord = MockMemberDiscord::new();
        let service =
            MemberModerationService::new(discord.clone(), store.clone(), policy(), || {
                1_700_000_000_000
            });
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (inside, gate, held) = (entered.clone(), release.clone(), store.clone());
        let holder = tokio::spawn(async move {
            held.serialize_member("2222", "3333", || async {
                inside.notify_one();
                gate.notified().await;
            })
            .await;
        });
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .expect("held lane");
        let read = AtomicBool::new(false);
        let exec = request(action, "queued");
        let mut waiting = service.execute_with_authority(&exec, || async {
            read.store(true, Ordering::SeqCst);
            let mut facts = authority(&exec);
            facts.actor.permissions = 0;
            Ok(facts)
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut waiting)
                .await
                .is_err()
        );
        assert!(
            !read.load(Ordering::SeqCst),
            "no authority snapshot before acquiring lane"
        );
        let mut other = request(ModerationAction::Ban, "other");
        other.target.as_mut().expect("target").user_id = "7777".into();
        tokio::time::timeout(Duration::from_secs(5), service.execute(&other))
            .await
            .expect("independent member")
            .expect("other effect");
        release.notify_one();
        holder.await.expect("holder");
        assert!(matches!(waiting.await, Err(MemberError::Policy(_))));
        assert!(read.load(Ordering::SeqCst));
        assert_eq!(discord.call_count("ban"), 1);
        assert!(store.ban_attempt("queued").is_none());
        assert!(store.unban_state("queued").is_none());
    }
}
