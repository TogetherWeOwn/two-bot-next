#![cfg(test)]
//! Protected acceptance: healthy recovery cannot erase committed readiness.
//! Held locks order the real queue and checkpoint IO; elapsed sleeps never
//! choose the interleaving. The fixture owns a guarded disposable database.

use super::*;

const READINESS_BUDGET: Duration = Duration::from_secs(5);
// One combined schedule budget, not one per wait: release every held IO
// before the unchanged five-second client and SQL statement deadlines.
const HELD_IO_BUDGET: Duration = Duration::from_secs(3);

async fn wait_blocked(case: &Case, blocker: i32, query_prefix: &str) {
    let mut poll = tokio::time::interval(Duration::from_millis(10));
    loop {
        poll.tick().await;
        let blocked: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_stat_activity
             WHERE datname = current_database() AND pid <> pg_backend_pid()
             AND $1 = ANY(pg_blocking_pids(pid)) AND query LIKE $2)",
        )
        .bind(blocker)
        .bind(format!("{query_prefix}%"))
        .fetch_one(case.pool())
        .await
        .expect("observe only this disposable database's blocked IO");
        if blocked {
            return;
        }
    }
}

async fn wait_ready(case: &Case, runner: &Runner) {
    let shared = crate::server::SharedState::new(runner.state.clone(), Some(case.pool().clone()));
    let jobs = crate::jobs::statuses(&[], true);
    tokio::time::timeout(READINESS_BUDGET, async {
        let mut poll = tokio::time::interval(Duration::from_millis(10));
        loop {
            poll.tick().await;
            assert!(
                !runner.task.is_finished(),
                "healthy IO must not stop the shard"
            );
            // Exercise the real /readyz computation, including its database
            // ping; isolate token state from unrelated process-global tests.
            let (code, report) = crate::server::readiness(&shared, &jobs, false).await;
            if code == axum::http::StatusCode::OK {
                assert!(report.health.components.iter().any(|(name, status)| {
                    name == "gateway" && *status == two_bot_core::ComponentStatus::Ready
                }));
                return;
            }
        }
    })
    .await
    .expect("healthy completed queue IO must not leave /readyz's gateway status starting");
}

async fn recovery_finishes_after_ready(resume: bool) {
    let case = Case::new().await;
    let (mock, _) = recipient_discord(Evidence::Absent).await;
    let locks = case.db.independent_pool().await.expect("owned lock pool");
    let result = bounded(async {
        let (mut ws, connected) = controlled_gateway().await;
        seed_opening(&case, &ws.mock.url, resume).await;
        interrupted(&case).await;
        let runner = spawn(&case, &mock, &ws.mock.url).await;
        connected.await.expect("connected local gateway");

        // Independent connections avoid pool saturation as an accidental gate.
        // SHARE blocks recovery's UPDATE, but not a job-free READY checkpoint.
        let mut recovery_lock = locks.begin().await.unwrap();
        let recovery_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *recovery_lock)
            .await
            .unwrap();
        sqlx::query("LOCK TABLE gateway_onboarding_jobs IN SHARE MODE")
            .execute(&mut *recovery_lock)
            .await
            .unwrap();
        let mut checkpoint_lock = locks.begin().await.unwrap();
        let checkpoint_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *checkpoint_lock)
            .await
            .unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("gateway:{GUILD}:0"))
            .execute(&mut *checkpoint_lock)
            .await
            .unwrap();

        ws.send(json!({"op":10,"d":{"heartbeat_interval":45000}}))
            .await;
        let auth = ws.mock.authentication().await;
        assert_eq!(auth["op"], if resume { 6 } else { 2 });
        if resume {
            assert_eq!(auth["d"]["session_id"], SESSION);
            assert_eq!(auth["d"]["seq"], 2);
        }
        assert_eq!(*runner.state.read().await, GatewayState::Armed);
        assert!(
            mock.requests().is_empty(),
            "authentication alone cannot release recovery"
        );
        let sequence = if resume { 3 } else { 1 };
        tokio::time::timeout(HELD_IO_BUDGET, async {
            ws.send(if resume {
                resumed(sequence)
            } else {
                ready(&ws.mock.url, SESSION)
            })
            .await;

            // Both helpers entered while Armed. The database, not a timer,
            // proves recovery started before the writer can promote READY.
            // Match the boot recovery UPDATE, not the later claim UPDATE.
            wait_blocked(
                &case,
                recovery_pid,
                "UPDATE gateway_onboarding_jobs SET\n             state = CASE",
            )
            .await;
            wait_blocked(&case, checkpoint_pid, "SELECT pg_advisory_xact_lock").await;
            assert_eq!(*runner.state.read().await, GatewayState::Armed);
            assert!(mock.requests().is_empty());
            checkpoint_lock.commit().await.unwrap();
            wait_sequence(&case.store(), sequence).await;
            wait_ready(&case, &runner).await;
            assert_eq!(
                row(&case, 2).await.0,
                "running",
                "recovery is still held after READY promotion"
            );
            assert!(mock.requests().is_empty());

            // Recovery captured Armed, but finishes after committed READY.
            recovery_lock.commit().await.unwrap();
        })
        .await
        .expect("establish and release the schedule before the unchanged IO deadlines");
        // Wait for actual recovered work to finish, not a transient ready poll.
        completed(&case, 2, 2).await;
        case.assert_unsent(&mock).await;
        assert!(mock
            .requests()
            .iter()
            .any(|request| request.method == "GET" && request.path == RECIPIENT_PATH));
        assert_eq!(
            case.store().load().await.unwrap().unwrap().sequence,
            sequence
        );
        wait_ready(&case, &runner).await;
        runner.stop().await;
    })
    .await;
    tokio::time::timeout(DEADLINE, locks.close())
        .await
        .expect("bounded owned lock pool teardown");
    tokio::time::timeout(DEADLINE, finish(case, mock, result))
        .await
        .expect("bounded owned fixture teardown");
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_gateway_recovery_finishing_after_ready_preserves_readiness() {
    recovery_finishes_after_ready(false).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_gateway_recovery_finishing_after_resumed_preserves_readiness() {
    recovery_finishes_after_ready(true).await;
}
