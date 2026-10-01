//! Hermetic child-process proofs: replacement validation precedes DB setup.
use std::process::{Command, Output};

const GUILD: &str = "90000000000000001";
const ROLE: &str = "90000000000000002";

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_levels-role-rewards"))
        .args(args)
        .env_clear()
        .output()
        .expect("run reward CLI")
}

#[test]
fn invalid_replacements_fail_before_database_configuration() {
    for (spec, diagnostic) in [
        ("0:90000000000000002", "Usage:"),
        ("2147483648:90000000000000002", "reward level must be between"),
        ("18446744073709551615:90000000000000002", "reward level must be between"),
        ("1:90000000000000002,2:90000000000000002", "duplicate Discord role id"),
        ("no:90000000000000002", "Usage:"),
        ("1:bad", "Usage:"),
        ("1:90000000000000002:extra", "Usage:"),
        ("1:90000000000000002,", "Usage:"),
    ] {
        let output = run(&["--guild", GUILD, "--set", spec]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{spec}: {stderr}");
        assert!(stderr.contains(diagnostic), "{spec}: {stderr}");
        assert!(!stderr.contains("TWO_DATABASE_URL"), "{spec}: {stderr}");
        assert!(!stderr.contains("cannot open database"), "{spec}: {stderr}");
        assert!(output.stdout.is_empty());
    }
    let output = run(&["--guild", GUILD, "--set"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr.contains("Usage:"));
    assert!(!stderr.contains("TWO_DATABASE_URL"));
}

#[test]
fn valid_replacements_reach_database_configuration() {
    for spec in [
        "2147483647:90000000000000002",
        "1:90000000000000002,2:90000000000000002,1:90000000000000003",
        "",
        "   ",
    ] {
        let output = run(&["--guild", GUILD, "--set", spec]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{spec}: {stderr}");
        assert!(stderr.contains("TWO_DATABASE_URL is required"), "{spec}: {stderr}");
    }
    let output = run(&["--guild", GUILD]);
    assert!(String::from_utf8_lossy(&output.stderr).contains("TWO_DATABASE_URL is required"));
}

#[test]
fn live_guild_refusal_still_precedes_reward_and_database_validation() {
    let output = run(&["--guild", two_bot_cutover::LIVE_GUILD_ID, "--set", "bad"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr.contains("Refusing live guild"));
    assert!(!stderr.contains("TWO_DATABASE_URL"));
    assert!(!stderr.contains("Usage:"));
    let output = run(&["--guild", GUILD, "--set", &format!("1:{ROLE}")]);
    assert!(String::from_utf8_lossy(&output.stderr).contains("TWO_DATABASE_URL is required"));
}
