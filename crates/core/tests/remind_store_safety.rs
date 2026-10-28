//! Protected reminder-store safety acceptance. Synthetic disposable databases only.
//! Breaks caught: non-atomic owner capacity, replay resurrection, erased replay
//! tombstones, cancellation across owner/guild boundaries, ambiguous prefix
//! cancellation, staff quota/list leakage, and stale claim completion after cancel.
#![cfg(feature = "db")]

use std::sync::Arc;

use two_bot_core::remind_store::{self, Admission};
use two_bot_core::scheduled_store::{self, ScheduledWrite};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "2222";
const OWNER: &str = "881001001001";
const NOW: &str = "2026-10-01T00:00:00.000Z";
const LATER: &str = "2026-10-01T00:02:00.000Z";

async fn fixture() -> Option<TestDatabase> {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI requires test DB"
        );
        eprintln!("SKIP reminder store safety: TWO_TEST_DATABASE_URL is not set");
        return None;
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("migrated disposable database"),
    )
}

fn row(id: &str) -> ScheduledWrite {
    ScheduledWrite {
        id: id.to_owned(),
        guild_id: GUILD.to_owned(),
        channel_id: "3333".to_owned(),
        body: format!("Reminder for {OWNER}: {id}"),
        next_run_at: NOW.to_owned(),
        interval_seconds: None,
        enabled: true,
        created_by: OWNER.to_owned(),
        created_at: NOW.to_owned(),
        updated_by: OWNER.to_owned(),
        updated_at: NOW.to_owned(),
    }
}

async fn row_count(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM scheduled_messages WHERE guild_id = $1")
        .bind(GUILD)
        .fetch_one(pool)
        .await
        .expect("scoped row count")
}

#[tokio::test]
async fn independent_pools_admit_exactly_five_concurrent_creates_per_owner() {
    let Some(db) = fixture().await else {
        return;
    };
    let peer = db
        .independent_pool()
        .await
        .expect("independent worker pool");
    let barrier = Arc::new(tokio::sync::Barrier::new(12));
    let mut tasks = Vec::new();
    for index in 0..12 {
        let pool = if index % 2 == 0 {
            db.pool().clone()
        } else {
            peer.clone()
        };
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            let write = row(&format!("remind-race-{index:02}"));
            barrier.wait().await;
            remind_store::create(&pool, &write)
                .await
                .expect("concurrent admission")
        }));
    }
    let mut created = 0;
    let mut capacity = 0;
    for task in tasks {
        match task.await.expect("worker") {
            Admission::Created => created += 1,
            Admission::Capacity => capacity += 1,
            Admission::Existing => panic!("distinct invocations cannot be replays"),
        }
    }
    assert_eq!(
        (created, capacity),
        (5, 7),
        "capacity is atomic across pools"
    );
    assert_eq!(
        row_count(db.pool()).await,
        5,
        "refusals insert no tombstones"
    );
    let live = remind_store::list(&peer, GUILD, OWNER)
        .await
        .expect("owner list");
    assert_eq!(live.len(), 5);
    assert_eq!(
        remind_store::create(&peer, &row(&live[0].id))
            .await
            .expect("replay at cap"),
        Admission::Existing
    );
    let mut other_owner = row("remind-other-owner");
    other_owner.created_by = "882002002002".to_owned();
    other_owner.updated_by.clone_from(&other_owner.created_by);
    assert_eq!(
        remind_store::create(&peer, &other_owner)
            .await
            .expect("other owner"),
        Admission::Created
    );
    let mut other_guild = row("remind-other-guild");
    other_guild.guild_id = "9999".to_owned();
    assert_eq!(
        remind_store::create(&peer, &other_guild)
            .await
            .expect("other guild"),
        Admission::Created
    );
    peer.close().await;
    db.close().await.expect("drop disposable DB");
}

