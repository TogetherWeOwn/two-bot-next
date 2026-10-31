//! Acceptance proof that the presence-store database fixtures route through
//! the shared disposable-database guard (`two_bot_testsupport::TestDatabase`)
//! instead of a bespoke local connection guard.
//!
//! Two halves:
//!
//! - Guard controls: the shared validator the helper must delegate to
//!   refuses loopback (even with CI flags), wrong principal/port/database,
//!   credentials, query overrides and inherited libpq `PG*` settings before
//!   any connection, using synthetic URLs only (zero network on rejection),
//!   while a valid synthetic `agent-testdb` URL stays accepted.
//! - Delegation: the `presence_store` test module itself must build its
//!   fixture with `TestDatabase::create`, close it explicitly so teardown
//!   failures surface, and carry no hand-rolled connect/DDL path of its own.
//!   These assertions fail until the bespoke helper is replaced; retesting
//!   the validator alone would not prove the presence helper delegates.
//!
//! Run: `python3 scripts/cargo_cache.py run -- test -p two-bot-core
//! --features db --test presence_fixture_guard_acceptance`

use two_bot_testsupport::guard_database_url;

const VALID: &str = "postgres://agent_test:@agent-testdb:5432/two_bot_test_local";

const PRESENCE_STORE: &str = include_str!("../src/presence_store.rs");

/// The `#[cfg(test)]` module of `presence_store.rs` (it ends the file).
fn presence_test_module() -> &'static str {
    let start = PRESENCE_STORE
        .find("#[cfg(test)]")
        .expect("presence_store keeps a cfg(test) module");
    &PRESENCE_STORE[start..]
}

// ---------------------------------------------------------------------------
// Guard controls: the contract the presence helper must inherit.
// ---------------------------------------------------------------------------

#[test]
fn valid_synthetic_agent_testdb_url_is_accepted() {
    guard_database_url(VALID).expect("valid synthetic agent-testdb URL accepted");
    guard_database_url("postgresql://agent_test:@agent-testdb:5432/two_bot_test_presence_1")
        .expect("postgresql scheme with a guarded database name accepted");
}

#[test]
fn loopback_is_refused_even_where_ci_would_allow_it() {
    // The shared guard takes no CI-flag input at all: there is no
    // GITHUB_ACTIONS-style carve-out to re-enable loopback.
    for host in ["localhost", "127.0.0.1", "[::1]"] {
        let url = format!("postgres://agent_test:@{host}:5432/two_bot_test_local");
        assert!(
            guard_database_url(&url).is_err(),
            "loopback host must be refused: {host}"
        );
    }
}

#[test]
fn wrong_principal_port_and_database_are_refused() {
    for url in [
        // Wrong principal.
        "postgres://postgres:@agent-testdb:5432/two_bot_test_local",
        "postgres://agent:@agent-testdb:5432/two_bot_test_local",
        // Wrong or missing explicit port.
        "postgres://agent_test:@agent-testdb:5433/two_bot_test_local",
        "postgres://agent_test:@agent-testdb/two_bot_test_local",
        // Bootstrap-shaped names only: shared, production-shaped or
        // malformed database names never reach a connection.
        "postgres://agent_test:@agent-testdb:5432/postgres",
        "postgres://agent_test:@agent-testdb:5432/two_bot",
        "postgres://agent_test:@agent-testdb:5432/two_bot_test_",
        "postgres://agent_test:@agent-testdb:5432/two_bot_test_Local",
        "postgres://agent_test:@agent-testdb:5432/two_bot_test_local_",
        // Wrong host outright.
        "postgres://agent_test:@production:5432/two_bot_test_local",
        "postgres://agent_test:@staging:5432/two_bot_test_local",
    ] {
        assert!(
            guard_database_url(url).is_err(),
            "unsafe fixture URL must be refused: {url}"
        );
    }
}

#[test]
fn credentials_are_refused() {
    for url in [
        "postgres://agent_test:secret@agent-testdb:5432/two_bot_test_local",
        // No explicit empty password: authority must be `agent_test:@`.
        "postgres://agent_test@agent-testdb:5432/two_bot_test_local",
        "postgres://%61gent_test:@agent-testdb:5432/two_bot_test_local",
    ] {
        assert!(
            guard_database_url(url).is_err(),
            "credentialed fixture URL must be refused: {url}"
        );
    }
}

#[test]
fn query_overrides_and_fragments_are_refused() {
    for suffix in [
        "?host=production",
        "?hostaddr=10.0.0.1",
        "?port=5433",
        "?dbname=production",
        "?user=postgres",
        "?options=-csearch_path=public",
        "?sslmode=disable",
        "?service=production",
        "?",
        "#fragment",
    ] {
        let url = format!("{VALID}{suffix}");
        assert!(
            guard_database_url(&url).is_err(),
            "query/fragment override must be refused: {suffix}"
        );
    }
}

#[test]
fn refusal_errors_never_echo_the_url_or_credentials() {
    let error = guard_database_url(
        "postgres://agent_test:private_password@agent-testdb:5432/two_bot_test_local",
    )
    .expect_err("credentialed URL refused")
    .to_string();
    assert!(!error.contains("private_password"), "error leaks secret");
    assert!(!error.contains("postgres://"), "error echoes URL");
}

