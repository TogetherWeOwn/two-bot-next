//! Shared safety opt-ins must be refused before any file or database access.
use std::path::Path;
use std::process::{Command, Output};
use two_bot_cutover::LIVE_GUILD_ID;

const BINS: [&str; 7] = [
    env!("CARGO_BIN_EXE_levels-import-mee6"),
    env!("CARGO_BIN_EXE_levels-role-rewards"),
    env!("CARGO_BIN_EXE_levels-import-rewards-probe"),
    env!("CARGO_BIN_EXE_backfill"),
    env!("CARGO_BIN_EXE_backfill-messages"),
    env!("CARGO_BIN_EXE_capture"),
    env!("CARGO_BIN_EXE_dedupe-events"),
];
const MISSING_INPUT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/safety-flags-absent/export.json"
);
// Closed test-container port, empty password, no inherited credentials or DB URL.
const TEST_ONLY_DB: &str = "postgres://agent_test:@agent-testdb:1/two_bot_test_safety_flags";

fn run(bin: &str, args: &[&str]) -> Output {
    assert!(!Path::new(MISSING_INPUT).exists());
    Command::new(bin)
        .env_clear()
        .env("TWO_DATABASE_URL", TEST_ONLY_DB)
        .args(args)
        .output()
        .unwrap()
}

fn assert_input_refusal(bin: &str, args: &[&str], flag: &str) {
    let output = run(bin, args);
    let error = String::from_utf8(output.stderr).unwrap();
    assert_eq!(output.status.code(), Some(2), "{bin} {args:?}: {error}");
    assert!(output.stdout.is_empty(), "{bin} {args:?}");
    assert!(
        error.starts_with("input error: "),
        "{bin} {args:?}: {error}"
    );
    assert!(error.contains(flag), "{bin} {args:?}: {error}");
    assert!(!error.contains("cannot read"), "{error}");
    assert!(!error.contains("database"), "{error}");
    assert!(!error.contains(TEST_ONLY_DB), "{error}");
}

#[test]
fn valued_and_repeated_opt_ins_fail_before_io_in_every_shared_consumer() {
    for bin in BINS {
        for flag in ["apply", "allow-lower", "allow-live-guild"] {
            let bare = format!("--{flag}");
            let equals_false = format!("--{flag}=false");
            let equals_true = format!("--{flag}=true");
            let equals_empty = format!("--{flag}=");
            for invalid in [
                vec![equals_false.as_str()],
                vec![equals_true.as_str()],
                vec![equals_empty.as_str()],
                vec![bare.as_str(), "false"],
                vec![bare.as_str(), "true"],
                vec![bare.as_str(), bare.as_str()],
                vec![bare.as_str(), equals_false.as_str()],
                vec![equals_false.as_str(), bare.as_str()],
            ] {
                for invalid_first in [false, true] {
                    let mut args = vec![
                        "--guild",
                        LIVE_GUILD_ID,
                        "--file",
                        MISSING_INPUT,
                        "--roles",
                        MISSING_INPUT,
                        "--bot-id=111111111111111111",
                    ];
                    if invalid_first {
                        args.splice(0..0, invalid.clone());
                    } else {
                        args.extend_from_slice(&invalid);
                    }
                    assert_input_refusal(bin, &args, &bare);
                }
            }
        }
    }
}

#[test]
fn inventory_also_rejects_invalid_opt_ins_before_opening_db() {
    for flag in [
        "--apply=false",
        "--allow-lower=false",
        "--allow-live-guild=false",
    ] {
        assert_input_refusal(
            BINS[0],
            &["inventory", "--guild", LIVE_GUILD_ID, flag],
            flag.split('=').next().unwrap(),
        );
    }
}

#[test]
fn conflicting_apply_and_dry_run_fail_before_io() {
    for bin in BINS {
        for invalid in [["--apply", "--dry-run"], ["--dry-run", "--apply"]] {
            let args = [
                "--guild=111111111111111111",
                "--file",
                MISSING_INPUT,
                invalid[0],
                invalid[1],
            ];
            assert_input_refusal(bin, &args, "conflicts");
        }
    }
}

#[test]
fn bare_opt_ins_and_normal_values_reach_the_missing_file_not_the_db() {
    let equals_file = format!("--file={MISSING_INPUT}");
    for args in [
        vec![
            "import",
            "--guild",
            LIVE_GUILD_ID,
            "--file",
            MISSING_INPUT,
            "--apply",
            "--allow-lower",
            "--allow-live-guild",
        ],
        vec![
            "import",
            "--allow-live-guild",
            "--allow-lower",
            "--apply",
            "--guild",
            LIVE_GUILD_ID,
            equals_file.as_str(),
        ],
        vec!["--guild=111111111111111111", "--file", MISSING_INPUT],
    ] {
        let output = run(BINS[0], &args);
        let error = String::from_utf8(output.stderr).unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}: {error}");
        assert!(output.stdout.is_empty());
        assert!(error.starts_with("cannot read "), "{args:?}: {error}");
        assert!(error.contains(MISSING_INPUT));
        assert!(!error.contains("input error") && !error.contains("database"));
        assert!(!error.contains("Refusing live guild"));
    }
}

#[test]
fn live_guild_still_requires_the_bare_opt_in() {
    let output = run(
        BINS[0],
        &["--guild", LIVE_GUILD_ID, "--file", MISSING_INPUT],
    );
    let error = String::from_utf8(output.stderr).unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(error.starts_with("Refusing live guild"));
    assert!(!error.contains("cannot read") && !error.contains("database"));
}
