//! Protected behavioral contract for lifecycle refusal evidence. These tests
//! exercise real commands and stores, not a replacement renderer or source grep.
#![cfg(test)]

use super::*;

fn refusal_evidence(reply: &str, operation: &str, id: &str, count: usize) {
    let lower = reply.to_ascii_lowercase();
    assert!(
        reply.contains(id),
        "keep the saved relay addressable: {reply}"
    );
    assert!(
        lower.contains("snapshot"),
        "not a live/completion count: {reply}"
    );
    assert!(
        lower.contains("unresolved") && lower.contains("delivery") && lower.contains("evidence"),
        "label uncertain delivery evidence, not failed or successful posts: {reply}"
    );
    let numbers: Vec<_> = reply
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .collect();
    assert_eq!(
        numbers,
        vec![count.to_string()],
        "one guild/relay-scoped evidence count: {reply}"
    );
    let unchanged = match operation {
        "feed-pause" => {
            lower.contains("not paused")
                || lower.contains("kept enabled")
                || lower.contains("remains enabled")
        }
        "feed-remove" => {
            lower.contains("not removed")
                || lower.contains("kept")
                || lower.contains("remains saved")
        }
        _ => unreachable!("test operation"),
    };
    assert!(
        unchanged,
        "explicitly say the requested change did not happen: {reply}"
    );
    assert!(
        lower.contains("wait") && (lower.contains("reconcil") || lower.contains("recovery")),
        "safe next action is waiting for reconciliation: {reply}"
    );
    assert!(
        lower.contains("/feed-inspect"),
        "inspect the saved relay, not recreate it: {reply}"
    );
    for misleading in [
        "delivery failures",
        "failed deliveries",
        "successfully posted",
        "posts sent",
        "in 60 seconds",
        "in 5 minutes",
    ] {
        assert!(
            !lower.contains(misleading),
            "do not invent a failure, success or ETA: {reply}"
        );
    }
    assert!(reply.encode_utf16().count() <= 2000);
}

async fn audit_outcomes(pool: &PgPool, action: &str, id: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT outcome FROM announcements_audit_log WHERE guild_id = $1 AND action = $2 AND target_key = $3 ORDER BY created_at, id")
        .bind(GUILD).bind(action).bind(id).fetch_all(pool).await.unwrap()
}

async fn scoped_refusal(operation: &str, count: usize) {
    let db = fixture().await;
    let relay = feed("selected-relay");
    let other = feed("other-private-relay");
    let mut foreign = feed("foreign-private-relay");
    foreign.guild_id = "9999".into();
    foreign.source = "https://foreign.example/secret-source".into();
    for row in [&relay, &other, &foreign] {
        store::add_feed(db.pool(), row).await.unwrap();
    }
    accepted(db.pool(), &relay, 100).await;
    let mut secrets = vec![
        relay.source.clone(),
        other.id.clone(),
        foreign.id.clone(),
        foreign.source.clone(),
    ];
    for index in 0..count {
        let post = seed_pending(db.pool(), &relay, index).await;
        secrets.extend([post.item_key, post.nonce, format!("pending-seed-{index}")]);
    }
    // Fresh, expired and restored tokenless rows are all unresolved evidence.
    sqlx::query("UPDATE feed_deliveries SET claimed_at = now() - interval '120 seconds' WHERE feed_id = $1 AND item_key = 'item-1'")
        .bind(&relay.id).execute(db.pool()).await.unwrap();
    sqlx::query("UPDATE feed_deliveries SET claimed_at = NULL, claim_token = NULL WHERE feed_id = $1 AND item_key = 'item-2'")
        .bind(&relay.id).execute(db.pool()).await.unwrap();
    for index in 0..7 {
        seed_pending(db.pool(), &other, index).await;
        let post = plan_post(&foreign, &item(index)).unwrap();
        assert_eq!(
            store::claim_delivery(
                db.pool(),
                &foreign.guild_id,
                &post,
                "foreign-private-claim",
                now_millis_for_test()
            )
            .await
            .unwrap(),
            Some(DeliveryClaim::Fresh)
        );
    }
    let before = snapshot(db.pool(), true).await;
    let reply = command(db.pool(), operation, &relay.id).await;
    let after = snapshot(db.pool(), true).await;
    let outcomes = audit_outcomes(db.pool(), &operation.replace('-', "."), &relay.id).await;
    db.close().await.unwrap();
    assert_eq!(
        after, before,
        "refusal preserves every relay/high-water/ledger column, including leases"
    );
    assert_eq!(
        outcomes,
        vec!["recovery_required"],
        "retain the existing refusal audit outcome"
    );
    refusal_evidence(&reply, operation, &relay.id, count);
    for secret in secrets {
        assert!(
            !reply.contains(&secret),
            "no item, source, nonce, claim token or other relay evidence in a private reply"
        );
    }
}

