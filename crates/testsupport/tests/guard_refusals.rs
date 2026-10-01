use two_bot_testsupport::guard_database_url;

const SAFE: &str = "postgres://agent_test:@agent-testdb:5432/two_bot_test_guard";

/// Verbatim refusal vectors from website_contract and internal_action_store.
/// Slice-specific database allowlists are replaced by the shared test prefix;
/// the former generic agent_test database is now deliberately refused too.
#[test]
fn preserves_migrated_guard_refusal_cases() {
    for url in [
        "postgres://user@production/two_bot",
        "postgres://test_runner@production/real_data",
        "postgres://agent_test:@production/two_bot_test_tog10090",
        "postgres://agent_test:@agent-testdb/real_data",
        "postgres://agent_test:@agent-testdb/test",
        "postgres://agent_test:@agent-testdb/postgres",
        "postgres://agent_test:@agent-testdb/two_bot_test_tog10090_",
        "postgres://agent_test:@agent-testdb/two_bot_test_tog10090-unsafe",
        "postgres://agent_test:@agent-testdb:5433/two_bot_test_tog10090",
        "postgres://agent_test:@agent-testdb.example/two_bot_test_tog10090",
        "postgres://agent_test:@agent-testdb/two_bot_test_tog10090?host=production",
        "postgres://agent_test:@agent-testdb/two_bot_test_tog10090?hostaddr=127.0.0.1",
        "postgres://agent_test:@agent-testdb/two_bot_test_tog10090?options=-csearch_path=test",
        "postgres://agent_test:@production/real_data?application_name=test",
        "postgres://agent_test:@%2Fvar%2Frun%2Fpostgresql/two_bot_test_tog10090",
        "postgres://agent_test:unexpected@agent-testdb/two_bot_test_tog10090",
        "postgres://agent_test:@agent-testdb/two_bot_test_tog10090#test",
        "postgres://agent_test:@agent-testdb/",
        "postgres://agent-testdb/two_bot_test_tog10090",
        "not a URL",
        "postgres://agent_test@agent-testdb:5432/agent_test",
        "postgres://agent_test:other@agent-testdb:5432/agent_test",
        "postgres://agent_test:@staging:5432/agent_test",
        "postgres://agent_test:@agent-testdb:5433/agent_test",
        "postgres://other:@agent-testdb:5432/agent_test",
        "postgres://agent_test:@agent-testdb:5432/production",
        "postgres://agent_test:@agent-testdb:5432/agent_test?hostaddr=127.0.0.1",
        "postgres://agent_test:@agent-testdb:5432/agent_test#fragment",
        "postgres://agent_test:@agent-testdb:5432/agent_test",
        // Existing sticky guard refusal vectors remain covered centrally even
        // while the sticky live suite still uses its original harness.
        "postgres://agent_test@production.example/testdb",
        "postgres://agent_test:localhost@production.example/realdb",
        "postgres://agent_test:@agent-testdb.production.example/agent_test",
        "postgres://agent_test:@localhost/agent_test",
        "postgres://agent_test:@127.0.0.1/agent_test",
        "postgres://agent_test:@agent-testdb/agent_test?host=production.example",
        "postgres://agent_test:@agent-testdb/agent_test?hostaddr=192.0.2.1",
        "postgres://agent_test:@agent-testdb/agent_test?host=/var/run/postgresql",
        "postgres://other_user:@agent-testdb/agent_test",
        "postgres://agent_test:@agent-testdb/production",
        "postgres://agent_test:@agent-testdb/agent_test?user=other_user",
        "postgres://agent_test:@agent-testdb/agent_test?dbname=production",
        "postgres://agent_test:@agent-testdb/agent_test?port=5433",
        "postgres://agent_test:fixture-password@agent-testdb/agent_test",
    ] {
        assert!(guard_database_url(url).is_err(), "unsafe target accepted");
    }
    // Same refusal classes with an otherwise valid URL: no wrong-name or
    // missing-port failure can accidentally mask a redirect/credential bug.
    for query in [
        "host=/var/run/postgresql",
        "hostaddr=127.0.0.1",
        "host=agent-testdb",
        "port=5432",
        "dbname=two_bot_test_other",
        "user=agent_test",
        "password=",
    ] {
        assert!(guard_database_url(&format!("{SAFE}?{query}")).is_err());
    }
    for scheme in ["postgres", "postgresql"] {
        assert!(guard_database_url(&format!(
            "{scheme}://agent_test:@agent-testdb:5432/two_bot_test_tog10090"
        ))
        .is_ok());
    }
}

/// An isolated child is essential: mutating PG* in this parallel test process
/// would race other fixtures. No child opens a connection or prints values.
#[test]
fn inherited_configuration_child() {
    if std::env::var_os("TESTSUPPORT_GUARD_PROBE").is_some() {
        assert!(guard_database_url(SAFE).is_err());
    }
}

#[test]
fn rejects_actual_inherited_configuration_before_connecting() {
    for name in [
        "PGHOST",
        "PGHOSTADDR",
        "PGPORT",
        "PGUSER",
        "PGPASSWORD",
        "PGDATABASE",
        "PGOPTIONS",
        "PGSERVICE",
        "PGSERVICEFILE",
        "PGPASSFILE",
        "PGSSLMODE",
        "PGSSLROOTCERT",
        "PGSSLCERT",
        "PGSSLKEY",
        "PGAPPNAME",
    ] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["inherited_configuration_child", "--exact"])
            .env_clear()
            .env("TESTSUPPORT_GUARD_PROBE", "1")
            .env(name, "synthetic-sentinel-never-a-credential")
            .output()
            .expect("run isolated guard probe");
        assert!(output.status.success(), "guard probe failed for {name}");
    }
    guard_database_url(SAFE).unwrap();
}

#[test]
fn host_allowlist_child() {
    if std::env::var_os("TESTSUPPORT_HOST_PROBE").is_some() {
        for scheme in ["postgres", "postgresql"] {
            guard_database_url(&format!(
                "{scheme}://agent_test:@agent-testdb:5432/two_bot_test_guard"
            ))
            .unwrap();
            assert!(guard_database_url(&format!(
                "{scheme}://agent_test:@127.0.0.1:5432/two_bot_test_guard"
            ))
            .is_err());
        }
    }
}

#[test]
fn loopback_is_refused_with_and_without_ci_flags() {
    for ci in [false, true] {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args(["host_allowlist_child", "--exact"])
            .env_clear()
            .env("TESTSUPPORT_HOST_PROBE", "1");
        if ci {
            child
                .env("CI", "true")
                .env("GITHUB_ACTIONS", "true")
                .env("TWO_LEVELING_TEST_CI", "1");
        }
        let output = child.output().expect("run isolated host allowlist probe");
        assert!(output.status.success(), "host probe failed (CI={ci})");
    }
}
