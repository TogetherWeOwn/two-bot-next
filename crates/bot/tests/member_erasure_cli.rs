//! Real operator CLI acceptance. No gateway/Discord/runtime credentials.

use std::process::Output;
use std::time::Duration;
use tokio::process::Command;
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "1545644954272137297";
const USER: &str = "123456789012345678";

async fn cli(args: &[&str], database_url: Option<&str>, actor: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_two-bot"));
    command
        .env_clear()
        .env("TWO_DATABASE_TLS", "local-only")
        .args(args)
        .kill_on_drop(true);
    if let Some(url) = database_url {
        command.env("TWO_DATABASE_URL", url);
    }
    if let Some(actor) = actor {
        command.env("TWO_ERASURE_ACTOR", actor);
    }
    tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .expect("operator command must exit, not start the gateway")
        .unwrap()
}

fn table_lines(output: &Output, final_line: &str) -> Vec<String> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let text = String::from_utf8(output.stdout.clone()).unwrap();
    assert!(!text.contains(GUILD) && !text.contains(USER));
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    assert_eq!(lines.pop().as_deref(), Some(final_line));
    lines
}

#[tokio::test]
async fn real_erasure_cli_validates_before_connection_and_redacts_failures() {
    let help = cli(&["erase-member", "--help"], None, None).await;
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("Dry run by default"));
    let invalid = cli(
        &["erase-member", "--guild", GUILD, "--user", "0"],
        None,
        None,
    )
    .await;
    assert_eq!(invalid.status.code(), Some(2));
    let missing_actor = cli(
        &[
            "erase-member",
            "--guild",
            GUILD,
            "--user",
            USER,
            "--execute",
        ],
        None,
        None,
    )
    .await;
    assert_eq!(missing_actor.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&missing_actor.stderr).contains("TWO_ERASURE_ACTOR"));
    // An invalid parse fixture, not a credential or an attempted external call.
    let fake = "fixture-DO-NOT-LOG-private-value";
    let bad_url = format!("not-a-postgres-url:{fake}");
    let failure = cli(
        &["erase-member", "--guild", GUILD, "--user", USER],
        Some(&bad_url),
        None,
    )
    .await;
    assert_eq!(failure.status.code(), Some(1));
    let diagnostics = String::from_utf8_lossy(&failure.stderr);
    assert!(
        !diagnostics.contains(fake) && !diagnostics.contains(USER) && !diagnostics.contains(GUILD)
    );
}

#[tokio::test]
async fn real_erasure_cli_dry_execute_and_second_execute_counts() {
    let bootstrap = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure operator CLI acceptance"
            );
            eprintln!("SKIP erasure CLI database integration: TWO_TEST_DATABASE_URL not set");
            return;
        }
    };
    let db = TestDatabase::create(&bootstrap, &sqlx::migrate!("../cutover/migrations"))
        .await
        .unwrap();
    sqlx::raw_sql(include_str!(
        "../../cutover/tests/fixtures/member_erasure.sql"
    ))
    .execute(db.pool())
    .await
    .unwrap();
    let url = format!("postgres://agent_test:@agent-testdb:5432/{}", db.name());
    let dry_args = ["erase-member", "--guild", GUILD, "--user", USER];
    let execute_args = [
        "erase-member",
        "--guild",
        GUILD,
        "--user",
        USER,
        "--execute",
    ];
    let dry = cli(&dry_args, Some(&url), None).await;
    let dry_lines = table_lines(&dry, "DRY RUN: no changes");
    assert_eq!(
        dry_lines.len(),
        two_bot_cutover::member_erasure::plan().tables.len()
    );
    assert!(dry_lines.iter().all(|line| line.ends_with("\t1")));
    let execute = cli(&execute_args, Some(&url), Some("fixture-privacy-operator")).await;
    assert_eq!(table_lines(&execute, "ERASURE COMMITTED"), dry_lines);
    let second = cli(&execute_args, Some(&url), Some("fixture-privacy-operator")).await;
    let second_lines = table_lines(&second, "ERASURE COMMITTED");
    assert!(second_lines.iter().all(|line| line.ends_with("\t0")));
    let after = cli(&dry_args, Some(&url), None).await;
    assert_eq!(table_lines(&after, "DRY RUN: no changes"), second_lines);
    db.close().await.unwrap();
}