#[tokio::test]
async fn pause_explains_one_pending_evidence_snapshot_without_mutating_history() {
    scoped_refusal("feed-pause", 1).await;
}

#[tokio::test]
async fn removal_explains_one_pending_evidence_snapshot_without_mutating_history() {
    scoped_refusal("feed-remove", 1).await;
}

#[tokio::test]
async fn pause_counts_fresh_expired_and_tokenless_evidence_but_not_settled_or_foreign_rows() {
    scoped_refusal("feed-pause", 3).await;
}

#[tokio::test]
async fn removal_counts_fresh_expired_and_tokenless_evidence_but_not_settled_or_foreign_rows() {
    scoped_refusal("feed-remove", 3).await;
}

#[tokio::test]
async fn healthy_control_zero_pending_and_settled_history_keep_normal_lifecycle_outcomes() {
    let db = fixture().await;
    for operation in ["feed-pause", "feed-remove"] {
        for settled in [false, true] {
            let relay = feed(&format!("control-{operation}-{settled}"));
            store::add_feed(db.pool(), &relay).await.unwrap();
            if settled {
                accepted(db.pool(), &relay, 0).await;
            }
            let before = snapshot(db.pool(), false).await;
            let reply = command(db.pool(), operation, &relay.id).await;
            let lower = reply.to_ascii_lowercase();
            assert!(
                !lower.contains("unresolved") && !lower.contains("snapshot"),
                "zero is not a recovery refusal: {reply}"
            );
            if operation == "feed-pause" {
                assert!(lower.contains("paused"));
                assert!(!enabled(db.pool(), &relay.id).await);
                assert_eq!(snapshot(db.pool(), false).await, before);
            } else {
                assert_eq!(reply, "Feed relay removed.");
                assert_eq!(relay_count(db.pool(), &relay.id).await, 0);
                assert!(delivery_rows(db.pool(), &relay.id).await.is_empty());
            }
        }
    }
    db.close().await.unwrap();
}

