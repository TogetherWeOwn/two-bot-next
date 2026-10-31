//! Acceptance: the backup restore pool-open path returns a typed error for a
//! missing URL instead of relying on `Option::expect` (reliability P7).
//!
//! BUILDER CONTRACT: this file is frozen. Build on the test card's branch,
//! keep every test here verbatim, and make the suite green by changing
//! production code only:
//!
//! - Add `pub(crate) async fn open_restore_pool(url: Option<&str>) ->
//!   Result<sqlx::PgPool, &'static str>` to `crates/bot/src/backup_cli.rs`.
//! - `None` returns `Err("missing restore database URL")` — a fixed,
//!   non-echoing constant in the same `&'static str` failure type as
//!   `open_pool`. It must not panic.
//! - `Some(url)` delegates to the existing `open_pool(url)` unchanged:
//!   byte-identical error constants, TLS policy and connection behavior.
//! - In `cmd_restore`, replace
//!   `open_pool(url.as_ref().expect("checked"))` with
//!   `open_restore_pool(url.as_deref())`, keeping the existing `Err` arm
//!   (`eprintln!("restore: {detail}")`, return 1) untouched. The exit-2
//!   guard for a missing/invalid-scheme `TWO_RESTORE_URL` stays as is; the
//!   `None` arm is the defense-in-depth typed failure behind it.
//! - No new env vars, secrets, dependencies or connection behavior. Do not
//!   edit this file or the `#[cfg(test)] mod backup_cli_pool_open_tests;`
//!   wiring in `main.rs`.
//!
//! Suite map (all offline: no DB, no network beyond a loopback listener
//! that must never be reached):
//!
//! - missing-URL pin (RED now: `open_restore_pool` does not exist; the
//!   restore path still carries the latent `expect("checked")`).
//! - malformed-URL pin: the `Some` path keeps `open_pool`'s exact
//!   `invalid database URL` constant and never echoes the input.
//! - unknown-query-secret pin: the `Some` path is refused before any
//!   socket, with `open_pool`'s exact constant (green path alongside).
//! - wiring pin: `cmd_restore` opens its pool through
//!   `open_restore_pool(url.as_deref())` and contains no `expect`.

use crate::backup_cli::open_restore_pool;

/// Expected typed constant for a missing restore URL. Fixed and
/// non-echoing, in `open_pool`'s `&'static str` failure type.
const MISSING_URL_ERROR: &str = "missing restore database URL";

#[tokio::test]
async fn restore_pool_open_none_url_returns_typed_constant_not_panic() {
    let error = open_restore_pool(None).await.unwrap_err();
    assert_eq!(error, MISSING_URL_ERROR);
    // Deterministic: the same fixed constant on a repeat call, with no
    // state carried between calls.
    let again = open_restore_pool(None).await.unwrap_err();
    assert_eq!(again, MISSING_URL_ERROR);
}

#[tokio::test]
async fn restore_pool_open_some_malformed_url_returns_invalid_constant() {
    let error = open_restore_pool(Some("fixture-marker not a url"))
        .await
        .unwrap_err();
    assert_eq!(error, "invalid database URL");
    assert!(!error.contains("fixture-marker"), "refusal echoed the URL");
}

#[tokio::test]
async fn restore_pool_open_some_unknown_query_secret_refused_without_network() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let url = format!(
        "postgres://fixture-user:fixture-password@127.0.0.1:{port}/db?api_key=fixture-restore-secret"
    );
    let error = open_restore_pool(Some(&url)).await.unwrap_err();
    assert_eq!(error, "unsupported database URL parameter");
    assert!(!error.contains("fixture"), "refusal echoed the URL");
    // The Some path must stay the pre-socket `open_pool` validation:
    // nothing may reach the listener, not even one connection attempt.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
}

#[test]
fn restore_pool_open_wiring_cmd_restore_uses_typed_helper_without_expect() {
    let source =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/backup_cli.rs")).unwrap();
    let start = source
        .find("async fn cmd_restore(args")
        .expect("cmd_restore must exist in backup_cli.rs");
    let end = source[start..]
        .find("fn warn_missing_dump_tables")
        .map(|offset| start + offset)
        .expect("cmd_restore segment must end before warn_missing_dump_tables");
    let segment = &source[start..end];
    assert!(
        segment.contains("open_restore_pool(url.as_deref())"),
        "cmd_restore must open its pool via open_restore_pool(url.as_deref()):\n{segment}"
    );
    assert!(
        !segment.contains(".expect("),
        "cmd_restore must not rely on expect for the restore URL:\n{segment}"
    );
}
