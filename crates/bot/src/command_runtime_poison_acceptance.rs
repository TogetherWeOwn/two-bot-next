//! Acceptance: `CommandDispatchGuard` releases on a poisoned lock.
//!
//! Contract under test: dropping the guard whose task-scope mutex is
//! poisoned still stops the scope, drains every lane and aborts admitted
//! work instead of panicking inside `drop`, and emits the fixed-vocabulary
//! `two_bot_drop_poison_total{guard="command_dispatch_guard"}` counter. The
//! healthy drop keeps its exact behavior.
//!
//! This module also hosts the workspace Drop-body source scan: no production
//! `fn drop` may contain `.lock().expect()/unwrap()`, and neither may any
//! function it calls (ONE call level followed, same file). RED on unfixed code
//! names every offending `file:line`; GREEN means all guards (and every other
//! production Drop) release poisoned locks, directly or one down. Drops call
//! only drop-only variants using `recover_drop_poison`, never loud `.expect`
//! helpers.
//!
//! Scan-shadow decision (TOG-36798): `truncate_test_tail` used to cut at the
//! FIRST mid-file `#[cfg(test)] mod`, hiding production Drops in
//! `voice_rooms.rs` past line 842 (`GuildRoomWorker::drop` at 3173, drains
//! only; `VoiceActorMetricGuard::drop` at 8882, `unwrap_or_else` +
//! `remove_voice_actor`). The scan now strips ALL inline `#[cfg(test)] mod`
//! blocks (brace-matched, line numbers preserved) instead of truncating, so
//! both Drops are scanned. Executable pins below assert neither Drop body
//! contains `.lock().expect/unwrap()`, independent of the scan walk.
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
    // Inner `#![cfg(test)]` may follow doc comments (this file's own header),
    // so look in the first lines, not just byte zero.
    if text
        .lines()
        .take(30)
        .any(|line| line.trim() == "#![cfg(test)]")
    {
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

/// Strip every inline `#[cfg(test)] mod ... { ... }` block (brace-matched),
/// preserving line numbers by blanking stripped lines. `#[path]` declarations
/// survive: the line after their `#[cfg(test)]` is the path attribute, not a
/// module block, and `mod foo;` file imports are kept (their files are
/// excluded separately via `is_test_only`). Mid-file test mods no longer hide
/// production Drops below them (the `voice_rooms.rs:842` shadow).
fn truncate_test_tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut blank = vec![false; lines.len()];
    let mut index = 0;
    while index < lines.len() {
        let mut stripped = false;
        if lines[index].trim() == "#[cfg(test)]" {
            if let Some(next) = lines.get(index + 1) {
                if next.trim_start().starts_with("mod ") && !next.trim().ends_with(';') {
                    // Inline block: find `mod name {` (brace may be on this or a
                    // later line), then its matching close brace. `#[path]` mods
                    // end with `;` and are kept (checked above).
                    let mut depth = 0usize;
                    let mut opened = false;
                    let mut end = None;
                    for (offset, line) in lines.iter().enumerate().skip(index + 1) {
                        for ch in line.chars() {
                            if ch == '{' {
                                depth += 1;
                                opened = true;
                            } else if ch == '}' {
                                depth = depth.saturating_sub(1);
                            }
                        }
                        if opened && depth == 0 {
                            end = Some(offset);
                            break;
                        }
                        // Do not run past a plausible block end without an opener
                        // (a `mod foo;` slipped through): bail after one line.
                        if !opened && offset > index + 3 {
                            break;
                        }
                    }
                    if let Some(end) = end {
                        for flag in blank.iter_mut().take(end + 1).skip(index) {
                            *flag = true;
                        }
                        index = end + 1;
                        stripped = true;
                    }
                }
            }
        }
        if !stripped {
            index += 1;
        }
    }
    lines
        .iter()
        .enumerate()
        .map(|(i, line)| if blank[i] { "" } else { *line })
        .collect::<Vec<_>>()
        .join("\n")
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

/// A `.lock().expect()/unwrap()` on this line (same line, or the expect on the
/// immediately following line).
fn is_lock_abort(line: &str, next: Option<&&str>) -> bool {
    if line.contains(".lock().expect(") || line.contains(".lock().unwrap(") {
        return true;
    }
    line.contains(".lock()")
        && next.is_some_and(|next| {
            let next = next.trim_start();
            next.starts_with(".expect(") || next.starts_with(".unwrap(")
        })
}

/// Callee names on one Drop-body line: every `.ident(` and bare `ident(`
/// (last segment before the paren). Callers filter to same-file definitions;
/// std helpers simply have no definition here.
fn callees_on(line: &str) -> Vec<&str> {
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'(' {
            let mut end = i;
            while end > 0 && bytes[end - 1].is_ascii_whitespace() {
                end -= 1;
            }
            let mut start = end;
            while start > 0
                && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_')
            {
                start -= 1;
            }
            if start < end {
                if let Some(name) = line.get(start..end) {
                    if !name.is_empty() {
                        // Skip keywords and the `fn name(` declaration itself.
                        if !matches!(
                            name,
                            "if" | "for"
                                | "while"
                                | "match"
                                | "return"
                                | "fn"
                                | "mod"
                                | "struct"
                                | "enum"
                                | "impl"
                                | "let"
                                | "mut"
                                | "self"
                                | "Self"
                                | "super"
                                | "crate"
                        ) {
                            out.push(name);
                        }
                    }
                }
            }
        }
        i += 1;
    }
    out
}