#[tokio::test]
async fn healthy_control_missing_foreign_and_already_paused_pending_are_not_count_refusals() {
    let db = fixture().await;
    let mut foreign = feed("private-foreign-id");
    foreign.guild_id = "9999".into();
    store::add_feed(db.pool(), &foreign).await.unwrap();
    let foreign_post = plan_post(&foreign, &item(0)).unwrap();
    store::claim_delivery(
        db.pool(),
        &foreign.guild_id,
        &foreign_post,
        "foreign-token",
        now_millis_for_test(),
    )
    .await
    .unwrap();
    let relay = feed("restored-paused");
    store::add_feed(db.pool(), &relay).await.unwrap();
    seed_pending(db.pool(), &relay, 0).await;
    sqlx::query("UPDATE feed_relays SET enabled = false WHERE id = $1")
        .bind(&relay.id)
        .execute(db.pool())
        .await
        .unwrap();
    let before = snapshot(db.pool(), true).await;
    for operation in ["feed-pause", "feed-remove"] {
        for id in ["missing-relay", foreign.id.as_str()] {
            assert_eq!(
                command(db.pool(), operation, id).await,
                "No feed relay with that id."
            );
        }
        invoke_silent(db.pool(), operation, Some(&relay.id), 32, "9999").await;
    }
    for _ in 0..2 {
        let reply = command(db.pool(), "feed-pause", &relay.id).await;
        assert!(reply.to_ascii_lowercase().contains("already paused"));
        assert!(!reply.to_ascii_lowercase().contains("snapshot"));
    }
    assert_eq!(snapshot(db.pool(), true).await, before);
    assert_eq!(
        audit_outcomes(db.pool(), "feed.pause", &relay.id).await,
        vec!["already_paused", "already_paused"]
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn healthy_control_store_failure_does_not_fabricate_an_evidence_count_or_success() {
    let db = fixture().await;
    let relay = feed("storage-failure");
    store::add_feed(db.pool(), &relay).await.unwrap();
    seed_pending(db.pool(), &relay, 0).await;
    let before = snapshot(db.pool(), true).await;
    sqlx::query("ALTER TABLE feed_deliveries RENAME TO fixture_unavailable_deliveries")
        .execute(db.pool())
        .await
        .unwrap();
    let mut replies = Vec::new();
    for operation in ["feed-pause", "feed-remove"] {
        replies.push(command(db.pool(), operation, &relay.id).await);
    }
    sqlx::query("ALTER TABLE fixture_unavailable_deliveries RENAME TO feed_deliveries")
        .execute(db.pool())
        .await
        .unwrap();
    assert_eq!(snapshot(db.pool(), true).await, before);
    for action in ["feed.pause", "feed.remove"] {
        assert_eq!(
            audit_outcomes(db.pool(), action, &relay.id).await,
            vec!["rejected"]
        );
    }
    db.close().await.unwrap();
    for reply in replies {
        assert_eq!(reply, "Feed command failed; try again.");
    }
}

#[tokio::test]
async fn count_snapshot_includes_claim_admitted_ahead_of_each_lifecycle_fence_waiter() {
    for operation in ["feed-pause", "feed-remove"] {
        let db = fixture().await;
        let relay = feed("fenced-count");
        store::add_feed(db.pool(), &relay).await.unwrap();
        seed_pending(db.pool(), &relay, 0).await;
        let before = snapshot(db.pool(), true).await;
        let holder = hold_feed_delivery_advisory_lock(db.pool(), GUILD, &relay.id).await;
        let claimant = db.independent_pool().await.unwrap();
        let claim_pool = claimant.clone();
        let post = plan_post(&relay, &item(1)).unwrap();
        let claim_post = post.clone();
        let claim = tokio::spawn(async move {
            store::claim_fresh_delivery(
                &claim_pool,
                GUILD,
                &claim_post,
                "queued-claim",
                now_millis_for_test(),
            )
            .await
        });
        let first = wait_for_advisory_waiters(db.pool(), GUILD, &relay.id, 1)
            .await
            .unwrap();
        let commander = db.independent_pool().await.unwrap();
        let command_pool = commander.clone();
        let id = relay.id.clone();
        let task = tokio::spawn(async move { command(&command_pool, operation, &id).await });
        let both = wait_for_advisory_waiters(db.pool(), GUILD, &relay.id, 2)
            .await
            .unwrap();
        holder.commit().await.unwrap();
        let admitted = claim.await.unwrap().unwrap();
        let reply = task.await.unwrap();
        let after = snapshot(db.pool(), true).await;
        claimant.close().await;
        commander.close().await;
        db.close().await.unwrap();
        assert!(
            first && both,
            "observe actual lock queue ordering before release"
        );
        assert_eq!(admitted, Some(DeliveryClaim::Fresh));
        assert_eq!(
            after["relays"], before["relays"],
            "refusal cannot change the relay or its high-water"
        );
        assert_eq!(after["deliveries"].as_array().unwrap().len(), 2);
        assert_eq!(
            after["deliveries"][0], before["deliveries"][0],
            "existing claim is not renewed or reconciled"
        );
        assert_eq!(after["deliveries"][1]["item_key"], post.item_key);
        assert_eq!(after["deliveries"][1]["nonce"], post.nonce);
        assert_eq!(after["deliveries"][1]["claim_token"], "queued-claim");
        assert_eq!(after["deliveries"][1]["state"], "pending");
        refusal_evidence(&reply, operation, &relay.id, 2);
    }
}

#[tokio::test]
async fn healthy_control_settlement_inside_fence_wait_is_not_reported_as_stale_pending() {
    for operation in ["feed-pause", "feed-remove"] {
        let db = fixture().await;
        let relay = feed("settled-during-wait");
        store::add_feed(db.pool(), &relay).await.unwrap();
        seed_pending(db.pool(), &relay, 0).await;
        let mut holder = hold_feed_delivery_advisory_lock(db.pool(), GUILD, &relay.id).await;
        let commander = db.independent_pool().await.unwrap();
        let command_pool = commander.clone();
        let id = relay.id.clone();
        let task = tokio::spawn(async move { command(&command_pool, operation, &id).await });
        let queued = wait_for_advisory_waiters(db.pool(), GUILD, &relay.id, 1)
            .await
            .unwrap();
        sqlx::query("UPDATE feed_deliveries SET state = 'delivered', message_id = '9001', delivered_at = now() WHERE feed_id = $1")
            .bind(&relay.id).execute(&mut *holder).await.unwrap();
        holder.commit().await.unwrap();
        let reply = task.await.unwrap();
        let remaining = relay_count(db.pool(), &relay.id).await;
        if operation == "feed-pause" {
            assert!(!enabled(db.pool(), &relay.id).await);
        }
        commander.close().await;
        db.close().await.unwrap();
        assert!(queued, "settlement precedes the command's fenced decision");
        assert_eq!(remaining, if operation == "feed-pause" { 1 } else { 0 });
        assert!(!reply.to_ascii_lowercase().contains("snapshot"));
        assert!(reply.contains(if operation == "feed-pause" {
            "paused"
        } else {
            "removed"
        }));
    }
}

#[tokio::test]
async fn large_pending_ledger_projects_only_a_bounded_private_summary() {
    let db = fixture().await;
    let mut relay = feed("large-ledger");
    relay.id = "a".repeat(128);
    store::add_feed(db.pool(), &relay).await.unwrap();
    sqlx::query("INSERT INTO feed_deliveries (feed_id, item_key, nonce, state, first_seen_at, claim_token, claimed_at) SELECT $1, 'private-item-' || n, 'private-nonce-' || n, 'pending', now(), 'private-lease-' || n, now() FROM generate_series(1, 2048) AS n")
        .bind(&relay.id).execute(db.pool()).await.unwrap();
    let before = snapshot(db.pool(), true).await;
    let mut replies = Vec::new();
    for operation in ["feed-pause", "feed-remove"] {
        replies.push(command(db.pool(), operation, &relay.id).await);
    }
    assert_eq!(snapshot(db.pool(), true).await, before);
    db.close().await.unwrap();
    for reply in replies {
        assert!(reply.contains(&relay.id));
        assert!(reply.encode_utf16().count() <= 2000);
        let lower = reply.to_ascii_lowercase();
        assert!(lower.contains("snapshot") && lower.contains("evidence"));
        let numbers: Vec<usize> = reply
            .split(|c: char| !c.is_ascii_digit())
            .filter(|part| !part.is_empty())
            .map(|part| part.parse().unwrap())
            .collect();
        assert_eq!(numbers.len(), 1, "one summary, never an item list: {reply}");
        assert!(numbers[0] > 0 && numbers[0] <= 2048);
        if numbers[0] < 2048 {
            assert!(
                lower.contains("at least") || reply.contains('+') || reply.contains('≥'),
                "a capped count is explicitly a lower bound: {reply}"
            );
        }
        for secret in [
            "private-item-",
            "private-nonce-",
            "private-lease-",
            "https://",
        ] {
            assert!(!reply.contains(secret));
        }
    }
}
