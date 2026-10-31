//! Acceptance: `two-bot restore <file> --dry-run` reports `DRY RUN FAILED`
//! (never `DRY RUN VERIFIED`) for an archive whose row carries only a
//! strict subset of its table's manifest columns. Such a row can never
//! restore (restore refuses the missing cell), so a dry run must not
//! verify the file.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;
use two_bot_core::backup::dump_file::{self, DUMP_TABLES, DUMP_VERSION};

/// A cell value that only exists inside the test archive. CLI output that
/// contains it echoed row content to the operator.
const ECHO_SENTINEL: &str = "sentinel_subset_row_echo_4e2a";

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::var_os("PAPERCLIP_RUN_SCRATCH_DIR")
            .or_else(|| std::env::var_os("PAPERCLIP_SCRATCH_DIR"))
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = root.join(format!(
            "backup-dry-run-subset-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn subset_dump_bytes() -> Vec<u8> {
    let tables: Vec<Value> = DUMP_TABLES
        .iter()
        .map(|name| {
            let (cols, count): (Vec<&str>, u64) = if *name == "events" {
                (vec!["id", "guild_id"], 1)
            } else {
                (Vec::new(), 0)
            };
            serde_json::json!({
                "name": name,
                "columns": cols,
                "column_types": cols.iter().map(|_| "text").collect::<Vec<_>>(),
                "count": count,
            })
        })
        .collect();
    // The row omits the declared `guild_id` column; its surviving cell
    // carries the sentinel so any echo into CLI output is visible.
    let objs = [
        serde_json::json!({
            "kind": "manifest",
            "version": DUMP_VERSION,
            "createdAt": "2026-08-25T00:00:00.000Z",
            "tables": tables,
            "eventsSequence": 1,
            "schemaMigrations": ["001"],
        }),
        serde_json::json!({"kind":"row","table":"events","data":{"id": ECHO_SENTINEL}}),
        serde_json::json!({"kind":"end","rows":1}),
    ];
    let mut enc = dump_file::new_encoder();
    for obj in &objs {
        let mut line = serde_json::to_vec(obj).unwrap();
        line.push(b'\n');
        enc.write_all(&line).unwrap();
    }
    enc.finish().unwrap()
}

fn restore_dry_run(dir: &Scratch, file: &std::path::Path) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_two-bot"));
    command
        .args(["restore", file.to_str().unwrap(), "--dry-run"])
        .env_clear()
        .current_dir(&dir.0);
    if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
        command.env("LLVM_PROFILE_FILE", profile);
    }
    command.output().unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[test]
fn acceptance_dry_run_fails_for_subset_row_and_never_verifies() {
    let dir = Scratch::new();
    let file = dir.0.join("subset.ndjson.gz");
    std::fs::write(&file, subset_dump_bytes()).unwrap();

    let output = restore_dry_run(&dir, &file);
    let stdout = text(&output.stdout);
    let stderr = text(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("DRY RUN FAILED"),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stdout.contains("DRY RUN VERIFIED"),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stdout.contains(ECHO_SENTINEL) && !stderr.contains(ECHO_SENTINEL),
        "stdout: {stdout}\nstderr: {stderr}"
    );
}