#[tokio::test]
async fn identical_invocation_is_existing_before_and_after_completion_and_cancel() {
    let Some(db) = fixture().await else {
        return;
    };
    let peer = db.independent_pool().await.expect("restart pool");
    let completed = row("remind-completed-replay");
    assert_eq!(
        remind_store::create(db.pool(), &completed)
            .await
            .expect("create"),
        Admission::Created
    );
    assert_eq!(
        remind_store::create(&peer, &completed)
            .await
            .expect("pending replay"),
        Admission::Existing
    );
    let claimed = scheduled_store::claim_due(db.pool(), GUILD, NOW, "claim-a", LATER, "nonce-a")
        .await
        .expect("claim pending reminder");
    assert_eq!(claimed.len(), 1);
    assert!(scheduled_store::complete_run(
        db.pool(),
        GUILD,
        &completed.id,
        NOW,
        Some("9001"),
        "claim-a"
    )
    .await
    .expect("complete")
    .is_some());
    assert_eq!(
        remind_store::create(&peer, &completed)
            .await
            .expect("completed replay"),
        Admission::Existing
    );
    let persisted = scheduled_store::get_scheduled(&peer, GUILD, &completed.id)
        .await
        .expect("read tombstone")
        .expect("retained");
    assert!(
        !persisted.enabled,
        "completion replay cannot resurrect delivery"
    );
    assert_eq!(persisted.last_message_id.as_deref(), Some("9001"));

    let cancelled = row("remind-cancelled-replay");
    assert_eq!(
        remind_store::create(&peer, &cancelled)
            .await
            .expect("second create"),
        Admission::Created
    );
    assert!(
        remind_store::cancel(db.pool(), GUILD, OWNER, &cancelled.id, NOW)
            .await
            .expect("cancel")
    );
    assert_eq!(
        remind_store::create(&peer, &cancelled)
            .await
            .expect("cancel replay"),
        Admission::Existing
    );
    assert!(
        !scheduled_store::get_scheduled(&peer, GUILD, &cancelled.id)
            .await
            .expect("read")
            .expect("cancel tombstone retained")
            .enabled
    );
    assert!(remind_store::list(&peer, GUILD, OWNER)
        .await
        .expect("no pending")
        .is_empty());
    assert_eq!(
        row_count(&peer).await,
        2,
        "disabled replay tombstones are retained, not duplicated"
    );
    assert!(scheduled_store::claim_due(
        &peer,
        GUILD,
        LATER,
        "claim-b",
        "2026-10-01T00:04:00.000Z",
        "nonce-b"
    )
    .await
    .expect("later claim")
    .is_empty());
    // Disabled rows free capacity without forgetting their invocation identity.
    for index in 0..5 {
        assert_eq!(
            remind_store::create(&peer, &row(&format!("remind-new-{index}")))
                .await
                .expect("new invocation"),
            Admission::Created
        );
    }
    assert_eq!(
        remind_store::create(&peer, &cancelled)
            .await
            .expect("old invocation at cap"),
        Admission::Existing
    );
    assert_eq!(row_count(&peer).await, 7);
    peer.close().await;
    db.close().await.expect("drop disposable DB");
}

