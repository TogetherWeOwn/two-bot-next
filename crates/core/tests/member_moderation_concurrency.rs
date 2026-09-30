use std::sync::Arc;
use tokio::sync::Notify;
use two_bot_core::member_moderation::{ClaimState, MemMemberStore, MemberModerationStore};

const NOW: &str = "2023-11-14T22:13:20.000Z";

#[tokio::test]
async fn sweep_does_not_recover_a_live_prepared_ban() {
    let store = MemMemberStore::new();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let ban_store = store.clone();
    let ban_entered = entered.clone();
    let ban_release = release.clone();
    let ban = tokio::spawn(async move {
        ban_store
            .serialize_member("guild", "user", || async {
                ban_store
                    .stage_unban("guild", "user", NOW, "expiry", "live", NOW)
                    .await
                    .expect("stage");
                ban_entered.notify_one();
                ban_release.notified().await;
                ban_store
                    .reject_ban("guild", "user", "live", NOW)
                    .await
                    .expect("reject");
            })
            .await;
    });
    entered.notified().await;
    assert!(store
        .claim_due_unbans("guild", NOW, 25)
        .await
        .expect("prepared excluded")
        .is_empty());
    release.notify_one();
    ban.await.expect("ban task");
    assert!(store
        .claim_due_unbans("guild", NOW, 25)
        .await
        .expect("rejected excluded")
        .is_empty());
    assert_eq!(store.unban_state("live").as_deref(), Some("cancelled"));
}

#[tokio::test]
async fn accepted_recovery_waits_on_owning_member_queue() {
    let store = MemMemberStore::new();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let ban_store = store.clone();
    let ban_entered = entered.clone();
    let ban_release = release.clone();
    let ban = tokio::spawn(async move {
        ban_store
            .serialize_member("guild", "user", || async {
                ban_store
                    .stage_unban("guild", "user", NOW, "expiry", "accepted", NOW)
                    .await
                    .expect("stage");
                ban_store
                    .confirm_ban("guild", "user", "accepted", NOW)
                    .await
                    .expect("acceptance");
                ban_entered.notify_one();
                ban_release.notified().await;
            })
            .await;
    });
    entered.notified().await;
    let sweep_store = store.clone();
    let mut sweep =
        tokio::spawn(async move { sweep_store.claim_due_unbans("guild", NOW, 25).await });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), &mut sweep)
            .await
            .is_err()
    );
    release.notify_one();
    ban.await.expect("ban task");
    assert_eq!(sweep.await.expect("sweep task").expect("recovery").len(), 1);
}

#[tokio::test]
async fn newer_expiry_supersedes_older_accepted_crash_staged_jobs() {
    let store = MemMemberStore::new();
    let later = "2023-11-14T23:13:20.000Z";
    for user in ["activated", "recovered"] {
        let old = format!("old-{user}");
        let new = format!("new-{user}");
        store
            .stage_unban("guild", user, NOW, "old expiry", &old, NOW)
            .await
            .expect("old stage");
        store
            .confirm_ban("guild", user, &old, NOW)
            .await
            .expect("old accepted");
        store
            .stage_unban("guild", user, later, "extension", &new, NOW)
            .await
            .expect("new stage");
        store
            .confirm_ban("guild", user, &new, NOW)
            .await
            .expect("new accepted");
        if user == "activated" {
            store
                .activate_staged_unban("guild", user, &new, NOW)
                .await
                .expect("new activation");
        }
    }
    assert!(store
        .claim_due_unbans("guild", NOW, 25)
        .await
        .expect("no early expiry")
        .is_empty());
    assert_eq!(
        store.unban_state("old-activated").as_deref(),
        Some("superseded")
    );
    assert_eq!(
        store.unban_state("old-recovered").as_deref(),
        Some("superseded")
    );
    let due = store
        .claim_due_unbans("guild", later, 25)
        .await
        .expect("later expiry");
    assert_eq!(due.len(), 2);
    assert!(due.iter().all(|job| job.request_id.starts_with("new-")));
}

#[tokio::test]
async fn claims_and_retry_tokens_have_single_owners() {
    let store = MemMemberStore::new();
    let (a, b) = tokio::join!(
        store.claim("guild", "key", "moderation.ban", "hash", NOW),
        store.claim("guild", "key", "moderation.ban", "hash", NOW),
    );
    assert_eq!(a.expect("first"), ClaimState::Claimed);
    assert_eq!(b.expect("second"), ClaimState::InFlight);
    store
        .stage_unban("guild", "user", NOW, "expiry", "request", NOW)
        .await
        .expect("stage");
    store
        .confirm_ban("guild", "user", "request", NOW)
        .await
        .expect("acceptance");
    let job = store
        .claim_due_unbans("guild", NOW, 25)
        .await
        .expect("recovery")
        .pop()
        .expect("job");
    store
        .requeue_unban(&job.request_id, &job.claim_token)
        .await
        .expect("safe retry");
    let next = store
        .claim_due_unbans("guild", NOW, 25)
        .await
        .expect("new claim")
        .pop()
        .expect("job");
    assert_ne!(job.claim_token, next.claim_token);
    assert!(!store
        .owns_unban_claim(&job.request_id, &job.claim_token)
        .await
        .expect("old owner"));
    assert!(store
        .complete_unban(&job.request_id, &job.claim_token)
        .await
        .is_err());
    store
        .requeue_unban(&job.request_id, &job.claim_token)
        .await
        .expect("stale requeue ignored");
    assert!(store
        .owns_unban_claim(&next.request_id, &next.claim_token)
        .await
        .expect("new owner preserved"));
    assert!(store
        .claim_due_unbans("guild", "2024-11-14T22:13:20.000Z", 25)
        .await
        .expect("no timed takeover")
        .is_empty());
}
