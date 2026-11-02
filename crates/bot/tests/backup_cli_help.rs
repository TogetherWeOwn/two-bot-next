//! The real binary, run with a cleared environment: help after an operator
//! subcommand prints usage and touches nothing; stray arguments refuse.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

const SUBCOMMANDS: [&str; 5] = [
    "backup",
    "restore",
    "backup-upload",
    "guild-config-snapshot",
    "guild-config-restore",
];
const USAGE_HEAD: &str = "two-bot operator commands\n";

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::var_os("PAPERCLIP_RUN_SCRATCH_DIR")
            .or_else(|| std::env::var_os("PAPERCLIP_SCRATCH_DIR"))
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = root.join(format!(
            "backup-cli-help-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn is_empty(&self) -> bool {
        // Coverage harness artifact, not product output: an instrumented
        // child (cargo-llvm-cov) writes its profile next to its cwd when
        // LLVM_PROFILE_FILE is unset. `two_bot` forwards the parent's path
        // so this is normally empty; ignore any stray profile so a harness
        // change cannot re-flake the "touches nothing" assertion.
        std::fs::read_dir(&self.0)
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| {
                !entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.ends_with(".profraw"))
            })
            .is_none()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Cleared env, scratch cwd: the default `./backups` would land in `dir`.
fn two_bot(dir: &Scratch, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_two-bot"));
    command.args(args).env_clear().current_dir(&dir.0);
    // Under cargo-llvm-cov the child is instrumented and writes its profile
    // to LLVM_PROFILE_FILE. `env_clear` would drop that redirect, so the
    // child drops `default.profraw` into the scratch dir and `is_empty`
    // flakes. Forward the harness redirect (it carries `%p`/`%m`, so each
    // child still gets its own profile); the cleared app env stays cleared.
    if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
        command.env("LLVM_PROFILE_FILE", profile);
    }
    command
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn assert_usage(output: &Output, args: &[&str]) {
    let stdout = text(&output.stdout);
    let stderr = text(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{args:?}: {stderr}");
    assert!(stdout.starts_with(USAGE_HEAD), "{args:?}: {stdout}");
    assert!(stdout.contains("two-bot guild-config-restore --snapshot FILE"));
    assert!(stderr.is_empty(), "{args:?}: {stderr}");
}

#[test]
fn help_after_each_subcommand_prints_usage_and_exits_zero() {
    for subcommand in SUBCOMMANDS {
        for help in ["--help", "-h"] {
            let dir = Scratch::new();
            let args = [subcommand, help];
            let output = two_bot(&dir, &args).output().unwrap();
            assert_usage(&output, &args);
            assert!(dir.is_empty(), "{args:?} wrote to its working directory");
        }
    }
}

#[test]
fn backup_help_never_reaches_a_configured_database() {
    // An operator shell with backup.env loaded: help must still decide
    // before env, so nothing connects and no backup directory appears.
    // The URL names a port that can never accept a database (port 1 is
    // privileged and never a test listener), so the proof is the child's
    // deterministic output, not a timing-sensitive accept on a live
    // listener: a future change that dials first fails here with exit 1
    // and a redacted connection error on stderr instead of usage.
    // Same closed-port probe as
    // `crates/cutover/tests/admission_configuration.rs`.
    let dir = Scratch::new();
    let args = ["backup", "--help"];
    let output = two_bot(&dir, &args)
        .env(
            "TWO_DATABASE_URL",
            "postgres://fixture-user:fixture-password@127.0.0.1:1/db",
        )
        .env("TWO_BACKUP_DIR", dir.0.join("backups"))
        .output()
        .unwrap();
    assert_usage(&output, &args);
    assert!(dir.is_empty(), "backup --help created a backup directory");
    assert!(
        !dir.0.join("backups").exists(),
        "backup --help created its configured backup directory"
    );
}

#[test]
fn scratch_is_empty_ignores_stray_profraw_profiles() {
    let dir = Scratch::new();
    assert!(dir.is_empty());
    std::fs::write(dir.0.join("default.profraw"), b"profile").unwrap();
    assert!(
        dir.is_empty(),
        "stray coverage profile should not count as product output"
    );
    std::fs::write(dir.0.join("output.txt"), b"product").unwrap();
    assert!(
        !dir.is_empty(),
        "non-profile output should still count as touching the directory"
    );
}

#[test]
fn extra_argument_to_backup_or_snapshot_exits_two_with_usage() {
    for subcommand in ["backup", "guild-config-snapshot"] {
        let code = if subcommand == "backup" {
            "backup_usage"
        } else {
            "snapshot_usage"
        };
        let dir = Scratch::new();
        let output = two_bot(&dir, &[subcommand, "now"]).output().unwrap();
        let stderr = text(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{subcommand}: {stderr}");
        assert!(output.stdout.is_empty());
        assert!(
            stderr.starts_with(&format!(
                "{subcommand}: unexpected argument \"now\". code={code}\n{USAGE_HEAD}"
            )),
            "{subcommand}: {stderr}"
        );
        assert!(dir.is_empty());
    }
}