#[tokio::test]
async fn cancel_is_owner_guild_scoped_and_requires_a_unique_id_prefix_or_exact_text() {
    let Some(db) = fixture().await else {
        return;
    };
    let mut alpha = row("remind-aa-one");
    alpha.body = two_bot_core::remind::delivery_body(OWNER, "shared dentist appointment");
    let mut beta = row("remind-aa-two");
    beta.body = two_bot_core::remind::delivery_body(OWNER, "shared dentist appointment");
    for write in [&alpha, &beta] {
        assert_eq!(
            remind_store::create(db.pool(), write).await.expect("seed"),
            Admission::Created
        );
    }
    for target in [alpha.id.as_str(), "shared dentist appointment"] {
        assert!(
            !remind_store::cancel(db.pool(), GUILD, "882002002002", target, NOW)
                .await
                .expect("other owner refuses")
        );
        assert!(!remind_store::cancel(db.pool(), "9999", OWNER, target, NOW)
            .await
            .expect("other guild refuses"));
    }
    for target in [
        "",
        "   ",
        "remind-aa-",
        "shared dentist",
        "shared dentist appointment",
        "unknown",
        &"x".repeat(241),
    ] {
        assert!(!remind_store::cancel(db.pool(), GUILD, OWNER, target, NOW)
            .await
            .expect("nonunique/invalid target refuses"));
        assert_eq!(
            remind_store::list(db.pool(), GUILD, OWNER)
                .await
                .expect("unchanged pending")
                .len(),
            2
        );
        assert_eq!(row_count(db.pool()).await, 2, "refusal creates no state");
    }
    assert!(
        remind_store::cancel(db.pool(), GUILD, OWNER, "remind-aa-o", NOW)
            .await
            .expect("unique prefix")
    );
    let left = remind_store::list(db.pool(), GUILD, OWNER)
        .await
        .expect("one survivor");
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].id, beta.id);
    assert!(
        remind_store::cancel(db.pool(), GUILD, OWNER, "shared dentist appointment", NOW)
            .await
            .expect("exact text")
    );
    assert!(remind_store::list(db.pool(), GUILD, OWNER)
        .await
        .expect("empty")
        .is_empty());
    assert_eq!(row_count(db.pool()).await, 2, "cancel retains tombstones");
    db.close().await.expect("drop disposable DB");
}

#[tokio::test]
async fn staff_quota_list_windows_and_id_resolution_exclude_reminders() {
    let Some(db) = fixture().await else {
        return;
    };
    for index in 0..5 {
        assert_eq!(
            remind_store::create(db.pool(), &row(&format!("remind-private-{index}")))
                .await
                .expect("reminder seed"),
            Admission::Created
        );
    }
    for index in 0..25 {
        scheduled_store::put_scheduled(db.pool(), &row(&format!("staff-{index:02}")))
            .await
            .expect("staff seed");
    }
    assert_eq!(
        scheduled_store::count_scheduled(db.pool(), GUILD)
            .await
            .expect("staff quota"),
        25
    );
    let staff = scheduled_store::list_scheduled(db.pool(), GUILD)
        .await
        .expect("staff list");
    assert_eq!(staff.len(), 25);
    assert!(staff.iter().all(|row| row.id.starts_with("staff-")));
    let window = scheduled_store::list_scheduled_window(db.pool(), GUILD, 25, 0)
        .await
        .expect("staff page");
    assert_eq!(window.len(), 25);
    assert!(window.iter().all(|row| row.id.starts_with("staff-")));
    for target in ["remind-private-0", "remind-private-", "remind-"] {
        assert!(
            scheduled_store::resolve_scheduled_id(db.pool(), GUILD, target)
                .await
                .expect("staff resolution")
                .is_none(),
            "staff cannot resolve reserved id {target}"
        );
    }
    assert_eq!(
        remind_store::list(db.pool(), GUILD, OWNER)
            .await
            .expect("personal list excludes staff")
            .len(),
        5
    );
    assert_eq!(row_count(db.pool()).await, 30);
    db.close().await.expect("drop disposable DB");
}

