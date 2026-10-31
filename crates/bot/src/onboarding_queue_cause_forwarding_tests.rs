//! Regression supplement: `onboarding_queue` forwards the real cause.
//!
//! The frozen worker-failure acceptance tests pin the literal each
//! cause renders, and only loosely that the queue mentions the
//! mapping. A loose mention still allows the queue to collapse or
//! mislabel a cause: discarding the runtime error, discarding the
//! forwarded worker cause, or naming the elapsed timeout after a
//! different variant. This non-frozen supplement pins the exact
//! whitespace-normalized statements that carry each cause, scoped to
//! `onboarding_queue` in `gateway.rs`, so each of those collapses
//! fails by name.
//!
//! Offline only: a source scan of `gateway.rs`. No database, no live
//! Discord.

/// The body of `onboarding_queue`, whitespace-normalized so exact
/// statement pins survive reformatting but not rewording.
fn normalized_queue() -> String {
    let source = include_str!("gateway.rs");
    let queue = source
        .split("async fn onboarding_queue")
        .nth(1)
        .expect("gateway.rs keeps onboarding_queue")
        .split("pub fn build_shard")
        .next()
        .expect("onboarding_queue precedes build_shard");
    queue.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The spawned-worker span inside `onboarding_queue`: from the
/// 90-second `tokio::time::timeout` that bounds the worker to the
/// typed `Ok` that hands its outcome back. Scoping the timeout and
/// runtime pins to this span ties them to that timeout call, not to
/// any other timeout in the queue.
fn normalized_worker_span(queue: &str) -> &str {
    let start = queue
        .find("tokio::time::timeout(std::time::Duration::from_secs(90),")
        .expect("onboarding_queue bounds the worker with a 90-second timeout");
    let rest = &queue[start..];
    let end = rest
        .find("Ok::<_, OnboardingWorkerFailure>")
        .expect("the spawned worker returns a typed OnboardingWorkerFailure outcome");
    &rest[..end]
}

/// The elapsed error from the 90-second timeout maps to `Timeout`,
/// and to nothing else: mislabeling it `Join` (or any other variant)
/// drops the exact statement pinned here.
#[test]
fn onboarding_queue_timeout_maps_elapsed_to_timeout_cause() {
    let queue = normalized_queue();
    let worker = normalized_worker_span(&queue);
    assert_eq!(
        worker
            .matches(".map_err(|_| OnboardingWorkerFailure::Timeout)?")
            .count(),
        1,
        "the 90-second timeout's elapsed error must map exactly once to \
         OnboardingWorkerFailure::Timeout, got {worker}"
    );
    assert!(
        !worker.contains(".map_err(|_| OnboardingWorkerFailure::Join)?"),
        "the 90-second timeout must not be mislabeled as a join failure, got {worker}"
    );
}

/// The runtime error is forwarded itself, via the `Runtime` variant
/// constructor as the `map_err` argument — never discarded by a
/// `|_|` closure and re-created as a fixed cause. The two-step chain
/// is pinned whole, in order: elapsed becomes `Timeout` first, then
/// the actual runtime error becomes `Runtime(error)`.
#[test]
fn onboarding_queue_forwards_the_actual_runtime_error() {
    let queue = normalized_queue();
    let worker = normalized_worker_span(&queue);
    assert_eq!(
        worker
            .matches(
                ".map_err(|_| OnboardingWorkerFailure::Timeout)? \
                 .map_err(OnboardingWorkerFailure::Runtime)?;"
            )
            .count(),
        1,
        "the worker must forward its runtime error through \
         OnboardingWorkerFailure::Runtime(error), got {worker}"
    );
}

/// A failed worker's cause is returned unchanged — the same `cause`
/// binding the match arm captured goes into `onboarding_worker_failure`
/// — while a join failure or panic (any other `join_next` outcome)
/// still yields `Join`. Collapsing the failed arm to `Join` via
/// `Err(_cause)` drops the exact statement pinned here.
#[test]
fn onboarding_queue_returns_the_same_worker_cause_and_join_for_panics() {
    let queue = normalized_queue();
    assert_eq!(
        queue
            .matches("Some(Ok(Err(cause))) => return Err(onboarding_worker_failure(cause)),")
            .count(),
        1,
        "a failed worker must return onboarding_worker_failure(cause) with \
         its own cause, got {queue}"
    );
    assert!(
        !queue.contains("Some(Ok(Err(_cause)))"),
        "the failed worker's cause must not be discarded, got {queue}"
    );
    assert_eq!(
        queue
            .matches("_ => return Err(onboarding_worker_failure(OnboardingWorkerFailure::Join)),")
            .count(),
        1,
        "a join failure or panic must return the Join cause, got {queue}"
    );
}
