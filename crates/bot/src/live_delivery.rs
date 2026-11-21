//! Bounded, ordered, best-effort delivery for normalized site deltas.

use std::time::Duration;
use tokio::{
    sync::mpsc,
    time::{sleep, timeout_at, Instant},
};

const BUFFER: usize = 128;
const ATTEMPT_TIMEOUT: Duration = Duration::from_millis(500);
const RETRY_DELAY: Duration = Duration::from_millis(250);
const MAX_ATTEMPTS: usize = 3;
const MAX_AGE: Duration = Duration::from_secs(10);

struct Queued<T> {
    delta: T,
    expires_at: Instant,
}

/// The attempt's disposition. A terminal refusal is not retried.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Accepted,
    Retry,
    Rejected,
}

/// Gateway-side handle. Enqueue only normalized, size-bounded deltas, numbered
/// before submission so a dropped item is detectable as a gap at the site.
pub struct Sender<T>(mpsc::Sender<Queued<T>>);

/// Worker-side handle. One delta may be in flight outside the bounded buffer.
pub struct Receiver<T>(mpsc::Receiver<Queued<T>>);

pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let (sender, receiver) = mpsc::channel(BUFFER);
    (Sender(sender), Receiver(receiver))
}

impl<T> Sender<T> {
    /// Never waits on the worker. Returns false if full or closed.
    pub fn offer(&self, delta: T) -> bool {
        self.0
            .try_send(Queued {
                delta,
                expires_at: Instant::now() + MAX_AGE,
            })
            .is_ok()
    }
}

impl<T> Receiver<T> {
    /// Sends serially, keeping the same delta (and sequence) on retry.
    /// The caller owns the worker future and may cancel it on shutdown.
    pub async fn run(mut self, mut attempt: impl AsyncFnMut(&T) -> Delivery) {
        while let Some(item) = self.0.recv().await {
            for index in 0..MAX_ATTEMPTS {
                let now = Instant::now();
                if now >= item.expires_at {
                    break;
                }
                let deadline = (now + ATTEMPT_TIMEOUT).min(item.expires_at);
                match timeout_at(deadline, attempt(&item.delta)).await {
                    Ok(Delivery::Accepted | Delivery::Rejected) => break,
                    Ok(Delivery::Retry) | Err(_) if index + 1 < MAX_ATTEMPTS => {
                        if Instant::now() + RETRY_DELAY >= item.expires_at {
                            break;
                        }
                        sleep(RETRY_DELAY).await;
                    }
                    _ => break,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_full_queue_drops_the_new_delta_without_reordering_the_rest() {
        let (sender, receiver) = channel();
        for seq in 1..=BUFFER {
            assert!(sender.offer(seq));
        }
        assert!(!sender.offer(BUFFER + 1));
        drop(sender);
        let mut received = Vec::new();
        receiver
            .run(async |&delta| {
                received.push(delta);
                Delivery::Accepted
            })
            .await;
        assert_eq!(received, (1..=BUFFER).collect::<Vec<_>>());
    }

    #[tokio::test(start_paused = true)]
    async fn retry_and_timeout_keep_the_same_sequence_before_progressing() {
        let (sender, receiver) = channel();
        for seq in 1..=3 {
            assert!(sender.offer(seq));
        }
        drop(sender);
        let start = Instant::now();
        let mut calls = Vec::new();
        receiver
            .run(async |&seq| {
                calls.push(seq);
                match (seq, calls.len()) {
                    (1, 1) => std::future::pending().await,
                    (1, _) => Delivery::Retry,
                    (2, _) => Delivery::Rejected,
                    _ => Delivery::Accepted,
                }
            })
            .await;
        assert_eq!(calls, [1, 1, 1, 2, 3]);
        let expected = ATTEMPT_TIMEOUT + RETRY_DELAY * 2;
        assert!(start.elapsed() >= expected);
        assert!(start.elapsed() <= expected + Duration::from_millis(50));
    }

    #[tokio::test(start_paused = true)]
    async fn expired_deltas_are_not_sent_and_closed_workers_refuse_offers() {
        let (sender, receiver) = channel();
        assert!(sender.offer(1));
        tokio::time::advance(MAX_AGE).await;
        assert!(sender.offer(2));
        let mut received = Vec::new();
        drop(sender);
        receiver
            .run(async |&seq| {
                received.push(seq);
                Delivery::Accepted
            })
            .await;
        assert_eq!(received, [2]);

        let (sender, receiver) = channel();
        drop(receiver);
        assert!(!sender.offer(3));
    }

    #[tokio::test(start_paused = true)]
    async fn in_flight_attempt_stops_at_the_queue_age_deadline() {
        let (sender, receiver) = channel();
        assert!(sender.offer(1));
        let remaining = Duration::from_millis(100);
        tokio::time::advance(MAX_AGE - remaining).await;
        assert!(sender.offer(2));
        drop(sender);
        let start = Instant::now();
        let mut attempts = Vec::new();
        receiver
            .run(async |&seq| {
                attempts.push((seq, start.elapsed()));
                if seq == 1 {
                    std::future::pending().await
                } else {
                    Delivery::Accepted
                }
            })
            .await;
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0], (1, Duration::ZERO));
        assert_eq!(attempts[1].0, 2);
        assert!((remaining..ATTEMPT_TIMEOUT).contains(&attempts[1].1));
    }

    #[tokio::test(start_paused = true)]
    async fn retry_does_not_wait_past_the_expiry() {
        let (sender, receiver) = channel();
        assert!(sender.offer(1));
        drop(sender);
        tokio::time::advance(MAX_AGE - RETRY_DELAY / 2).await;
        let start = Instant::now();
        let mut calls = 0;
        receiver
            .run(async |_| {
                calls += 1;
                Delivery::Retry
            })
            .await;
        assert_eq!(calls, 1);
        assert_eq!(start.elapsed(), Duration::ZERO);
    }
}
