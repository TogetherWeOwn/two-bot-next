//! Offline acceptance: execute the production sweep with a recording SQL boundary.
//!
//! No database, socket, environment credential, or wall-clock benchmark is used.
//! The child compiler inherits the admitted Cargo lease's scratch directory; run
//! this target through scripts/cargo_cache.py on persistent controllers.

use std::path::PathBuf;
use std::process::Command;

const STORE: &str = include_str!("../src/inactivity_store.rs");
const DOMAIN: &str = include_str!("../src/inactivity.rs");
const FUNNEL: &str = include_str!("../src/funnel.rs");
const DRIVER: &str = include_str!("fixtures/inactivity_sweep_driver.rs");

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
         mod subject {{\nuse crate::CountingVec as Vec;\n{}\n\
         impl From<crate::Error> for InactivityStoreError {{\n\
         fn from(error: crate::Error) -> Self {{ Self::Db(error) }}\n}}\n}}\n",
        dependency_free(&format!("pub fn format_iso_millis{funnel}")),
        dependency_free(domain),
        dependency_free(store),
    )
}

#[test]
fn inactivity_sweep_offline_acceptance() {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let root = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let scratch = root.join(format!(
        "inactivity-sweep-acceptance-{}-{}",
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
