use std::{future::Future, sync::Arc, time::Duration};

pub(crate) async fn wall_clock_timeout<T>(
    duration: Duration,
    future: impl Future<Output = T>,
) -> Result<T, &'static str> {
    // Tokio cannot bound a future while its own clock is deliberately held.
    // Dropping the sender cancels the independent timer without unpausing Tokio.
    let (_cancel, cancelled) = std::sync::mpsc::channel::<()>();
    let (elapsed, alarm) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        if matches!(
            cancelled.recv_timeout(duration),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ) {
            let _ = elapsed.send(());
        }
    });
    tokio::select! {
        output = future => Ok(output),
        _ = alarm => Err("frozen-clock fixture exceeded wall-clock watchdog"),
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
        Self {
            _release: release,
            released,
        }
    }
}
