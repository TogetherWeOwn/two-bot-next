//! The real binary, run with a cleared environment: help after an operator
//! subcommand prints usage and touches nothing; stray arguments refuse.

use std::net::TcpListener;
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
        std::fs::read_dir(&self.0).unwrap().next().is_none()
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
    let dir = Scratch::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let args = ["backup", "--help"];
    let output = two_bot(&dir, &args)
        .env(
            "TWO_DATABASE_URL",
            format!("postgres://fixture-user:fixture-password@127.0.0.1:{port}/db"),
        )
        .env("TWO_BACKUP_DIR", dir.0.join("backups"))
        .output()
        .unwrap();
    assert_usage(&output, &args);
    assert!(dir.is_empty(), "backup --help created a backup directory");
    let accepted = listener.accept();
    assert!(
        matches!(&accepted, Err(err) if err.kind() == std::io::ErrorKind::WouldBlock),
        "backup --help opened a database connection"
    );
}

#[test]
fn extra_argument_to_backup_or_snapshot_exits_two_with_usage() {
    for subcommand in ["backup", "guild-config-snapshot"] {
        let dir = Scratch::new();
        let output = two_bot(&dir, &[subcommand, "now"]).output().unwrap();
        let stderr = text(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{subcommand}: {stderr}");
        assert!(output.stdout.is_empty());
        assert!(
            stderr.starts_with(&format!(
                "{subcommand}: unexpected argument \"now\".\n{USAGE_HEAD}"
            )),
            "{subcommand}: {stderr}"
        );
        assert!(dir.is_empty());
    }
}
