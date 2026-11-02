//! Acceptance: bounded operator listings for fenced member rows
//! (`reconcile-member --list`) and unknown event intents (`reconcile-event
//! --list`).
//!
//! Protected test-first handoff: the paired Builder implements the bounds on top
//! of this branch without editing this file. Contract pinned here:
//!
//! - Both `--list` commands take an optional `--limit <n>` (positive integer;
//!   the Builder documents the exact max in `--help`, at least 30). Default 25.
//! - Non-positive, non-numeric or missing `--limit` values exit 2.
//! - When rows are capped, stdout ends with exactly one receipt line:
//!   `showing {shown} of {total} rows (truncated; rerun with --limit {total})`.
//! - At or under the limit, no `showing ...` line prints — even when the shown
//!   count equals the cap, the command must count before deciding.
//! - Claim tokens, hashes and row-internal column words never appear in stdout.
//!
//! Database tests use the authorized disposable test database only
//! (`TWO_TEST_DATABASE_URL`, `agent-testdb` principal): they skip with a
//! diagnostic when the URL is absent locally and fail closed in CI.

use std::process::Output;
use std::time::Duration;
use tokio::process::Command;
use two_bot_core::internal_action_store::{
    AuditSubject, DiscordId, InternalActionStore, InternalClaim, RequestIdentity,
};
use two_bot_core::member_moderation::MemberModerationStore;
use two_bot_core::member_moderation_store::PgMemberModerationStore;
use two_bot_testsupport::TestDatabase;

// TWO_STAGING_GUILD_ID: the reconcile CLIs are staging-guild only.
const GUILD: &str = "1545644954272137297";
const NOW: &str = "2023-11-14T22:13:20.000Z";
const DUE: &str = "2023-11-14T23:13:20.000Z";
const DEFAULT_LIMIT: usize = 25;

async fn cli(args: &[&str], database_url: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_two-bot"));
    command
        .env_clear()
        .env("TWO_DATABASE_TLS", "local-only")
        .args(args)
        .kill_on_drop(true);
    if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
        command.env("LLVM_PROFILE_FILE", profile);
    }
    if let Some(url) = database_url {
        command.env("TWO_DATABASE_URL", url);
    }
    tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .expect("operator command must exit, not start the gateway")
        .unwrap()
}

fn bootstrap_url(test: &str) -> Option<String> {
    match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure operator CLI acceptance"
            );
            eprintln!("SKIP {test}: TWO_TEST_DATABASE_URL not set");
            None
        }
    }
}

async fn fixture(bootstrap: &str) -> TestDatabase {
    TestDatabase::create(bootstrap, &sqlx::migrate!("../cutover/migrations"))
        .await
        .unwrap()
}

fn child_url(db: &TestDatabase) -> String {
    format!("postgres://agent_test:@agent-testdb:5432/{}", db.name())
}

fn member_user(i: usize) -> String {
    format!("1333333333333{i:05}")
}

async fn seed_prepared_member(store: &PgMemberModerationStore, request: &str, user: &str) {
    store
        .stage_unban(GUILD, user, NOW, "expiry", request, NOW)
        .await
        .expect("prepared intent stages");
}

fn event_subject() -> AuditSubject {
    AuditSubject {
        guild_id: Some(DiscordId::new(GUILD).unwrap()),
        actor_id: None,
        target_id: None,
        resolved_role_id: None,
    }
}

async fn seed_unknown_event(store: &InternalActionStore, key: &str, action: &str) {
    let id = RequestIdentity::new("website", key, action, b"{\"event_key\":\"launch\"}").unwrap();
    let claim = match store.claim(&id, &event_subject()).await.unwrap() {
        InternalClaim::Claimed(claim) => claim,
        other => panic!("expected committed execution claim, got {other:?}"),
    };
    store.mark_unknown(&claim).await.unwrap();
}

fn receipt(shown: usize, total: usize) -> String {
    format!("showing {shown} of {total} rows (truncated; rerun with --limit {total})")
}

