//! Supervisor cancellation must reach its task scope even after holder panic.
#![cfg(test)]

use super::*;
use crate::discord_test_common::{MockRest, ScriptedResponse};
use crate::drop_poison_test_serial::DROP_POISON_SERIAL;
use std::panic::{catch_unwind, AssertUnwindSafe};
use tokio::sync::oneshot;

struct Cancelled(Option<oneshot::Sender<()>>);

impl Drop for Cancelled {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

async fn supervisor_drop(poisoned: bool) {
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    // No READY is delivered: maintenance never queries this lazy test pool.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://agent_test:@agent-testdb:5432/two_bot_test_local")
        .expect("lazy disposable test pool");
    let runtime = Arc::new(
        TicketRuntime::new(
            pool,
            ActionExecutor::with_proxy("ticket-test-token".into(), Some(mock.origin()))
                .expect("loopback executor"),
            TicketConfig {
                guild_id: "100".into(),
                category_id: "200".into(),
                panel_channel_id: "700".into(),
                staff_role_id: "300".into(),
                cooldown_seconds: COOLDOWN_SECONDS,
            },
            HashMap::new(),
        )
        .expect("ticket runtime"),
    );
    let supervisor = runtime.start().expect("first supervisor");
    let shutdown = supervisor.shutdown.subscribe();
    let supervisor_task = supervisor.task.abort_handle();
    let mut cancellations = Vec::new();
    for _ in 0..2 {
        let (started, entered) = oneshot::channel();
        let (cancelled, cancellation) = oneshot::channel();
        runtime.spawn(async move {
            let _cancelled = Cancelled(Some(cancelled));
            let _ = started.send(());
            std::future::pending::<()>().await;
        });
        tokio::time::timeout(Duration::from_secs(5), entered)
            .await
            .expect("ticket task starts within bound")
            .expect("ticket task started");
        cancellations.push(cancellation);
    }
    if poisoned {
        let holder = catch_unwind(AssertUnwindSafe(|| {
            let _held = runtime.tasks.lock().expect("healthy task scope");
            std::panic::panic_any("poison the ticket task scope");
        }));
        assert!(holder.is_err());
        assert!(runtime.tasks.is_poisoned());
    }

    // The poison counter series is process-global: hold the shared lock so
    // this drop cannot land inside a counter test's delta window.
    let outcome = {
        let _serial = DROP_POISON_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        catch_unwind(AssertUnwindSafe(|| drop(supervisor)))
    };
    let stopping = runtime.stopping.load(Ordering::Acquire);
    let notified = *shutdown.borrow();
    // Unfixed code panics before aborting anything. Clean up only after saving
    // the observations; never strand pending tasks when the RED assertion fires.
    if outcome.is_err() {
        runtime
            .tasks
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .abort_all();
        supervisor_task.abort();
    }
    for cancellation in cancellations {
        tokio::time::timeout(Duration::from_secs(5), cancellation)
            .await
            .expect("scoped task is cancelled within bound")
            .expect("scoped task future was dropped");
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while !supervisor_task.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("supervisor task terminates within bound");
    assert!(
        mock.requests().is_empty(),
        "no Discord request before READY"
    );
    mock.shutdown().await;

    assert!(outcome.is_ok(), "TicketSupervisor::drop must not panic");
    assert!(stopping, "drop fences off further ticket tasks");
    assert!(notified, "drop sends the shutdown watch notification");
}

#[tokio::test]
async fn healthy_ticket_supervisor_drop_aborts_tasks_and_notifies_shutdown() {
    supervisor_drop(false).await;
}

#[tokio::test]
async fn poisoned_ticket_supervisor_drop_aborts_tasks_and_notifies_shutdown() {
    supervisor_drop(true).await;
}
