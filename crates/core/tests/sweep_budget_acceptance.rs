//! Sweep-budget acceptance: budgeted passes page under a cap and resume completes.
//!
//! RED now — child compile refusal: the Builder adds a budgeted, resumable
//! pass beside the complete sweep in `inactivity_store.rs`:
//!
//! ```rust,ignore
//! pub struct SweepBudget {
//!     pub max_pages: usize,
//!     pub deadline: std::time::Instant,
//! }
//! pub struct SweepCursor; // opaque to callers; moved by value
//! pub async fn run_sweep_budgeted(
//!     pool: &Pool<Postgres>,
//!     now: &str,
//!     days: u64,
//!     budget: SweepBudget,
//!     resume: Option<SweepCursor>,
//! ) -> Result<(InactivityOutcome, Option<SweepCursor>), InactivityStoreError>;
//! ```
//!
//! Semantics: at most `max_pages` selects per pass; stop when the deadline
//! passes (checked per page); return `Some(cursor)` iff more work may remain
//! (short/empty page means done and returns `None`; an expired deadline before
//! any page returns `Some`). Resume passes continue past the cursor: the union
//! across passes is byte-identical to one complete sweep, every member event
//! is written exactly once, nothing is silently skipped. `run_sweep` keeps its
//! signature and complete semantics; the paged SELECT keeps its SQL text.
//!
//! Like `inactivity_sweep_acceptance.rs`, this target child-compiles the
//! production sweep body (not a copy of its logic) beside the test-only SQL
//! boundary in `fixtures/sweep_budget_driver.rs`, so the RED names the
//! missing contracted API and the GREEN runs the real budgeted pass offline.
//! Adoption is pinned in `sweep_bound_guard.rs`, which runs on the unmodified
//! tree. Offline: no database, socket, credential, or wall-clock benchmark.
//! The child compiler inherits the admitted Cargo lease's scratch directory;
//! run this target through scripts/cargo_cache.py on persistent controllers.
//!
//! Builder contract: satisfy the semantics. New items live in the production
//! part of `inactivity_store.rs` (above `#[cfg(test)]`), add no new
//! external-crate imports or derives (std only), and keep every SQL statement
//! byte-identical. If any shape needs to change, renegotiate on the card —
//! never edit this file or the driver to loosen it.

use std::path::PathBuf;
use std::process::Command;

const STORE: &str = include_str!("../src/inactivity_store.rs");
const DOMAIN: &str = include_str!("../src/inactivity.rs");
const FUNNEL: &str = include_str!("../src/funnel.rs");
const DRIVER: &str = include_str!("fixtures/sweep_budget_driver.rs");

// Only documentation and dependency derive attributes are removed. In particular,
// run_sweep, selection, timestamp arithmetic and filtering are never rewritten.
fn dependency_free(source: &str) -> String {
    source
        .lines()
        .filter(|line| {
            let line = line.trim_start();
            !line.starts_with("//!")
                && !line.starts_with("#[error(")
                && line != "use serde::Serialize;"
        })
        .map(|line| {
            line.replace(", thiserror::Error", "")
                .replace(", Serialize", "")
                .replace("#[from] ", "")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn executable_subject() -> String {
    let store = STORE
        .split_once("#[cfg(test)]")
        .expect("store production/test boundary")
        .0;
    let domain = DOMAIN
        .split_once("/// Header row of the reengagement CSV")
        .expect("domain sweep/CSV boundary")
        .0;
    let funnel = FUNNEL
        .split_once("pub fn format_iso_millis")
        .expect("timestamp formatter")
        .1
        .split_once("#[cfg(test)]")
        .expect("timestamp production/test boundary")
        .0;
    format!(
        "{DRIVER}\nmod funnel {{ {} }}\nmod inactivity {{ {} }}\n\
         mod subject {{\nuse crate::SubjectVec as Vec;\n{}\n\
         impl From<crate::Error> for InactivityStoreError {{\n\
         fn from(error: crate::Error) -> Self {{ Self::Db(error) }}\n}}\n}}\n",
        dependency_free(&format!("pub fn format_iso_millis{funnel}")),
        dependency_free(domain),
        dependency_free(store),
    )
}

#[test]
fn sweep_budget_offline_acceptance() {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let root = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let scratch = root.join(format!(
        "sweep-budget-acceptance-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir(&scratch).expect("create owned compiler scratch");
    let source = scratch.join("acceptance.rs");
    let binary = scratch.join("acceptance");
    std::fs::write(&source, executable_subject()).expect("write test-only subject");
    let compile = Command::new("rustc")
        .args(["--edition=2021", "--test", "-C", "debuginfo=0"])
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .output()
        .expect("run admitted child compiler");
    if !compile.status.success() {
        panic!(
            "acceptance fixture COMPILE REFUSAL (not behavioral RED):\n{}",
            String::from_utf8_lossy(&compile.stderr)
        );
    }
    let run = Command::new(&binary)
        .args(["--test-threads=1", "--nocapture"])
        .output()
        .expect("execute offline fixture");
    let evidence = format!(
        "{}\n{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    std::fs::remove_file(&source).expect("remove owned generated source");
    std::fs::remove_file(&binary).expect("remove owned fixture binary");
    std::fs::remove_dir(&scratch).expect("remove owned fixture directory");
    assert!(
        run.status.success(),
        "behavioral acceptance RED:\n{evidence}"
    );
}
