//! Acceptance: `CommandDispatchGuard` releases on a poisoned lock.
//!
//! Contract under test: dropping the guard whose task-scope mutex is
//! poisoned still stops the scope, drains every lane and aborts admitted
//! work instead of panicking inside `drop`, and emits the fixed-vocabulary
//! `two_bot_drop_poison_total{guard="command_dispatch_guard"}` counter. The
//! healthy drop keeps its exact behavior.
//!
//! This module also hosts the workspace Drop-body source scan: no production
//! `fn drop` may contain `.lock().expect()/unwrap()`. RED on unfixed code
//! names every offending `file:line`; GREEN means all four guards (and every
//! other production Drop) release poisoned locks.
//!
//! The runtime fixture is fully offline: a lazy pool that never connects, a
//! loopback executor origin that is validated but never called, and a
//! command-free router. Lane tasks are detached pending futures aborted by
//! the drop under test.
#![cfg(test)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use two_bot_core::RouterGates;
use two_bot_discord::ActionExecutor;

use super::{router_with_commands, CommandDispatchGuard, CommandRuntime, LANE_MESSAGES};

const GUARD: &str = "command_dispatch_guard";
const GUILD: u64 = 7;

/// Poison `mutex` via a caught holder panic. The panic never escapes: the
/// runner only sees the poisoned lock.
fn poison<T>(mutex: &Mutex<T>) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _held = mutex.lock().expect("holder takes the healthy lock");
        std::panic::panic_any("poison the task-scope lock");
    }));
    assert!(mutex.is_poisoned(), "holder must leave the lock poisoned");
}

/// Rendered `two_bot_drop_poison_total{guard="..."}` count, or `None` while
/// the series does not exist. Global counters are monotonic, so `>= 1` after
/// this test's own poisoned drop is order- and parallel-safe: only this
/// guard's label is read.
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

/// Offline runtime: lazy pool, validated-but-uncalled executor, bare router.
fn offline_runtime() -> Arc<CommandRuntime> {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("lazy pool never connects");
    // Validated, never called: these tests drop guards without touching REST.
    let executor = ActionExecutor::with_proxy(
        "drop-poison-test-token".to_owned(),
        Some("http://127.0.0.1:1/".to_owned()),
    )
    .expect("loopback origin validates without I/O");
    let gates = RouterGates {
        configured_guild: Some(GUILD),
        scorecard: false,
        automations: false,
        announcements: false,
        moderation: false,
        voice: false,
        voice_assistant: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    };
    CommandRuntime::new(pool, executor, router_with_commands(gates), GUILD, false)
}

/// Admit one never-completing task into the message lane. The join handle is
/// detached; only the abort handle survives, so the drop under test owns the
/// only path that finishes the task.
async fn admit_lane_task(runtime: &CommandRuntime) -> tokio::task::AbortHandle {
    let probe = tokio::spawn(std::future::pending::<()>());
    let handle = probe.abort_handle();
    runtime.tasks.lock().expect("healthy lane insert").lanes[LANE_MESSAGES].push(handle.clone());
    handle
}

async fn await_aborted(handle: &tokio::task::AbortHandle, what: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if handle.is_finished() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{what} must finish after its abort"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn healthy_dispatch_guard_drop_stops_and_aborts_lanes() {
    let runtime = offline_runtime();
    let handle = admit_lane_task(&runtime).await;
    drop(CommandDispatchGuard(Arc::clone(&runtime)));
    await_aborted(&handle, "lane task").await;
    let tasks = runtime.tasks.lock().expect("healthy lock");
    assert!(tasks.stopped, "healthy drop stops the scope");
    assert!(
        tasks.lanes.iter().all(Vec::is_empty),
        "healthy drop drains every lane"
    );
}

#[tokio::test]
async fn poisoned_dispatch_guard_drop_releases_instead_of_panicking() {
    let runtime = offline_runtime();
    let handle = admit_lane_task(&runtime).await;
    poison(&runtime.tasks);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(CommandDispatchGuard(Arc::clone(&runtime)));
    }));
    assert!(
        outcome.is_ok(),
        "poisoned dispatch-guard drop must release, not panic"
    );
    await_aborted(&handle, "lane task under poison").await;
    let tasks = runtime
        .tasks
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert!(tasks.stopped, "poisoned drop still stops the scope");
    assert!(
        tasks.lanes.iter().all(Vec::is_empty),
        "poisoned drop still drains every lane"
    );
}

#[tokio::test]
async fn poisoned_dispatch_guard_drop_emits_fixed_vocabulary_counter() {
    let runtime = offline_runtime();
    poison(&runtime.tasks);
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(CommandDispatchGuard(Arc::clone(&runtime)));
    }));
    let count = drop_poison_count(GUARD).expect("poison counter series must render");
    assert!(
        count >= 1,
        "poisoned drop must emit {GUARD} counter, saw {count}"
    );
}