#[tokio::test]
async fn cancelling_a_claim_clears_ownership_and_stale_completion_cannot_resurrect() {
    let Some(db) = fixture().await else {
        return;
    };
    let write = row("remind-claimed-cancel");
    assert_eq!(
        remind_store::create(db.pool(), &write)
            .await
            .expect("create"),
        Admission::Created
    );
    assert_eq!(
        scheduled_store::claim_due(db.pool(), GUILD, NOW, "stale-claim", LATER, "nonce-held")
            .await
            .expect("claim")
            .len(),
        1
    );
    assert!(
        scheduled_store::begin_scheduled_delivery(db.pool(), GUILD, &write.id, "stale-claim")
            .await
            .expect("arm fence")
    );
    assert!(
        remind_store::cancel(db.pool(), GUILD, OWNER, &write.id, NOW)
            .await
            .expect("cancel held reminder")
    );
    let retained = scheduled_store::get_scheduled(db.pool(), GUILD, &write.id)
        .await
        .expect("read")
        .expect("tombstone");
    assert!(!retained.enabled);
    assert!(
        retained.claim_token.is_none(),
        "cancel invalidates old worker ownership"
    );
    assert!(scheduled_store::complete_run(
        db.pool(),
        GUILD,
        &write.id,
        LATER,
        Some("9002"),
        "stale-claim"
    )
    .await
    .expect("stale completion")
    .is_none());
    assert_eq!(
        remind_store::create(db.pool(), &write)
            .await
            .expect("replay"),
        Admission::Existing
    );
    assert!(remind_store::list(db.pool(), GUILD, OWNER)
        .await
        .expect("no pending")
        .is_empty());
    db.close().await.expect("drop disposable DB");
}

async fn reserved_row_rejects_staff_write(delete: bool) {
    let Some(db) = fixture().await else {
        return;
    };
    for state in ["pending", "completed", "cancelled"] {
        let original = row(&format!("remind-staff-{state}"));
        assert_eq!(
            remind_store::create(db.pool(), &original).await.unwrap(),
            Admission::Created
        );
        if state == "completed" {
            let claimed = scheduled_store::claim_due(
                db.pool(),
                GUILD,
                NOW,
                "staff-guard-claim",
                LATER,
                "staff-guard-nonce",
            )
            .await
            .unwrap();
            // A previously pending row also exists; complete only this row's claim.
            assert!(claimed.iter().any(|candidate| candidate.id == original.id));
            assert!(scheduled_store::complete_run(
                db.pool(),
                GUILD,
                &original.id,
                NOW,
                Some("9001"),
                "staff-guard-claim"
            )
            .await
            .unwrap()
            .is_some());
        } else if state == "cancelled" {
            assert!(
                remind_store::cancel(db.pool(), GUILD, OWNER, &original.id, NOW)
                    .await
                    .unwrap()
            );
        }
        let before = scheduled_store::get_scheduled(db.pool(), GUILD, &original.id)
            .await
            .unwrap()
            .unwrap();
        if delete {
            assert!(
                !scheduled_store::delete_scheduled(db.pool(), GUILD, &original.id)
                    .await
                    .unwrap(),
                "staff delete must refuse {state}"
            );
        } else {
            let mut replacement = original.clone();
            replacement.body = "staff overwrite".to_owned();
            replacement.channel_id = "9999".to_owned();
            replacement.enabled = true;
            replacement.interval_seconds = Some(60);
            replacement.updated_by = "staff".to_owned();
            replacement.updated_at = LATER.to_owned();
            assert!(
                !scheduled_store::put_scheduled(db.pool(), &replacement)
                    .await
                    .unwrap(),
                "staff put must refuse {state}"
            );
        }
        let after = scheduled_store::get_scheduled(db.pool(), GUILD, &original.id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            before == after,
            "all persisted {state} fields are unchanged"
        );
        assert_eq!(
            remind_store::create(db.pool(), &original).await.unwrap(),
            Admission::Existing,
            "replay stays fenced for {state}"
        );
        assert!(
            before
                == scheduled_store::get_scheduled(db.pool(), GUILD, &original.id)
                    .await
                    .unwrap()
                    .unwrap()
        );
    }
    db.close().await.unwrap();
}

#[tokio::test]
async fn staff_put_cannot_overwrite_reserved_pending_rows_or_replay_tombstones() {
    reserved_row_rejects_staff_write(false).await;
}

#[tokio::test]
async fn staff_delete_cannot_erase_reserved_pending_rows_or_replay_tombstones() {
    reserved_row_rejects_staff_write(true).await;
}
