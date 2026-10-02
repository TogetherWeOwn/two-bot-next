use super::*;
use tokio::sync::{oneshot, Notify};

fn counting_action(count: &Arc<AtomicU64>) -> JobAction {
    let count = Arc::clone(count);
    Arc::new(move || {
        let count = Arc::clone(&count);
        Box::pin(async move {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    })
}

async fn settle() {
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn ready_and_exact_timer_cadences_are_independent() {
    let recovery = Arc::new(AtomicU64::new(0));
    let purge = Arc::new(AtomicU64::new(0));
    let (ready, _) = watch::channel(0);
    let (shutdown, _) = watch::channel(false);
    let mut tasks = JoinSet::new();
    tasks.spawn(maintenance_loop(
        "recovery",
        counting_action(&recovery),
        RECOVERY_CADENCE,
        RECOVERY_TIMEOUT,
        ready.subscribe(),
        shutdown.subscribe(),
    ));
    tasks.spawn(maintenance_loop(
        "purge",
        counting_action(&purge),
        PURGE_CADENCE,
        PURGE_TIMEOUT,
        ready.subscribe(),
        shutdown.subscribe(),
    ));
    settle().await;
    assert_eq!(recovery.load(Ordering::SeqCst), 0);
    assert_eq!(purge.load(Ordering::SeqCst), 0);
    ready.send_replace(1);
    settle().await;
    assert_eq!(recovery.load(Ordering::SeqCst), 1);
    assert_eq!(purge.load(Ordering::SeqCst), 1);
    tokio::time::advance(RECOVERY_CADENCE - Duration::from_millis(1)).await;
    settle().await;
    assert_eq!(recovery.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_millis(1)).await;
    settle().await;
    assert_eq!(recovery.load(Ordering::SeqCst), 2);
    assert_eq!(purge.load(Ordering::SeqCst), 1);
    for _ in 0..11 {
        tokio::time::advance(RECOVERY_CADENCE).await;
        settle().await;
    }
    assert_eq!(recovery.load(Ordering::SeqCst), 13);
    assert_eq!(purge.load(Ordering::SeqCst), 2);
    ready.send_replace(2);
    settle().await;
    assert_eq!(recovery.load(Ordering::SeqCst), 14);
    assert_eq!(purge.load(Ordering::SeqCst), 3);
    shutdown.send_replace(true);
    while let Some(result) = tasks.join_next().await {
        result.unwrap();
    }
    tokio::time::advance(PURGE_CADENCE).await;
    assert_eq!(recovery.load(Ordering::SeqCst), 14);
    assert_eq!(purge.load(Ordering::SeqCst), 3);
}

#[tokio::test(start_paused = true)]
async fn busy_ticks_are_not_queued_and_ready_does_not_overlap_work() {
    let count = Arc::new(AtomicU64::new(0));
    let release = Arc::new(Notify::new());
    let action: JobAction = {
        let count = Arc::clone(&count);
        let release = Arc::clone(&release);
        Arc::new(move || {
            let count = Arc::clone(&count);
            let release = Arc::clone(&release);
            Box::pin(async move {
                count.fetch_add(1, Ordering::SeqCst);
                release.notified().await;
                Ok(())
            })
        })
    };
    let (ready, receiver) = watch::channel(0);
    let (shutdown, stopped) = watch::channel(false);
    let task = tokio::spawn(maintenance_loop(
        "slow",
        action,
        RECOVERY_CADENCE,
        Duration::from_secs(5_000),
        receiver,
        stopped,
    ));
    settle().await;
    ready.send_replace(1);
    settle().await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(1_201)).await;
    settle().await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
    release.notify_one();
    settle().await;
    assert_eq!(count.load(Ordering::SeqCst), 1, "no catch-up replay");
    ready.send_replace(2);
    settle().await;
    assert_eq!(count.load(Ordering::SeqCst), 2);
    ready.send_replace(3);
    ready.send_replace(4);
    settle().await;
    assert_eq!(count.load(Ordering::SeqCst), 2, "Ready work cannot overlap");
    shutdown.send_replace(true);
    task.await.unwrap();
}

struct Dropped(Arc<AtomicBool>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test(start_paused = true)]
async fn shutdown_and_timeout_drop_active_work_and_allow_later_retry() {
    let count = Arc::new(AtomicU64::new(0));
    let dropped = Arc::new(AtomicBool::new(false));
    let action: JobAction = {
        let count = Arc::clone(&count);
        let dropped = Arc::clone(&dropped);
        Arc::new(move || {
            let count = Arc::clone(&count);
            let guard = Dropped(Arc::clone(&dropped));
            Box::pin(async move {
                let _guard = guard;
                count.fetch_add(1, Ordering::SeqCst);
                std::future::pending().await
            })
        })
    };
    let (ready, receiver) = watch::channel(0);
    let (shutdown, stopped) = watch::channel(false);
    let task = tokio::spawn(maintenance_loop(
        "timeout",
        action,
        RECOVERY_CADENCE,
        RECOVERY_TIMEOUT,
        receiver,
        stopped,
    ));
    settle().await;
    ready.send_replace(1);
    settle().await;
    tokio::time::advance(RECOVERY_TIMEOUT).await;
    settle().await;
    assert!(dropped.swap(false, Ordering::SeqCst));
    assert_eq!(count.load(Ordering::SeqCst), 1);
    tokio::time::advance(RECOVERY_CADENCE - RECOVERY_TIMEOUT).await;
    settle().await;
    assert_eq!(count.load(Ordering::SeqCst), 2);
    shutdown.send_replace(true);
    task.await.unwrap();
    assert!(dropped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn gateway_scope_starts_once_joins_buttons_and_rejects_work_after_stop() {
    let mock = crate::discord_test_common::MockRest::start(
        vec![],
        crate::discord_test_common::ScriptedResponse::status(500),
    )
    .await;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@agent-testdb:5432/agent_test")
        .unwrap();
    let runtime = Arc::new(
        TicketRuntime::new(
            pool,
            ActionExecutor::with_proxy("ticket-test-token".into(), Some(mock.origin())).unwrap(),
            TicketConfig {
                guild_id: "100".into(),
                category_id: "200".into(),
                panel_channel_id: "700".into(),
                staff_role_id: "300".into(),
                cooldown_seconds: COOLDOWN_SECONDS,
            },
        )
        .unwrap(),
    );
    let supervisor = runtime.start().expect("first start");
    assert!(runtime.start().is_none());
    let dropped = Arc::new(AtomicBool::new(false));
    let guard = Dropped(Arc::clone(&dropped));
    let (started, receiver) = oneshot::channel();
    runtime.spawn(async move {
        let _guard = guard;
        started.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    receiver.await.unwrap();
    supervisor.shutdown().await;
    assert!(dropped.load(Ordering::SeqCst), "active button joined");
    let ran = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&ran);
    runtime.spawn(async move {
        flag.store(true, Ordering::SeqCst);
    });
    settle().await;
    assert!(!ran.load(Ordering::SeqCst));
    assert!(mock.requests().is_empty(), "no REST before Ready");
    mock.shutdown().await;
}