fn stdout_lines(output: &Output) -> Vec<String> {
    assert!(
        output.status.success(),
        "list exits 0: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn member_help_documents_list_limit() {
    let help = cli(&["reconcile-member", "--help"], None).await;
    assert!(help.status.success());
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(
        text.contains("--limit"),
        "member --help names the list limit"
    );
    assert!(text.contains("25"), "member --help documents the default");
}

#[tokio::test]
async fn event_help_documents_list_limit() {
    let help = cli(&["reconcile-event", "--help"], None).await;
    assert!(help.status.success());
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(
        text.contains("--limit"),
        "event --help names the list limit"
    );
    assert!(text.contains("25"), "event --help documents the default");
}

#[tokio::test]
async fn member_limit_flag_parses_before_database_check() {
    // No TWO_DATABASE_URL: reaching the "required" error proves --limit parsed
    // and the staging allowlist passed; exit 2 would mean invalid arguments.
    let output = cli(
        &[
            "reconcile-member",
            "--guild",
            GUILD,
            "--list",
            "--limit",
            "5",
        ],
        None,
    )
    .await;
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("TWO_DATABASE_URL"),
        "parsed --limit reaches the database check"
    );
}

#[tokio::test]
async fn event_limit_flag_parses_before_database_check() {
    let output = cli(
        &[
            "reconcile-event",
            "--guild",
            GUILD,
            "--list",
            "--limit",
            "5",
        ],
        None,
    )
    .await;
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("TWO_DATABASE_URL"),
        "parsed --limit reaches the database check"
    );
}

#[tokio::test]
async fn limit_flag_rejects_non_positive_and_non_numeric() {
    for subcommand in ["reconcile-member", "reconcile-event"] {
        for value in ["0", "-5", "abc"] {
            let output = cli(
                &[subcommand, "--guild", GUILD, "--list", "--limit", value],
                None,
            )
            .await;
            assert_eq!(
                output.status.code(),
                Some(2),
                "{subcommand} --limit {value} is invalid"
            );
        }
        let missing = cli(&[subcommand, "--guild", GUILD, "--list", "--limit"], None).await;
        assert_eq!(
            missing.status.code(),
            Some(2),
            "{subcommand} --limit needs a value"
        );
    }
}

#[tokio::test]
async fn member_list_capped_prints_fixed_truncation_receipt() {
    let Some(bootstrap) = bootstrap_url("member capped list") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    let store = PgMemberModerationStore::new(db.pool().clone(), GUILD);
    for i in 0..30 {
        seed_prepared_member(&store, &format!("bound-prepared-{i:02}"), &member_user(i)).await;
    }
    // One running dispatch sorts first and proves tokens stay out of the report.
    let running = store
        .stage_unban(
            GUILD,
            &member_user(30),
            NOW,
            "expiry",
            "bound-00-running",
            NOW,
        )
        .await
        .expect("running intent stages");
    store
        .confirm_ban_attempt(GUILD, &member_user(30), "bound-00-running", running, NOW)
        .await
        .expect("running intent accepted");
    let jobs = store
        .claim_due_unbans(GUILD, DUE, 25)
        .await
        .expect("due claim");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].request_id, "bound-00-running");
    let token = jobs[0].claim_token.clone();
    let url = child_url(&db);
    let output = cli(
        &["reconcile-member", "--guild", GUILD, "--list"],
        Some(&url),
    )
    .await;
    let lines = stdout_lines(&output);
    assert_eq!(lines.len(), DEFAULT_LIMIT + 1, "25 rows plus the receipt");
    assert_eq!(lines[DEFAULT_LIMIT], receipt(25, 31));
    for line in &lines[..DEFAULT_LIMIT] {
        assert!(line.contains('\t'), "row line keeps the tab shape: {line}");
    }
    let text = lines.join("\n");
    assert!(!text.contains(token.as_str()), "claim token never prints");
    for internal in ["claim_token", "retry_generation", "dispatch_uncertain"] {
        assert!(
            !text.contains(internal),
            "row internal never prints: {internal}"
        );
    }
    db.close().await.unwrap();
}

#[tokio::test]
async fn member_list_at_or_under_limit_prints_no_receipt() {
    let Some(bootstrap) = bootstrap_url("member uncapped list") else {
        return;
    };
    for count in [3usize, 25] {
        let db = fixture(&bootstrap).await;
        let store = PgMemberModerationStore::new(db.pool().clone(), GUILD);
        for i in 0..count {
            seed_prepared_member(&store, &format!("bound-calm-{i:02}"), &member_user(i)).await;
        }
        let url = child_url(&db);
        let output = cli(
            &["reconcile-member", "--guild", GUILD, "--list"],
            Some(&url),
        )
        .await;
        let lines = stdout_lines(&output);
        assert_eq!(lines.len(), count, "uncapped list prints every row");
        assert!(
            lines.iter().all(|line| !line.starts_with("showing ")),
            "no truncation receipt at or under the limit"
        );
        db.close().await.unwrap();
    }
}

