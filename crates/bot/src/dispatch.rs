//! One ordered blocking worker; reception must never wait for dispatch I/O.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use futures_util::{Stream, StreamExt};
use tokio::sync::{mpsc, watch};

pub const DISPATCH_BACKLOG: usize = 64;
// One REST read (10s) + checkpoint (<=5s), with scheduling headroom.
pub const DISPATCH_IO_MAX: Duration = Duration::from_secs(20);
pub const DISPATCH_DRAIN_MAX: Duration = Duration::from_secs(30);

#[cfg(test)]
pub async fn dispatch_ordered<T, S, F>(
    stream: S,
    capacity: usize,
    handle: F,
) -> Result<(), &'static str>
where
    T: Send + 'static,
    S: Stream<Item = T>,
    F: FnMut(T) + Send + 'static,
{
    dispatch_bounded(
        stream,
        capacity,
        handle,
        || async {},
        DISPATCH_IO_MAX,
        DISPATCH_DRAIN_MAX,
    )
    .await
}

/// Stop reception/readiness before draining a retained overflow tail. Both an
/// individual handler and the entire drain (including tail enqueue) are bounded.
/// Timeout is fatal: spawn_blocking cannot cancel running code. The essential
/// task supervisor MUST exit the process, not await runtime shutdown or reconnect
/// alongside an old writer. A durable checkpoint replays uncommitted dispatches.
pub async fn dispatch_bounded<T, S, F, Stop, Stopped>(
    stream: S,
    capacity: usize,
    mut handle: F,
    on_stop: Stop,
    io_max: Duration,
    drain_max: Duration,
) -> Result<(), &'static str>
where
    T: Send + 'static,
    S: Stream<Item = T>,
    F: FnMut(T) + Send + 'static,
    Stop: FnOnce() -> Stopped,
    Stopped: std::future::Future<Output = ()>,
{
    let (tx, mut rx) = mpsc::channel(capacity);
    let (progress, mut deadline) = watch::channel(None);
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = Arc::clone(&cancelled);
    let mut worker = tokio::task::spawn_blocking(move || {
        while let Some(event) = rx.blocking_recv() {
            if worker_cancelled.load(Ordering::Acquire) {
                break;
            }
            progress.send_replace(Some(tokio::time::Instant::now() + io_max));
            handle(event);
            progress.send_replace(None);
        }
    });
    futures_util::pin_mut!(stream);
    let mut tail = None;
    let result = loop {
        let active_deadline = *deadline.borrow_and_update();
        let stalled = async {
            match active_deadline {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            result = &mut worker => {
                on_stop().await;
                return match result {
                    Ok(()) => Err("dispatch worker ended unexpectedly"),
                    Err(_) => Err("dispatch worker failed"),
                };
            }
            _ = stalled => { break Err("dispatch I/O deadline exceeded"); }
            _ = deadline.changed() => {}
            item = stream.next() => {
                let Some(event) = item else { break Ok(()) };
                match tx.try_send(event) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(event)) => {
                        tail = Some(event);
                        break Err("dispatch backlog full");
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        break Err("dispatch worker unavailable");
                    }
                }
            }
        }
    };
    on_stop().await;
    if result == Err("dispatch I/O deadline exceeded") {
        cancelled.store(true, Ordering::Release);
        return result;
    }
    let drain = async {
        if let Some(event) = tail {
            tx.send(event)
                .await
                .map_err(|_| "dispatch worker unavailable")?;
        }
        drop(tx);
        worker.await.map_err(|_| "dispatch worker failed")?;
        result
    };
    futures_util::pin_mut!(drain);
    let drain_deadline = tokio::time::Instant::now() + drain_max;
    loop {
        let active_deadline = *deadline.borrow_and_update();
        let next_deadline = active_deadline.map_or(drain_deadline, |at| at.min(drain_deadline));
        tokio::select! {
            biased;
            result = &mut drain => return result,
            _ = tokio::time::sleep_until(next_deadline) => {
                cancelled.store(true, Ordering::Release);
                return Err(if next_deadline == drain_deadline {
                    "dispatch drain deadline exceeded"
                } else {
                    "dispatch I/O deadline exceeded"
                });
            }
            _ = deadline.changed() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slow_dispatch_does_not_stop_receiver_heartbeat_polling_and_preserves_order() {
        let (release, wait) = std::sync::mpsc::channel();
        let (started, start) = tokio::sync::oneshot::channel();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let rows = Arc::clone(&observed);
        let mut started = Some(started);
        // This stream stands in for a shard whose heartbeat is driven by
        // polling: it cannot release the slow handler unless polled again.
        let stream = futures_util::stream::unfold((0, Some(start)), move |(n, mut start)| {
            let release = release.clone();
            async move {
                if n == 4 {
                    return None;
                }
                if n == 1 {
                    start.take().unwrap().await.unwrap();
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
                if n == 3 {
                    release.send(()).unwrap();
                }
                Some((n, (n + 1, start)))
            }
        });
        let task = tokio::spawn(dispatch_ordered(stream, 8, move |n| {
            if n == 0 {
                started.take().unwrap().send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(2))
                    .expect("receiver stopped polling");
            }
            rows.lock().unwrap().push(n);
        }));
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap(),
            Ok(())
        );
        assert_eq!(*observed.lock().unwrap(), vec![0, 1, 2, 3]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn overload_drains_accepted_and_overflow_dispatches_before_fatal_return() {
        let (release, wait) = std::sync::mpsc::channel();
        let (started, start) = tokio::sync::oneshot::channel();
        let (overflow, received) = tokio::sync::oneshot::channel();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let rows = Arc::clone(&observed);
        let stream = futures_util::stream::unfold(
            (0, Some(start), Some(overflow)),
            |(n, mut start, mut overflow)| async move {
                if n == 1 {
                    start.take().unwrap().await.unwrap();
                }
                if n == 2 {
                    overflow.take().unwrap().send(()).unwrap();
                }
                assert!(n < 3, "reception must stop at overflow");
                Some((n, (n + 1, start, overflow)))
            },
        );
        let mut started = Some(started);
        let task = tokio::spawn(dispatch_ordered(stream, 1, move |n| {
            if n == 0 {
                started.take().unwrap().send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(2)).unwrap();
            }
            rows.lock().unwrap().push(n);
        }));
        received.await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let exited_before_commit = task.is_finished();
        release.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert!(!exited_before_commit, "fatal return detached pending work");
        assert_eq!(result, Err("dispatch backlog full"));
        assert_eq!(*observed.lock().unwrap(), vec![0, 1, 2]);
    }

    // No handler-side timer: release only AFTER fatal recovery has completed.
    // Otherwise the fixture itself could hide an unbounded dispatcher.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn never_completing_handler_exits_essential_supervisor_without_overflow() {
        let (release, wait) = std::sync::mpsc::channel();
        let (started, start) = tokio::sync::oneshot::channel();
        let (stopped, mut stop) = tokio::sync::oneshot::channel();
        let stream = futures_util::stream::once(async { 1 }).chain(futures_util::stream::pending());
        let mut started = Some(started);
        let worker = tokio::spawn(async move {
            dispatch_bounded(
                stream,
                8,
                move |_| {
                    started.take().unwrap().send(()).unwrap();
                    wait.recv().unwrap();
                },
                || async move {
                    stopped.send(()).unwrap();
                },
                Duration::from_millis(40),
                Duration::from_secs(2),
            )
            .await
            .map_err(|reason| sqlx::Error::InvalidArgument(reason.into()))
        });
        start.await.unwrap();
        let state = Arc::new(tokio::sync::RwLock::new(
            crate::gateway::GatewayState::Connected,
        ));
        let (shutdown, stopping) = watch::channel(false);
        let (http_stopped, mut http_stop) = tokio::sync::oneshot::channel();
        let http = async move {
            crate::server::shutdown_requested(stopping).await;
            http_stopped.send(()).unwrap();
            Ok(())
        };
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            crate::supervise_gateway(worker, http, Arc::clone(&state), shutdown),
        )
        .await;
        let stopped = stop.try_recv();
        let http_stopped = http_stop.try_recv();
        let final_state = *state.read().await;
        release.send(()).unwrap();
        assert!(stopped.is_ok(), "readiness stop must precede fatal return");
        assert!(
            http_stopped.is_ok(),
            "HTTP cleanup must observe shutdown before fatal return"
        );
        assert_eq!(final_state, crate::gateway::GatewayState::Draining);
        assert_eq!(
            result.unwrap().unwrap_err().to_string(),
            "gateway task stopped; container restart required",
            "supervisor must terminate without releasing the handler"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn overflow_tail_enqueue_has_total_deadline_and_immediate_readiness_loss() {
        let state = Arc::new(tokio::sync::RwLock::new(
            crate::gateway::GatewayState::Connected,
        ));
        let stop_state = Arc::clone(&state);
        let (release, wait) = std::sync::mpsc::channel();
        let (started, start) = tokio::sync::oneshot::channel();
        let (stopped, mut stop) = tokio::sync::oneshot::channel();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let rows = Arc::clone(&observed);
        let stream = futures_util::stream::unfold((0, Some(start)), |(n, mut start)| async move {
            if n == 1 {
                start.take().unwrap().await.unwrap();
            }
            assert!(n < 3, "reception must stop after the overflow tail");
            Some((n, (n + 1, start)))
        });
        let mut started = Some(started);
        let mut task = tokio::spawn(dispatch_bounded(
            stream,
            1,
            move |n| {
                if n == 0 {
                    started.take().unwrap().send(()).unwrap();
                    wait.recv().unwrap();
                }
                rows.lock().unwrap().push(n);
            },
            || async move {
                *stop_state.write().await = crate::gateway::GatewayState::Draining;
                stopped.send(()).unwrap();
            },
            Duration::from_secs(2),
            Duration::from_millis(100),
        ));
        tokio::time::timeout(Duration::from_secs(1), &mut stop)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*state.read().await, crate::gateway::GatewayState::Draining);
        assert_ne!(
            state.read().await.status(),
            two_bot_core::ComponentStatus::Ready
        );
        assert!(!task.is_finished(), "stop notification must precede drain");
        assert!(observed.lock().unwrap().is_empty());
        let result = tokio::time::timeout(Duration::from_secs(1), &mut task).await;
        // Always release before asserting, even if the tested timeout regresses.
        release.send(()).unwrap();
        assert_eq!(
            result.unwrap().unwrap(),
            Err("dispatch drain deadline exceeded")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn finite_stream_join_is_bounded_even_without_an_overflow_tail() {
        let (release, wait) = std::sync::mpsc::channel();
        let (started, start) = tokio::sync::oneshot::channel();
        let stream =
            futures_util::stream::unfold((false, Some(start)), |(sent, mut start)| async move {
                if sent {
                    start.take().unwrap().await.unwrap();
                    None
                } else {
                    Some((1, (true, start)))
                }
            });
        let mut started = Some(started);
        let task = tokio::spawn(dispatch_bounded(
            stream,
            8,
            move |_| {
                started.take().unwrap().send(()).unwrap();
                wait.recv().unwrap();
            },
            || async {},
            Duration::from_secs(2),
            Duration::from_millis(40),
        ));
        let result = tokio::time::timeout(Duration::from_secs(1), task).await;
        release.send(()).unwrap();
        assert_eq!(
            result.unwrap().unwrap(),
            Err("dispatch drain deadline exceeded")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn worker_panic_is_fatal_even_when_no_more_events_arrive() {
        let stream = futures_util::stream::once(async { 1 }).chain(futures_util::stream::pending());
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            dispatch_ordered(stream, 1, |_| panic!("fixture failure")),
        )
        .await
        .unwrap();
        assert_eq!(result, Err("dispatch worker failed"));
    }
}
