//! One-shot force-fresh IDENTIFY directive (`docs/gateway-recovery.md`).
use super::*;
use two_bot_core::gateway_session::{boot_action_with, BootAction, BootDirective};
use two_bot_cutover::gateway_session::ArmOutcome;

async fn consumed(db: &TestDb) -> Option<bool> {
    sqlx::query_scalar("SELECT consumed_at IS NOT NULL FROM gateway_boot_directives")
        .fetch_optional(&db.pool)
        .await
        .expect("directive row")
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn armed_boot_identifies_over_a_fresh_checkpoint_then_ready_resumes() {
    let db = TestDb::new().await;
    let mut first = MockGateway::new(false, false).await;
    // Fresh enough that an unarmed boot would RESUME it.
    db.store
        .commit_dispatch(
            &checkpoint("legacy-session", 42, &first.url),
            FunnelBatch::default(),
        )
        .await
        .expect("seed");
    assert_eq!(
        db.store
            .arm_force_identify("first production boot")
            .await
            .unwrap(),
        ArmOutcome::Armed
    );
    let (runner, _) = spawn_runner(&db, &first.url).await;
    assert_eq!(
        first.authentication().await["op"],
        2,
        "armed boot must IDENTIFY"
    );
    wait_sequence(&db.store, 2).await;
    assert_eq!(
        db.store.load().await.unwrap().unwrap().session_id,
        "fresh-session"
    );
    assert_eq!(consumed(&db).await, Some(true));
    runner.abort();
    let _ = runner.await;
    first.task.abort();

    // Next boot after READY persisted: no directive, so the new session RESUMEs.
    let mut second = MockGateway::new(false, true).await;
    sqlx::query("UPDATE gateway_sessions SET resume_url = $1")
        .bind(&second.url)
        .execute(&db.pool)
        .await
        .expect("mock URL");
    let (runner, state) = spawn_runner(&db, "ws://127.0.0.1:1").await;
    let auth = second.authentication().await;
    assert_eq!(auth["op"], 6, "one-shot directive must not repeat");
    assert_eq!(auth["d"]["session_id"], "fresh-session");
    assert_eq!(auth["d"]["seq"], 2);
    wait_sequence(&db.store, 3).await;
    assert_eq!(*state.read().await, GatewayState::Connected);
    runner.abort();
    let _ = runner.await;
    second.task.abort();
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn concurrent_boot_reads_consume_the_directive_exactly_once() {
    let db = TestDb::new().await;
    db.store
        .commit_dispatch(
            &checkpoint("current-session", 42, "ws://mock"),
            FunnelBatch::default(),
        )
        .await
        .expect("seed");
    // Compare durable reads, so millisecond rounding is identical on both sides.
    let saved = db.store.load().await.unwrap();
    db.store
        .arm_force_identify("first production boot")
        .await
        .unwrap();
    // Hold the directive row so both boot reads are in flight on its lock
    // before either can consume it.
    let mut blocker = db.pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM gateway_boot_directives FOR UPDATE")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let boots: Vec<_> = (0..2)
        .map(|_| {
            let store = db.store.clone();
            tokio::spawn(async move { store.load_for_boot().await })
        })
        .collect();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity WHERE wait_event_type = 'Lock'
                 AND query LIKE '%FROM gateway_boot_directives%FOR UPDATE%'",
            )
            .fetch_one(&db.admin)
            .await
            .unwrap();
            if waiting >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("both boot reads wait on the directive lock");
    blocker.rollback().await.unwrap();
    let mut directives = Vec::new();
    for boot in boots {
        let (loaded, directive) = boot.await.unwrap().expect("boot read");
        // The boot read never deletes or rewrites the checkpoint.
        assert!(loaded == saved);
        directives.push(directive);
    }
    directives.sort_by_key(|directive| *directive == BootDirective::ForceIdentify);
    assert_eq!(
        directives,
        [BootDirective::None, BootDirective::ForceIdentify],
        "exactly one boot read consumes the directive"
    );
    assert_eq!(consumed(&db).await, Some(true));
    let (loaded, directive) = db.store.load_for_boot().await.unwrap();
    assert_eq!(directive, BootDirective::None, "the directive is one-shot");
    assert_eq!(
        boot_action_with(
            loaded.as_ref(),
            directive,
            two_bot_core::funnel::now_millis_for_test()
        ),
        BootAction::Resume
    );
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn arming_keeps_a_pending_directive_and_rearms_after_consumption() {
    let db = TestDb::new().await;
    db.store
        .commit_dispatch(
            &checkpoint("current-session", 7, "ws://mock"),
            FunnelBatch::default(),
        )
        .await
        .expect("seed");
    let store = &db.store;
    let saved = store.load().await.unwrap();
    assert_eq!(
        store.arm_force_identify("first").await.unwrap(),
        ArmOutcome::Armed
    );
    let pending = store.force_identify_status().await.unwrap().unwrap();
    assert_eq!(
        store.arm_force_identify("second").await.unwrap(),
        ArmOutcome::AlreadyArmed
    );
    assert_eq!(store.force_identify_status().await.unwrap(), Some(pending));
    assert_eq!(
        store.load_for_boot().await.unwrap().1,
        BootDirective::ForceIdentify
    );
    assert!(store
        .force_identify_status()
        .await
        .unwrap()
        .unwrap()
        .consumed_at_ms
        .is_some());
    assert_eq!(
        store.arm_force_identify("again").await.unwrap(),
        ArmOutcome::Armed
    );
    let rearmed = store.force_identify_status().await.unwrap().unwrap();
    assert_eq!(
        (rearmed.reason.as_str(), rearmed.consumed_at_ms),
        ("again", None)
    );
    // Arming and consuming never touch the checkpoint row.
    assert!(store.load().await.unwrap() == saved);
    db.close().await;
}
