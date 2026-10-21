//! Bounded retry for the sync store seam.
//!
//! The core seams are sync, so a transient DB error inside
//! `block_on_retry` cannot surface as a value the hot task could act on:
//! reads degrade to fail-closed defaults and writes panic. Retrying first
//! keeps a momentary blip (failover, pool churn, a dropped handshake) from
//! ever reaching that terminal step.
//!
//! Every error class retries: classifying sqlx errors at this seam risks
//! misclassifying a novel transient as permanent and failing a blip the
//! retry would have absorbed. A deterministic error just burns the same
//! bounded budget before its unchanged terminal, and every retried write
//! runs in a fresh transaction (a failed attempt's transaction rolls back
//! on drop), so retries never half-apply.

use std::time::Duration;

/// Backoff before each retry: a transient blip usually clears in tens of
/// milliseconds, so a persistent outage fails through after 75 ms of
/// backoff — cheap on the per-event hot path. Attempts per call are one
/// initial try plus one retry per entry here (three total).
const STORE_RETRY_BACKOFF_MS: [u64; 2] = [25, 50];

/// Run a fallible store future from a sync trait method, retrying with
/// backoff before surfacing the final error. `make` rebuilds the future per
/// attempt (a future is single-shot); callers map the returned error to a
/// fail-closed default or a panic carrying the site's message.
pub(crate) fn block_on_retry<F, Fut, T>(
    handle: &tokio::runtime::Handle,
    make: F,
) -> Result<T, sqlx::Error>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T, sqlx::Error>>,
{
    tokio::task::block_in_place(|| {
        handle.block_on(async {
            let mut error = match make().await {
                Ok(value) => return Ok(value),
                Err(error) => error,
            };
            for backoff_ms in STORE_RETRY_BACKOFF_MS {
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                match make().await {
                    Ok(value) => return Ok(value),
                    Err(failed) => error = failed,
                }
            }
            Err(error)
        })
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::block_on_retry;

    fn failed() -> Result<(), sqlx::Error> {
        Err(sqlx::Error::RowNotFound)
    }

    /// A healthy call runs once: no retry delay on the hot path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn healthy_call_runs_once() {
        let attempts = AtomicU32::new(0);
        let outcome = block_on_retry(&tokio::runtime::Handle::current(), || async {
            attempts.fetch_add(1, Ordering::Relaxed);
            Ok::<(), sqlx::Error>(())
        });
        assert!(outcome.is_ok());
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
    }

    /// A blip that clears recovers on the retry instead of surfacing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn transient_blip_recovers_on_retry() {
        let attempts = AtomicU32::new(0);
        let outcome = block_on_retry(&tokio::runtime::Handle::current(), || async {
            let attempt = attempts.fetch_add(1, Ordering::Relaxed);
            if attempt == 0 {
                failed()
            } else {
                Ok(())
            }
        });
        assert!(outcome.is_ok());
        assert_eq!(attempts.load(Ordering::Relaxed), 2);
    }

    /// A persistent failure exhausts one try plus one retry per backoff
    /// entry, then surfaces the last error to the caller.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn persistent_failure_exhausts_three_attempts() {
        let attempts = AtomicU32::new(0);
        let outcome = block_on_retry(&tokio::runtime::Handle::current(), || async {
            attempts.fetch_add(1, Ordering::Relaxed);
            failed()
        });
        assert!(matches!(outcome, Err(sqlx::Error::RowNotFound)));
        assert_eq!(attempts.load(Ordering::Relaxed), 3);
    }
}