#[tokio::test]
async fn member_limit_override_bounds_output() {
    let Some(bootstrap) = bootstrap_url("member limit override") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    let store = PgMemberModerationStore::new(db.pool().clone(), GUILD);
    for i in 0..30 {
        seed_prepared_member(&store, &format!("bound-capped-{i:02}"), &member_user(i)).await;
    }
    let url = child_url(&db);
    let output = cli(
        &[
            "reconcile-member",
            "--guild",
            GUILD,
            "--list",
            "--limit",
            "5",
        ],
        Some(&url),
    )
    .await;
    let lines = stdout_lines(&output);
    assert_eq!(lines.len(), 6, "5 rows plus the receipt");
    assert_eq!(lines[5], receipt(5, 30));
    let full = cli(
        &[
            "reconcile-member",
            "--guild",
            GUILD,
            "--list",
            "--limit",
            "30",
        ],
        Some(&url),
    )
    .await;
    let full_lines = stdout_lines(&full);
    assert_eq!(full_lines.len(), 30, "override at the total shows all");
    assert!(
        full_lines.iter().all(|line| !line.starts_with("showing ")),
        "no receipt when the override covers the total"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn event_list_capped_prints_fixed_truncation_receipt() {
    let Some(bootstrap) = bootstrap_url("event capped list") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    let store = InternalActionStore::new(db.pool().clone());
    for i in 0..31 {
        let action = if i % 2 == 0 {
            "event.upsert"
        } else {
            "event.cancel"
        };
        seed_unknown_event(&store, &format!("bound-list:{i}"), action).await;
    }
    let url = child_url(&db);
    let output = cli(&["reconcile-event", "--guild", GUILD, "--list"], Some(&url)).await;
    let lines = stdout_lines(&output);
    assert_eq!(lines.len(), DEFAULT_LIMIT + 1, "25 rows plus the receipt");
    assert_eq!(lines[DEFAULT_LIMIT], receipt(25, 31));
    for line in &lines[..DEFAULT_LIMIT] {
        assert!(line.contains('\t'), "row line keeps the tab shape: {line}");
        assert!(
            line.contains("event.upsert") || line.contains("event.cancel"),
            "row names its verb: {line}"
        );
    }
    let text = lines.join("\n");
    let hash: String = sqlx::query_scalar("SELECT key_hash FROM internal_idempotency LIMIT 1")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert!(!text.contains(hash.as_str()), "row hashes never print");
    for internal in ["caller_hash", "key_hash", "payload_hash", "idempotency_key"] {
        assert!(
            !text.contains(internal),
            "row internal never prints: {internal}"
        );
    }
    db.close().await.unwrap();
}

#[tokio::test]
async fn event_list_at_or_under_limit_prints_no_receipt() {
    let Some(bootstrap) = bootstrap_url("event uncapped list") else {
        return;
    };
    for count in [3usize, 25] {
        let db = fixture(&bootstrap).await;
        let store = InternalActionStore::new(db.pool().clone());
        for i in 0..count {
            seed_unknown_event(&store, &format!("bound-calm:{i}"), "event.upsert").await;
        }
        let url = child_url(&db);
        let output = cli(&["reconcile-event", "--guild", GUILD, "--list"], Some(&url)).await;
        let lines = stdout_lines(&output);
        assert_eq!(lines.len(), count, "uncapped list prints every row");
        assert!(
            lines.iter().all(|line| !line.starts_with("showing ")),
            "no truncation receipt at or under the limit"
        );
        db.close().await.unwrap();
    }
}

#[tokio::test]
async fn event_limit_override_bounds_output() {
    let Some(bootstrap) = bootstrap_url("event limit override") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    let store = InternalActionStore::new(db.pool().clone());
    for i in 0..30 {
        seed_unknown_event(&store, &format!("bound-capped:{i}"), "event.upsert").await;
    }
    let url = child_url(&db);
    let output = cli(
        &[
            "reconcile-event",
            "--guild",
            GUILD,
            "--list",
            "--limit",
            "5",
        ],
        Some(&url),
    )
    .await;
    let lines = stdout_lines(&output);
    assert_eq!(lines.len(), 6, "5 rows plus the receipt");
    assert_eq!(lines[5], receipt(5, 30));
    let full = cli(
        &[
            "reconcile-event",
            "--guild",
            GUILD,
            "--list",
            "--limit",
            "30",
        ],
        Some(&url),
    )
    .await;
    let full_lines = stdout_lines(&full);
    assert_eq!(full_lines.len(), 30, "override at the total shows all");
    assert!(
        full_lines.iter().all(|line| !line.starts_with("showing ")),
        "no receipt when the override covers the total"
    );
    db.close().await.unwrap();
}
