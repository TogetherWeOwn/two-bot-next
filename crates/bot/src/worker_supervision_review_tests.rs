#![cfg(test)]
//! Builder pins for the review round on the supervised-worker core: the
//! supervisor-abort guard, the latched containment drain floor, and the
//! `main` supervisor retention. The protected acceptance file pins the worker
//! drain contract; these tests pin the supervisor paths no protected test
//! reaches, offline only.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use tokio::sync::watch;

use crate::raid_runtime::join_on_shutdown;

const MAIN_SOURCE: &str = include_str!("main.rs");
const CONTAINMENT_SOURCE: &str = include_str!("containment_runtime.rs");

/// Strip comments outside string/char literals so structural pins match code,
/// not prose. Handles `//`, `/* */`, `"..."` and `'x'`; the scanned files
/// hold no raw strings. A future construct the scanner mishandles eats code
/// and fails the pin loudly rather than masking a deletion.
fn code_only(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut code = bytes.to_vec();
    let mut cursor = 0;
    let mut in_string = false;
    let mut in_char = false;
    let mut in_line_comment = false;
    let mut block_depth = 0;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        let next = bytes.get(cursor + 1).copied();
        if in_line_comment {
            if byte == b'\n' {
                in_line_comment = false;
            } else {
                code[cursor] = b' ';
            }
            cursor += 1;
            continue;
        }
        if block_depth > 0 {
            if byte == b'/' && next == Some(b'*') {
                block_depth += 1;
                code[cursor] = b' ';
                code[cursor + 1] = b' ';
                cursor += 2;
            } else if byte == b'*' && next == Some(b'/') {
                block_depth -= 1;
                code[cursor] = b' ';
                code[cursor + 1] = b' ';
                cursor += 2;
            } else {
                if !matches!(byte, b'\n' | b'\r') {
                    code[cursor] = b' ';
                }
                cursor += 1;
            }
            continue;
        }
        if in_string {
            if byte == b'\\' {
                cursor = (cursor + 2).min(bytes.len());
            } else {
                if byte == b'"' {
                    in_string = false;
                }
                cursor += 1;
            }
            continue;
        }
        if in_char {
            if byte == b'\\' {
                cursor = (cursor + 2).min(bytes.len());
            } else {
                if byte == b'\'' {
                    in_char = false;
                }
                cursor += 1;
            }
            continue;
        }
        // A quote opens a char literal only for `'x'`/escapes; lifetimes such
        // as `'static` stay code.
        if byte == b'\'' {
            let closes = (cursor + 2 < bytes.len() && bytes[cursor + 2] == b'\'')
                || (cursor + 3 < bytes.len()
                    && bytes[cursor + 1] == b'\\'
                    && bytes[cursor + 3] == b'\'');
            if closes {
                in_char = true;
            }
            cursor += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            cursor += 1;
            continue;
        }
        if byte == b'/' && next == Some(b'/') {
            in_line_comment = true;
            code[cursor] = b' ';
            code[cursor + 1] = b' ';
            cursor += 2;
            continue;
        }
        if byte == b'/' && next == Some(b'*') {
            block_depth = 1;
            code[cursor] = b' ';
            code[cursor + 1] = b' ';
            cursor += 2;
            continue;
        }
        cursor += 1;
    }
    String::from_utf8(code).expect("masking source preserves UTF-8")
}

fn occurrences(haystack: &str, needle: &str) -> usize {
    haystack.match_indices(needle).count()
}

#[tokio::test]
async fn service_join_expired_budget_aborts_the_nested_worker_through_its_supervisor() {
    struct DropGuard(Arc<AtomicBool>);
    impl Drop for DropGuard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    // The service watch stays unlatched so the supervisor parks in its latch
    // wait; only the expired service budget aborts it. Without the AbortOnDrop
    // guard that abort would detach the worker instead of stopping it.
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let stopped = Arc::new(AtomicBool::new(false));
    let mark_stop = Arc::clone(&stopped);
    let worker = tokio::spawn(async move {
        let _guard = DropGuard(mark_stop);
        std::future::pending::<()>().await;
    });
    let supervisor = join_on_shutdown(worker, shutdown_rx, Duration::from_secs(60), || {});
    let expired =
        std::time::Instant::now() - (crate::shutdown::deadline() + Duration::from_secs(1));
    let latch_mark = std::sync::Mutex::new(Some(expired));
    tokio::time::timeout(
        Duration::from_secs(5),
        crate::join_worker_supervisors(vec![supervisor], &latch_mark),
    )
    .await
    .expect("expired service budget must not grant fresh worker budgets");
    // The worker abort lands asynchronously after its supervisor is dropped,
    // so poll the witness: without the guard the detached worker pends
    // forever and this times out red.
    tokio::time::timeout(Duration::from_secs(5), async {
        while !stopped.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("aborting the supervisor must abort its nested worker");
}

#[test]
fn join_budget_covers_a_latched_containment_drain() {
    // A latched containment worker starts no new Discord step (a latch check
    // before each of the three plan reads and between removals), so the drain
    // that remains is at most one in-flight call plus the settings tail and
    // one bounded staff post. The floor is computed from those named bounds;
    // lowering the budget below it aborts slow-but-healthy containment as
    // wedged. The behavioral pins live in `containment_latch_tests.rs`.
    let floor = crate::containment_runtime::SETTINGS_READ_MAX
        + Duration::from_millis(two_bot_discord::executor::MODERATION_TIMEOUT_MS)
        + crate::raid_runtime::DELIVERY_MAX;
    assert!(
        crate::raid_runtime::WORKER_JOIN_BUDGET >= floor,
        "WORKER_JOIN_BUDGET {:?} must cover the latched containment floor {:?} \
         (settings tail + one in-flight call + one staff post)",
        crate::raid_runtime::WORKER_JOIN_BUDGET,
        floor,
    );
    // Supplement to the behavioral latch tests: the floor holds only while the
    // plan keeps one latch check per Discord step (three reads) and the
    // per-removal check.
    let code = code_only(CONTAINMENT_SOURCE);
    assert!(
        occurrences(&code, "self.latched_plan(dry_run)") >= 3,
        "containment plan must latch-check before each of its three reads"
    );
    assert!(
        occurrences(&code, "is_latched") >= 2,
        "containment plan must keep its per-removal latch check"
    );
}

#[test]
fn main_retains_every_worker_supervisor_for_the_shutdown_join() {
    // Each production constructor's supervisor handle must be pushed into the
    // retained slot and the slot drained into the service join: dropping a
    // supervisor handle detaches its drain, which is exactly the defect this
    // work removes. Deleting any push, the drain, or the join call fails this.
    let main = code_only(MAIN_SOURCE);
    assert_eq!(
        occurrences(&main, ".push(raid_supervisor)"),
        1,
        "raid supervisor must be retained for the shutdown join"
    );
    assert_eq!(
        occurrences(&main, ".push(supervisor)"),
        2,
        "join-risk and containment supervisors must be retained for the shutdown join"
    );
    assert_eq!(
        occurrences(&main, ".drain(..)"),
        1,
        "retained supervisors must be drained into the service join"
    );
    let join = main
        .find("join_worker_supervisors(supervisors, &latch_mark)")
        .expect("retained supervisors must be joined on the shutdown path");
    for binding in [".push(raid_supervisor)", ".push(supervisor)"] {
        for (index, _) in main.match_indices(binding) {
            assert!(
                index < join,
                "supervisor retention must precede the shutdown join"
            );
        }
    }
}
