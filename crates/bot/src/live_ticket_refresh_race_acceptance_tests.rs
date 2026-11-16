//! Protected acceptance: two overlapping refreshes must not leave revision 2
//! carrying revision 1's cooldown. The baseline's existing tracing event is a
//! per-thread deterministic pause immediately before its stale config write.
//! No sleeps, database I/O, process-global subscriber or probabilistic stress.
//!
//! Builder seam (cfg(test)): TicketRuntime::try_refresh_config_with_for_test
//! (Option<&LiveSettings>) -> bool. It must try the SAME revision/config state
//! lock and common resolver as refresh_config_with, returning false immediately
//! when busy. The baseline adapter calls the actual split-lock refresh, where
//! B can complete while A is paused. A serialized implementation reports busy;
//! the test retries B only in that case, after A is released.

#![cfg(test)]

use super::*;
use std::io::Write;
use std::sync::{atomic::AtomicBool, mpsc, Mutex as StdMutex};
use two_bot_core::settings::LiveSettings;

trait BaselineTryRefresh {
    fn try_refresh_config_with_for_test(&self, live: Option<&LiveSettings>) -> bool;
}

impl BaselineTryRefresh for TicketRuntime {
    fn try_refresh_config_with_for_test(&self, live: Option<&LiveSettings>) -> bool {
        self.refresh_config_with(live);
        true
    }
}

#[derive(Clone)]
struct PauseOnApply {
    entered: mpsc::SyncSender<()>,
    release: Arc<StdMutex<mpsc::Receiver<()>>>,
    fired: Arc<AtomicBool>,
}

struct PauseWriter {
    pause: PauseOnApply,
    bytes: Vec<u8>,
}

impl Write for PauseWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.bytes.extend_from_slice(buf);
        if self.bytes.contains(&b'\n')
            && String::from_utf8_lossy(&self.bytes).contains("setting_changed: ticket config")
            && !self.pause.fired.swap(true, Ordering::SeqCst)
        {
            self.pause
                .entered
                .send(())
                .expect("test receives A's pause");
            self.pause
                .release
                .lock()
                .expect("release receiver")
                .recv_timeout(Duration::from_secs(10))
                .expect("test releases A");
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn snapshot(revision: i64, cooldown: u64) -> SettingsSnapshot {
    SettingsSnapshot {
        revision,
        rows: vec![SettingRow {
            guild_id: "100".into(),
            key: COOLDOWN_KEY.into(),
            value: json!(cooldown),
            version: revision,
        }],
    }
}

#[tokio::test]
async fn overlapping_refresh_cannot_pin_old_cooldown_at_the_new_revision() {
    let mock = MockRest::start(Vec::new(), ScriptedResponse::status(500)).await;
    let pool = PgPoolOptions::new()
        .connect_lazy(
            "postgres://agent_test:@agent-testdb:5432/two_bot_test_live_settings_acceptance",
        )
        .expect("lazy disposable-test-service pool");
    pool.close().await;
    let executor = ActionExecutor::with_proxy("fixture-token".into(), Some(mock.origin()))
        .expect("mock executor");
    let deployment = deployment_vars();
    let runtime = TicketRuntime::new(
        pool,
        executor,
        TicketConfig::from_vars(&deployment, 100, 120).expect("boot config"),
        deployment,
    )
    .expect("ticket runtime");
    let (mut writer, live) = live_channel();
    writer.publish(&snapshot(1, 60));
    let immutable_first = live.snapshot();

    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let pause = PauseOnApply {
        entered: entered_tx,
        release: Arc::new(StdMutex::new(release_rx)),
        fired: Arc::new(AtomicBool::new(false)),
    };
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || PauseWriter {
            pause: pause.clone(),
            bytes: Vec::new(),
        })
        .finish();

    let b_completed = std::thread::scope(|scope| {
        let a = scope.spawn(|| {
            tracing::subscriber::with_default(subscriber, || {
                runtime.refresh_config_with(Some(&live))
            });
        });
        let entered = entered_rx.recv_timeout(Duration::from_secs(5));
        // Always unblock/join the thread before asserting a failed hook, so a
        // missing event cannot strand a worker during test unwinding.
        if entered.is_err() {
            let _ = release_tx.send(());
            a.join().expect("A finishes without a hook");
            panic!("A did not reach the existing config-apply event");
        }
        writer.publish(&snapshot(2, 3600));
        assert_eq!(immutable_first.revision(), 1, "held snapshot is immutable");
        assert_eq!(immutable_first.get("100", COOLDOWN_KEY), Some(&json!(60)));
        assert_eq!(live.snapshot().revision(), 2);

        let (attempt_tx, attempt_rx) = mpsc::sync_channel(1);
        let runtime_ref = &runtime;
        let live_ref = &live;
        let b = scope.spawn(move || {
            let applied = runtime_ref.try_refresh_config_with_for_test(Some(live_ref));
            attempt_tx.send(applied).expect("test receives B's attempt");
        });
        let attempted = attempt_rx.recv_timeout(Duration::from_secs(5));
        release_tx
            .send(())
            .expect("release A after B's nonblocking attempt");
        a.join().expect("A finishes");
        b.join().expect("B finishes");
        attempted.expect("try-refresh must return without waiting for A")
    });

    // Never repair the baseline's false-success case: B said it published 2,
    // so A is forbidden to undo it. Only a genuinely busy B may retry.
    if !b_completed {
        runtime.refresh_config_with(Some(&live));
    }
    assert_eq!(
        runtime.config_for_test().cooldown_seconds,
        3600,
        "A must not overwrite B's newer cooldown after B reports success"
    );
    runtime.refresh_config_with(Some(&live));
    assert_eq!(
        runtime.config_for_test().cooldown_seconds,
        3600,
        "steady revision-2 reads must not permanently retain revision-1 config"
    );
    assert!(mock.requests().is_empty(), "refresh has no Discord effects");
    mock.shutdown().await;
}

#[tokio::test]
async fn explicit_baseline_try_adapter_preserves_the_normal_refresh_path() {
    let mock = MockRest::start(Vec::new(), ScriptedResponse::status(500)).await;
    let runtime = runtime(&mock);
    let (_writer, live) = live_channel();
    assert!(
        <TicketRuntime as BaselineTryRefresh>::try_refresh_config_with_for_test(
            &runtime,
            Some(&live)
        )
    );
    assert_eq!(runtime.config_for_test().cooldown_seconds, COOLDOWN_SECONDS);
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
}
