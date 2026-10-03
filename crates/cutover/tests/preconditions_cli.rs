//! Fixture-driven tests for the cutover preconditions checker.
//!
//! The real repo tree proves the exit-0 path (rehearsed against current
//! main); a scratch fixture tree missing one item from each check family
//! proves exit 1 names it. No DB, no network, no writes outside temp dirs.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_preconditions"))
        .env_clear()
        .args(args)
        .output()
        .unwrap()
}

fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .unwrap()
        .to_owned()
}

fn copy_tree(src: &Path, dst: &Path, rel: &str) {
    let from = src.join(rel);
    let to = dst.join(rel);
    if from.is_dir() {
        fs::create_dir_all(&to).unwrap();
        for entry in fs::read_dir(&from).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            copy_tree(src, dst, &format!("{rel}/{name}"));
        }
    } else {
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        fs::copy(&from, &to).unwrap();
    }
}

fn scratch_base() -> PathBuf {
    // Prefer the run-scoped scratch dir when the harness provides it so test
    // trees stay out of container /tmp; fall back to temp_dir locally.
    std::env::var_os("PAPERCLIP_RUN_SCRATCH_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("PAPERCLIP_SCRATCH_DIR").map(PathBuf::from))
        .unwrap_or_else(std::env::temp_dir)
}

fn scratch_tree(mutate: impl FnOnce(&Path)) -> PathBuf {
    let root = workspace_root();
    let dir = scratch_base().join(format!(
        "preconditions-test-{}-{}",
        std::process::id(),
        time_nanos()
    ));
    for rel in [
        "docs/parity.md",
        "docs/cutover.md",
        "docs/staging-soak.md",
        "docs/backup.md",
        "docs/preflight.md",
        "docs/commands.md",
        "docs/runbook.md",
        "deploy/two-bot-next-backup.timer",
        "deploy/two-bot-next-guild-config-backup.timer",
        "deploy/two-bot-next-restore-drill.timer",
        "crates/core/tests/fixtures/legacy_registry.json",
    ] {
        copy_tree(&root, &dir, rel);
    }
    mutate(&dir);
    dir
}

fn time_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

fn report(stdout: &[u8]) -> serde_json::Value {
    serde_json::from_slice(stdout).unwrap()
}

#[test]
fn current_tree_passes_with_machine_readable_report() {
    let root = workspace_root().to_string_lossy().into_owned();
    let output = run(&["--repo-root", &root]);
    assert_eq!(output.status.code(), Some(0));
    let value = report(&output.stdout);
    assert_eq!(value["ok"], true);
    assert!(value["missing"].as_array().unwrap().is_empty());
    let names: Vec<&str> = value["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    for expected in [
        "parity-zero-unmapped",
        "runbook-docs",
        "runbook-sections",
        "timer-units",
        "registry-fixture",
    ] {
        assert!(names.contains(&expected), "missing check {expected}");
    }
    assert!(output.stderr.is_empty());
}

#[test]
fn missing_items_fail_and_name_each_family() {
    let dir = scratch_tree(|root| {
        // Remove one item from every family: a doc, a section, a timer,
        // the registry fixture, and degrade the parity marker.
        fs::remove_file(root.join("docs/preflight.md")).unwrap();
        let cutover = fs::read_to_string(root.join("docs/cutover.md")).unwrap();
        let trimmed = cutover.replace("## 48-hour watch", "## watch (renamed)");
        fs::write(root.join("docs/cutover.md"), trimmed).unwrap();
        fs::remove_file(root.join("deploy/two-bot-next-restore-drill.timer")).unwrap();
        fs::remove_file(root.join("crates/core/tests/fixtures/legacy_registry.json")).unwrap();
        let parity = fs::read_to_string(root.join("docs/parity.md")).unwrap();
        fs::write(
            root.join("docs/parity.md"),
            parity.replace("Unmapped: **0**", "Unmapped: **3**"),
        )
        .unwrap();
    });
    let arg = dir.to_string_lossy().into_owned();
    let output = run(&["--repo-root", &arg]);
    assert_eq!(output.status.code(), Some(1));
    let value = report(&output.stdout);
    assert_eq!(value["ok"], false);
    let missing: Vec<String> = value["missing"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m.as_str().unwrap().to_owned())
        .collect();
    assert!(missing.iter().any(|m| m.contains("docs/preflight.md")));
    assert!(missing.iter().any(|m| m.contains("48-hour watch")));
    assert!(missing
        .iter()
        .any(|m| m.contains("two-bot-next-restore-drill.timer")));
    assert!(missing.iter().any(|m| m.contains("legacy_registry.json")));
    assert!(missing.iter().any(|m| m.contains("Unmapped")));
    let stderr = String::from_utf8(output.stderr).unwrap();
    for item in &missing {
        assert!(stderr.contains(item), "stderr names {item}");
    }
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn bad_flags_are_usage_not_missing() {
    for args in [
        vec!["--nope"],
        vec!["--repo-root"],
        vec!["--repo-root", "a", "b"],
    ] {
        let output = run(&args);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
    }
}