/// Production sources under the workspace root: every `crates/*/src` tree
/// plus the root package. Test-only code is excluded by path (`tests/`,
/// `*_tests.rs`), by crate (`testsupport`) and by `#![cfg(test)]` content,
/// mirroring the roadmap's production `impl Drop` census.
fn production_sources(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.join("src")];
    if let Ok(crates) = std::fs::read_dir(root.join("crates")) {
        for entry in crates.flatten() {
            stack.push(entry.path().join("src"));
        }
    }
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

fn is_test_only(path: &Path, text: &str) -> bool {
    if text.starts_with("#![cfg(test)]") {
        return true;
    }
    if path
        .components()
        .any(|c| matches!(c.as_os_str().to_str(), Some("tests") | Some("testsupport")))
    {
        return true;
    }
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.contains("_test") || n == "tests.rs" || n.starts_with("test"))
}

/// Cut the inline `#[cfg(test)] mod tests` tail (runs to EOF). `#[path]`
/// declarations survive: the line after their `#[cfg(test)]` is the path
/// attribute, not a module block.
fn truncate_test_tail(text: &str) -> &str {
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        if line.trim() == "#[cfg(test)]" {
            let rest = &text[start + line.len()..];
            if rest
                .lines()
                .next()
                .is_some_and(|next| next.trim_start().starts_with("mod "))
            {
                return &text[..start];
            }
        }
        start += line.len();
    }
    text
}

/// Name of the `fn` declared on this line, if any: `fn drop`, `pub(crate)
/// async fn begin` and friends. Attribute, comment, `use` and struct lines
/// yield nothing.
fn fn_name_at(line: &str) -> Option<&str> {
    let mut rest = line.trim();
    loop {
        if let Some(after) = rest.strip_prefix("pub(crate)") {
            rest = after.trim_start();
        } else if rest.starts_with("pub") && rest["pub".len()..].starts_with(['(', ' ']) {
            rest = rest["pub".len()..].trim_start();
            if let Some(after) = rest.strip_prefix('(') {
                let end = after.find(')')?;
                rest = after[end + 1..].trim_start();
            }
        } else if let Some(after) = rest.strip_prefix("async") {
            if after.starts_with(|c: char| c.is_alphanumeric() || c == '_') {
                break;
            }
            rest = after.trim_start();
        } else if let Some(after) = rest.strip_prefix("const") {
            if after.starts_with(|c: char| c.is_alphanumeric() || c == '_') {
                break;
            }
            rest = after.trim_start();
        } else {
            break;
        }
    }
    let after = rest.strip_prefix("fn")?;
    if after.starts_with(|c: char| c.is_alphanumeric() || c == '_') {
        return None;
    }
    let after = after.trim_start();
    let len: usize = after
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .map(char::len_utf8)
        .sum();
    if len == 0 {
        return None;
    }
    Some(&after[..len])
}

/// Every `.lock().expect()/unwrap()` (same line, or the expect on the
/// immediately following line) whose nearest enclosing `fn` above is `drop`.
/// Returns `file:line` citations.
fn drop_poison_aborts(path: &Path, text: &str) -> Vec<String> {
    let production = truncate_test_tail(text);
    let lines: Vec<&str> = production.lines().collect();
    let mut violations = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let same_line = line.contains(".lock().expect(") || line.contains(".lock().unwrap(");
        let split_call = !same_line
            && line.contains(".lock()")
            && lines.get(index + 1).is_some_and(|next| {
                let next = next.trim_start();
                next.starts_with(".expect(") || next.starts_with(".unwrap(")
            });
        if !same_line && !split_call {
            continue;
        }
        let enclosing = lines[..index]
            .iter()
            .rev()
            .find_map(|above| fn_name_at(above));
        if enclosing == Some("drop") {
            violations.push(format!("{}:{}: {}", path.display(), index + 1, line.trim()));
        }
    }
    violations
}

#[test]
fn no_drop_body_aborts_on_a_poisoned_lock() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut violations = Vec::new();
    let mut scanned = 0;
    for path in production_sources(&root) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if is_test_only(&path, &text) {
            continue;
        }
        scanned += 1;
        violations.extend(drop_poison_aborts(&path, &text));
    }
    assert!(
        scanned > 0,
        "source walk found no production files under {}",
        root.display()
    );
    assert!(
        violations.is_empty(),
        "Drop bodies must release poisoned locks, never expect:\n{}",
        violations.join("\n")
    );
}
