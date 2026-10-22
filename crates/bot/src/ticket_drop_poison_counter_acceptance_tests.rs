//! Drop-only counted recovery for `TicketSupervisor::drop` + loud live `stop_tasks`.
//!
//! Contract under test (follow-up to #1530): a poisoned `TicketSupervisor::drop`
//! still aborts scoped tasks, fences `stopping` and notifies the shutdown watch
//! (same as `ticket_drop_poison_acceptance_tests.rs`), and additionally emits
//! exactly one `two_bot_drop_poison_total{guard="ticket_supervisor"}`. A healthy
//! drop leaves that series at zero delta. A poisoned live `stop_tasks` (the
//! loud variant Builder keeps under the original name; the drop-only variant
//! gets a new name and is called only from `Drop`) PANICS without counting and
//! without mutating (no `stopping` fence, no abort) — fail loud, not silent.
//!
//! RED on main (silent `into_inner`, no counter series): counter assertions
//! fail (series missing), live panic assertions fail (no panic, `stopping`
//! set). GREEN after Builder (drop-only `recover_drop_poison` + loud
//! `.expect`). Counter deltas use the global with a serializing lock — never
//! absolute counts (parallel tests share the global).
#![cfg(test)]

use super::*;
use crate::discord_test_common::{MockRest, ScriptedResponse};
use crate::drop_poison_test_serial::DROP_POISON_SERIAL;
use std::panic::{catch_unwind, AssertUnwindSafe};
use tokio::sync::oneshot;

const GUARD: &str = "ticket_supervisor";

/// Rendered `two_bot_drop_poison_total{guard="..."}` count, or `None` while
/// the series does not exist (pre-Builder: `DROP_POISON_GUARDS` lacks it).
fn drop_poison_count(guard: &str) -> Option<u64> {
    let rendered = two_bot_core::metrics::global().render(None);
    let prefix = format!("two_bot_drop_poison_total{{guard=\"{guard}\"}} ");
    rendered.lines().find_map(|line| {
        line.strip_prefix(prefix.as_str())?
            .trim()
            .parse::<u64>()
            .ok()
    })
}

struct Cancelled(Option<oneshot::Sender<()>>);

impl Drop for Cancelled {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

async fn offline_runtime() -> (Arc<TicketRuntime>, MockRest) {
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
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
    (runtime, mock)
}

async fn admit_two_scoped_tasks(runtime: &Arc<TicketRuntime>) -> Vec<oneshot::Receiver<()>> {
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
    cancellations
}

async fn supervisor_drop_with_counter(poisoned: bool, expected_delta: u64) {
    let (runtime, mock) = offline_runtime().await;
    let supervisor = runtime.start().expect("first supervisor");
    let shutdown = supervisor.shutdown.subscribe();
    let supervisor_task = supervisor.task.abort_handle();
    let cancellations = admit_two_scoped_tasks(&runtime).await;
    if poisoned {
        let holder = catch_unwind(AssertUnwindSafe(|| {
            let _held = runtime.tasks.lock().expect("healthy task scope");
            std::panic::panic_any("poison the ticket task scope");
        }));
        assert!(holder.is_err());
        assert!(runtime.tasks.is_poisoned());
    }

    // Serialise the counter window: parallel tests share the global, so an
    // unguarded delta could mix increments from another test's drop.
    let (outcome, before, after) = {
        let _serial = DROP_POISON_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let before = drop_poison_count(GUARD);
        let outcome = catch_unwind(AssertUnwindSafe(|| drop(supervisor)));
        let after = drop_poison_count(GUARD);
        (outcome, before, after)
    };
    let stopping = runtime.stopping.load(Ordering::Acquire);
    let notified = *shutdown.borrow();
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
    let (before, after) = (
        before.expect("poison counter series must render"),
        after.expect("poison counter series must render"),
    );
    assert_eq!(
        after.saturating_sub(before),
        expected_delta,
        "drop must emit exactly {expected_delta} {GUARD} count(s)"
    );
}

#[tokio::test]
async fn healthy_ticket_supervisor_drop_leaves_counter_at_zero_delta() {
    supervisor_drop_with_counter(false, 0).await;
}

#[tokio::test]
async fn poisoned_ticket_supervisor_drop_counts_exactly_one() {
    supervisor_drop_with_counter(true, 1).await;
}

#[tokio::test]
async fn poisoned_live_stop_tasks_panics_without_counter_or_mutation() {
    let (runtime, mock) = offline_runtime().await;
    // One scoped task proves loud failure aborts nothing (drop aborts both).
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
    let holder = catch_unwind(AssertUnwindSafe(|| {
        let _held = runtime.tasks.lock().expect("healthy task scope");
        std::panic::panic_any("poison the ticket task scope");
    }));
    assert!(holder.is_err());
    assert!(runtime.tasks.is_poisoned());

    // `shutdown(self)` would invoke `Drop` on panic (counted fallback), so the
    // loud contract is pinned directly on `stop_tasks`: panic, no count, no
    // `stopping` fence, no abort. Builder keeps the loud variant under this
    // original name; the drop-only variant gets a new name called only from
    // `Drop`.
    let (outcome, before, after) = {
        let _serial = DROP_POISON_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let before = drop_poison_count(GUARD).unwrap_or(0);
        let outcome = catch_unwind(AssertUnwindSafe(|| runtime.stop_tasks()));
        let after = drop_poison_count(GUARD).unwrap_or(0);
        (outcome, before, after)
    };
    let stopping = runtime.stopping.load(Ordering::Acquire);
    // Loud failure aborts nothing: the task must still be pending. A short
    // timeout proves no cancellation without waiting out the drop bound.
    let cancelled_yet = tokio::time::timeout(Duration::from_millis(100), cancellation).await;
    assert!(
        cancelled_yet.is_err(),
        "loud stop_tasks must not abort scoped tasks"
    );
    // Clean up the still-pending task; poison is sticky so recover to abort.
    runtime
        .tasks
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .abort_all();
    assert!(
        mock.requests().is_empty(),
        "no Discord request before READY"
    );
    mock.shutdown().await;

    assert!(
        outcome.is_err(),
        "poisoned live stop_tasks must panic (fail loud)"
    );
    assert_eq!(after, before, "loud failure must not emit {GUARD}");
    assert!(
        !stopping,
        "loud failure must not fence stopping before panicking"
    );
}
