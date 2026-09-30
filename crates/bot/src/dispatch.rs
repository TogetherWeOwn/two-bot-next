//! One ordered blocking worker; reception must never wait for dispatch I/O.

use futures_util::{Stream, StreamExt};
use tokio::sync::mpsc;

pub const DISPATCH_BACKLOG: usize = 64;

/// A full backlog or dead worker is fatal, not permission to drop a dispatch.
/// The caller must terminate the process so its supervisor can restart it.
pub async fn dispatch_ordered<T, S, F>(
    stream: S,
    capacity: usize,
    mut handle: F,
) -> Result<(), &'static str>
where
    T: Send + 'static,
    S: Stream<Item = T>,
    F: FnMut(T) + Send + 'static,
{
    let (tx, mut rx) = mpsc::channel(capacity);
    let mut worker = tokio::task::spawn_blocking(move || {
        while let Some(event) = rx.blocking_recv() {
            handle(event);
        }
    });
    futures_util::pin_mut!(stream);
    loop {
        tokio::select! {
            result = &mut worker => {
                return match result {
                    Ok(()) => Err("dispatch worker ended unexpectedly"),
                    Err(_) => Err("dispatch worker failed"),
                };
            }
            item = stream.next() => {
                let Some(event) = item else { break };
                tx.try_send(event).map_err(|err| match err {
                    mpsc::error::TrySendError::Full(_) => "dispatch backlog full",
                    mpsc::error::TrySendError::Closed(_) => "dispatch worker unavailable",
                })?;
            }
        }
    }
    drop(tx);
    worker.await.map_err(|_| "dispatch worker failed")
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
    async fn overload_is_fatal_not_backpressure_on_reception() {
        let (release, wait) = std::sync::mpsc::channel();
        let result = dispatch_ordered(futures_util::stream::iter(0..128), 1, move |_| {
            let _ = wait.recv_timeout(Duration::from_secs(2));
        })
        .await;
        let _ = release.send(());
        assert_eq!(result, Err("dispatch backlog full"));
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
