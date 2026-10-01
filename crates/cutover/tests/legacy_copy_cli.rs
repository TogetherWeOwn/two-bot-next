//! Process-level refusal tests: every case exits before a DB connection.
use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_legacy_copy"))
        .env_clear()
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn plan_is_offline_and_never_echoes_url_credentials() {
    let output = run(&[
        "--plan",
        "--source-url=postgres://fake:source-secret@source.invalid/db",
        "--target-url=postgres://fake:target-secret@target.invalid/db",
    ]);
    assert!(output.status.success());
    let plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["version"], 1);
    assert_eq!(plan["apply"], false);
    assert!(plan["groups"]
        .as_array()
        .unwrap()
        .iter()
        .any(|g| g["name"] == "internal_actions" && g["status"] == "pending"));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(!text.contains("secret") && !text.contains(".invalid"));
    assert!(output.stderr.is_empty());
}

#[test]
fn pending_refusal_precedes_any_url_or_connection_attempt() {
    let output = run(&["--groups=tickets", "--apply"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("pending group tickets"));
}

#[test]
fn live_target_refusal_precedes_source_connection_and_withholds_credentials() {
    let output = run(&[
        "--source-url=postgres://fake:source-secret@source.invalid:5432/source",
        "--target-url=postgres://fake:target-secret@live.invalid:5432/target",
        "--apply",
    ]);
    assert_eq!(output.status.code(), Some(2));
    let text =
        String::from_utf8(output.stdout).unwrap() + &String::from_utf8(output.stderr).unwrap();
    assert!(text.contains("Refusing live/unknown target"));
    assert!(!text.contains("source connection failed"));
    assert!(!text.contains("secret") && !text.contains(".invalid"));
}

#[test]
fn invalid_flags_are_not_interpreted_as_apply_authorization() {
    for args in [
        vec!["--apply=false"],
        vec!["--apply", "false"],
        vec!["--allow-live-target=no"],
        vec!["--batch-size=10001"],
        vec!["--groups=unknown"],
        vec!["--apply", "--apply"],
    ] {
        let output = run(&args);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(!String::from_utf8(output.stderr)
            .unwrap()
            .contains("connection failed"));
    }
}