/// Every `.lock().expect()/unwrap()` whose nearest enclosing `fn` above is
/// `drop` (direct), plus every such abort in a function called from a `drop`
/// body (ONE level followed, same file). Returns `file:line` citations; the
/// one-down citation names the callee's lock line with its Drop caller.
fn drop_poison_aborts(path: &Path, text: &str) -> Vec<String> {
    let production = truncate_test_tail(text);
    let lines: Vec<&str> = production.lines().collect();
    // Function spans: each `fn` owns lines until the next `fn` (or EOF).
    let mut fns: Vec<(&str, usize)> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if let Some(name) = fn_name_at(line) {
            fns.push((name, index));
        }
    }
    let fn_end =
        |pos: usize| -> usize { fns.get(pos + 1).map(|(_, at)| *at).unwrap_or(lines.len()) };
    let mut violations = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if !is_lock_abort(line, lines.get(index + 1)) {
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
    // One level: for each `drop`, collect callees in its body; flag lock
    // aborts in any same-file callee definition (excluding `drop` itself).
    for (pos, (name, at)) in fns.iter().enumerate() {
        if *name != "drop" {
            continue;
        }
        let end = fn_end(pos);
        let mut callees = Vec::new();
        for line in &lines[*at..end] {
            callees.extend(callees_on(line));
        }
        callees.sort_unstable();
        callees.dedup();
        for callee in callees {
            if callee == "drop" {
                continue;
            }
            for (cpos, (cname, cat)) in fns.iter().enumerate() {
                if *cname != callee {
                    continue;
                }
                let cend = fn_end(cpos);
                for (offset, cline) in lines[*cat..cend].iter().enumerate() {
                    let abs = *cat + offset;
                    if is_lock_abort(cline, lines.get(abs + 1)) {
                        violations.push(format!(
                            "{}:{}: one-down {}.{}: {}",
                            path.display(),
                            abs + 1,
                            "drop",
                            callee,
                            cline.trim()
                        ));
                    }
                }
            }
        }
    }
    violations.sort();
    violations.dedup();
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

#[test]
fn one_level_follow_flags_a_one_down_expect() {
    let path = Path::new("synthetic_one_down.rs");
    let text = r#"
use std::sync::Mutex;
struct Foo {
    lock: Mutex<u32>,
}
impl Foo {
    fn helper(&self) {
        let _ = self.lock.lock().expect("boom");
    }
}
impl Drop for Foo {
    fn drop(&mut self) {
        self.helper();
    }
}
"#;
    let violations = drop_poison_aborts(path, text);
    assert!(
        !violations.is_empty(),
        "one-level follow must flag a .lock().expect() in a function called from Drop"
    );
    assert!(
        violations
            .iter()
            .any(|v| v.contains("helper") && v.contains("expect")),
        "violation must cite the one-down helper expect, saw: {violations:?}"
    );
}

#[test]
fn one_level_follow_ignores_silent_into_inner_helpers() {
    let path = Path::new("synthetic_silent_helper.rs");
    let text = r#"
use std::sync::Mutex;
struct Foo {
    lock: Mutex<u32>,
}
impl Foo {
    fn helper(&self) {
        let _ = self.lock.lock().unwrap_or_else(|e| e.into_inner());
    }
}
impl Drop for Foo {
    fn drop(&mut self) {
        self.helper();
    }
}
"#;
    let violations = drop_poison_aborts(path, text);
    assert!(
        violations.is_empty(),
        "silent into_inner helpers must stay green, saw: {violations:?}"
    );
}

#[test]
fn voice_rooms_shadow_drops_are_scanned() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let path = root.join("crates/bot/src/voice_rooms.rs");
    let text = std::fs::read_to_string(&path).expect("voice_rooms.rs must read");
    let production = truncate_test_tail(&text);
    // Both production Drops live past the mid-file #[cfg(test)] at line 842;
    // the old tail truncation hid them, the strip keeps them.
    assert!(
        production.contains("finish_pending_joins"),
        "scan must cover GuildRoomWorker::drop (line 3173), not truncate at mid-file tests"
    );
    assert!(
        production.contains("remove_voice_actor"),
        "scan must cover VoiceActorMetricGuard::drop (line 8882), not truncate at mid-file tests"
    );
}

