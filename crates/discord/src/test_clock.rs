use std::{
    future::Future,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

/// Shared liveness signal for frozen-clock fixtures. Every completed fixture
/// step (an advanced interval, a committed admission, a recorded mock request,
/// a fixture handshake) bumps it. A starved runner merely delays marks; a real
/// frozen-clock deadlock stops them entirely, which is what the watchdog can
/// detect without confusing slow progress for a hang.
static FIXTURE_PROGRESS: AtomicU64 = AtomicU64::new(0);

/// Wall-clock allowance between two consecutive fixture-progress marks. The
/// watchdog detects deadlocks, not speed: a single request or barrier on a
/// pinned, oversubscribed CPU can wait seconds for a slice, so this sits far
/// above one step's worst scheduling delay and far below the job timeout it
/// protects a hung fixture from.
pub(crate) const STALL_GRACE: Duration = Duration::from_secs(30);

pub(crate) fn mark_progress() {
    FIXTURE_PROGRESS.fetch_add(1, Ordering::Relaxed);
}

/// `tokio::time::advance` plus a progress mark: virtual-time steps are the
/// fixture's own forward motion and count toward liveness.
pub(crate) async fn advance(duration: Duration) {
    tokio::time::advance(duration).await;
    mark_progress();
}

/// Resolve `future`, or fail when no fixture progress is observed for `stall`
/// wall-clock time. The shared counter means progress by a concurrently running
/// fixture can only delay detection of a real hang, never fail a healthy test.
pub(crate) async fn stall_watchdog<T>(
    stall: Duration,
    future: impl Future<Output = T>,
) -> Result<T, &'static str> {
    stall_watchdog_on(&FIXTURE_PROGRESS, stall, future).await
}

/// Same liveness check against a caller-owned counter. Isolation lets a test
/// prove the watchdog fires even while other fixtures keep marking progress.
pub(crate) async fn stall_watchdog_on<T>(
    progress: &'static AtomicU64,
    stall: Duration,
    future: impl Future<Output = T>,
) -> Result<T, &'static str> {
    // Tokio cannot bound a future while its own clock is deliberately held, so
    // an independent thread watches the counter between wall-clock windows.
    // Dropping the sender cancels the watcher without unpausing Tokio.
    let (_cancel, cancelled) = std::sync::mpsc::channel::<()>();
    let (elapsed, alarm) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut observed = progress.load(Ordering::Relaxed);
        loop {
            match cancelled.recv_timeout(stall) {
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    let now = progress.load(Ordering::Relaxed);
                    if now == observed {
                        let _ = elapsed.send(());
                        return;
                    }
                    observed = now;
                }
                _ => return,
            }
        }
    });
    tokio::select! {
        output = future => Ok(output),
        _ = alarm => Err("frozen-clock fixture made no progress before the watchdog"),
    }
}

pub(crate) struct ClockHold {
    _release: std::sync::mpsc::Sender<()>,
    pub released: Arc<tokio::sync::Notify>,
}

impl ClockHold {
    pub async fn start() -> Self {
        // A live blocking task inhibits paused Tokio's idle auto-advance while
        // real loopback I/O runs. Cancellation releases it; no busy-spin task.
        let (release, held) = std::sync::mpsc::channel();
        let (started, ready) = tokio::sync::oneshot::channel();
        let released = Arc::new(tokio::sync::Notify::new());
        let finished = released.clone();
        tokio::task::spawn_blocking(move || {
            let _ = started.send(());
            let _ = held.recv();
            finished.notify_one();
        });
        ready.await.unwrap();
        mark_progress();
        Self {
            _release: release,
            released,
        }
    }
}