/// Child-process target: with the marker set, a valid URL must be refused
/// purely because an inherited libpq `PG*` variable is present. Without the
/// marker this is a no-op so the parent run stays green.
#[test]
fn libpq_child_refuses_valid_url_when_pg_env_present() {
    if std::env::var("PRESENCE_FIXTURE_GUARD_CHILD").as_deref() != Ok("1") {
        return;
    }
    let error = guard_database_url(VALID)
        .expect_err("inherited PG* settings must refuse the valid URL")
        .to_string();
    assert!(
        error.contains("PG*"),
        "refusal must name the libpq cause, got: {error}"
    );
}

#[test]
fn inherited_libpq_settings_are_refused_in_a_child_process() {
    // Process-isolated: the parent's environment (and sibling tests) never
    // see PG* mutations. Each child runs only the marker test above.
    let exe = std::env::current_exe().expect("test binary path");
    for var in ["PGHOST", "PGOPTIONS", "PGPASSWORD", "PGDATABASE"] {
        let output = std::process::Command::new(&exe)
            .args([
                "libpq_child_refuses_valid_url_when_pg_env_present",
                "--exact",
                "--nocapture",
            ])
            .env("PRESENCE_FIXTURE_GUARD_CHILD", "1")
            .env(var, "production")
            .output()
            .expect("spawn child test process");
        assert!(
            output.status.success(),
            "child with {var} set must refuse the valid URL; stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// The fixture constructor the presence helper must call refuses unsafe
/// URLs before opening any connection (the guard runs ahead of connect in
/// `TestDatabase::create`), so a configured invalid URL fails fast and
/// offline instead of skipping or dialing.
#[cfg(feature = "db")]
#[tokio::test]
async fn test_database_create_refuses_unsafe_urls_before_connecting() {
    let migrations = sqlx::migrate!("../cutover/migrations");
    for url in [
        "postgres://agent_test:@localhost:5432/two_bot_test_local",
        "postgres://agent_test:secret@agent-testdb:5432/two_bot_test_local",
        "postgres://agent_test:@agent-testdb:5432/postgres",
        "postgres://agent_test:@agent-testdb:5432/two_bot_test_local?host=production",
    ] {
        let error = match two_bot_testsupport::TestDatabase::create(url, &migrations).await {
            Ok(_) => panic!("unsafe URL must be refused before connecting: {url}"),
            Err(error) => error.to_string(),
        };
        assert!(
            !error.contains("secret"),
            "create refusal must not echo credentials: {error}"
        );
    }
}

// ---------------------------------------------------------------------------
// Delegation: the presence helper itself, not just the shared validator.
// ---------------------------------------------------------------------------

#[test]
fn presence_fixture_delegates_to_the_shared_test_database() {
    let module = presence_test_module();
    assert!(
        module.contains("two_bot_testsupport"),
        "presence fixtures must import the shared testsupport fixture"
    );
    assert!(
        module.contains("TestDatabase::create"),
        "presence fixtures must build their database via TestDatabase::create"
    );
}

#[test]
fn presence_fixture_has_no_bespoke_connection_or_schema_path() {
    let module = presence_test_module();
    for banned in [
        // Hand-parsed connect options with a local host/user allowlist.
        "PgConnectOptions",
        "PgPoolOptions",
        // The loopback-with-CI-flags carve-out the shared guard forbids.
        "GITHUB_ACTIONS",
        "localhost",
        // Hand-rolled fixture DDL: the shared fixture owns database
        // creation and teardown, and never touches the bootstrap database.
        "CREATE SCHEMA",
        "DROP SCHEMA",
        "CREATE DATABASE",
        "DROP DATABASE",
        "search_path",
    ] {
        assert!(
            !module.contains(banned),
            "presence fixture must not carry a bespoke path containing `{banned}`"
        );
    }
}

#[test]
fn presence_fixture_close_is_explicit_and_reports_cleanup_failure() {
    let module = presence_test_module();
    assert!(
        module.contains(".close().await.expect") || module.contains(".close().await?"),
        "each presence test must await the shared fixture's explicit close \
         and surface teardown failure, not rely on drop/best-effort cleanup"
    );
}

#[test]
fn presence_fixture_keeps_absent_url_skip_and_fails_a_configured_bad_url() {
    let module = presence_test_module();
    assert!(
        module.contains("NotPresent"),
        "an absent TWO_TEST_DATABASE_URL must keep the documented skip"
    );
    assert!(
        module.contains("TWO_TEST_DATABASE_URL"),
        "presence fixtures read only the opt-in test URL"
    );
    // A configured URL flows into the guarded constructor; its refusal is
    // an error the test propagates (see
    // test_database_create_refuses_unsafe_urls_before_connecting), never a
    // silent skip branch.
    assert!(
        module.contains("TestDatabase::create"),
        "a configured URL must go through the guarded constructor"
    );
}

#[test]
fn all_four_presence_behavior_tests_are_retained() {
    let module = presence_test_module();
    for name in [
        "probe_cycle_round_trips_through_postgres",
        "truncated_scan_round_trips_and_delays_the_next_attempt",
        "windowed_series_read_returns_only_rows_inside_the_bound",
        "stored_series_feeds_the_reopen_trigger",
    ] {
        assert!(
            module.contains(&format!("async fn {name}")),
            "presence behavior test `{name}` must be retained with its assertions"
        );
    }
}