#[test]
fn voice_rooms_shadow_drops_contain_no_lock_expects() {
    // Executable pins, independent of the scan walk (no truncate, no
    // drop_poison_aborts): neither shadowed Drop body may contain
    // `.lock().expect/unwrap()`. GuildRoomWorker::drop drains only (no locks);
    // VoiceActorMetricGuard::drop uses `unwrap_or_else` + `remove_voice_actor`
    // (which itself uses `into_inner`).
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let path = root.join("crates/bot/src/voice_rooms.rs");
    let text = std::fs::read_to_string(&path).expect("voice_rooms.rs must read");
    let lines: Vec<&str> = text.lines().collect();
    for drop_impl in ["Drop for GuildRoomWorker", "Drop for VoiceActorMetricGuard"] {
        let impl_pos = lines.iter().position(|line| line.contains(drop_impl));
        assert!(
            impl_pos.is_some(),
            "{drop_impl} must exist in voice_rooms.rs"
        );
        let impl_at = impl_pos.unwrap_or(0);
        let drop_pos = lines[impl_at..]
            .iter()
            .position(|line| fn_name_at(line) == Some("drop"))
            .map(|offset| impl_at + offset);
        assert!(drop_pos.is_some(), "{drop_impl} must declare fn drop");
        let drop_at = drop_pos.unwrap_or(impl_at);
        // Bound the body at the end of this `fn drop`, not a fixed line
        // window: the slice must never reach the next item (`load`, docs),
        // or a future expect/unwrap there fails this pin while Drop is valid.
        let drop_end = lines[drop_at..]
            .iter()
            .position(|line| *line == "    }")
            .map_or(lines.len(), |offset| drop_at + offset + 1);
        let body: String = lines[drop_at..drop_end].join("\n");
        assert!(
            !body.contains(".lock().expect(") && !body.contains(".lock().unwrap("),
            "{drop_impl} body must not abort on a poisoned lock, saw:\n{body}"
        );
        assert!(
            !body.contains(".expect(") && !body.contains(".unwrap("),
            "{drop_impl} body must not expect/unwrap at all, saw:\n{body}"
        );
    }
}
