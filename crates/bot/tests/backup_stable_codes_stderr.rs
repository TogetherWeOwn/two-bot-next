//! The real binary, run with a cleared environment: failure output is one
//! coded physical line per failure, so per-line matchers never see an
//! uncoded line.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::var_os("PAPERCLIP_RUN_SCRATCH_DIR")
            .or_else(|| std::env::var_os("PAPERCLIP_SCRATCH_DIR"))
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = root.join(format!(
            "backup-stable-codes-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn is_empty(&self) -> bool {
        // Same coverage-harness carve-out as `backup_cli_help.rs`: an
        // instrumented child writes its profile next to its cwd when
        // LLVM_PROFILE_FILE is unset; that is harness output, not product
        // output.
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

/// Cleared env, scratch cwd: failures must not read ambient config or write
/// beside the test.
fn two_bot(dir: &Scratch, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_two-bot"));
    command.args(args).env_clear().current_dir(&dir.0);
    if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
        command.env("LLVM_PROFILE_FILE", profile);
    }
    command
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[test]
fn token_check_refusal_is_a_single_coded_line() {
    // Every `check_staging_token` refusal embeds a newline; the snapshot
    // site flattens it so the code lands on the only failure line. The
    // refusal happens before any env read beyond the token, so a cleared
    // env plus a bogus token is a deterministic offline probe.
    let dir = Scratch::new();
    let output = two_bot(&dir, &["guild-config-snapshot"])
        .env("DISCORD_STAGING_BOT_TOKEN", "invalid-token")
        .output()
        .unwrap();
    let stderr = text(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(output.stdout.is_empty(), "{stderr}");
    let lines: Vec<&str> = stderr.lines().collect();
    assert_eq!(lines.len(), 1, "{stderr}");
    assert!(
        lines[0].ends_with(" code=snapshot_token_check_refused"),
        "{stderr}"
    );
    assert!(dir.is_empty());
}

#[test]
fn route_usage_refusals_carry_stable_codes() {
    for (subcommand, code) in [
        ("backup", "backup_usage"),
        ("guild-config-snapshot", "snapshot_usage"),
    ] {
        let dir = Scratch::new();
        let output = two_bot(&dir, &[subcommand, "now"]).output().unwrap();
        let stderr = text(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{subcommand}: {stderr}");
        assert!(output.stdout.is_empty(), "{subcommand}: {stderr}");
        let first = stderr.lines().next().unwrap();
        assert_eq!(
            first,
            format!("{subcommand}: unexpected argument \"now\". code={code}"),
            "{subcommand}: {stderr}"
        );
        assert!(dir.is_empty());
    }
}
