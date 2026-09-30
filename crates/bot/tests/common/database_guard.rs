//! Shared opt-in guard for runner and real-binary gateway tests.
//! Never read the deployed runtime DATABASE_URL.

use std::str::FromStr;

use sqlx::postgres::PgConnectOptions;

pub fn test_options() -> PgConnectOptions {
    let url = std::env::var("TWO_GATEWAY_TEST_DATABASE_URL")
        .expect("set the dedicated test URL; runtime DATABASE_URL is never used");
    guarded_options(&url)
}

fn guarded_options(url: &str) -> PgConnectOptions {
    let options = PgConnectOptions::from_str(url).expect("test URL");
    assert!(options.get_socket().is_none(), "test URL must use TCP");
    assert!(matches!(
        options.get_host(),
        "agent-testdb" | "localhost" | "127.0.0.1"
    ));
    assert_eq!(options.get_username(), "agent_test");
    assert_eq!(options.get_database(), Some("agent_test"));
    options
}

#[test]
fn accepts_only_disposable_gateway_databases() {
    for host in ["agent-testdb", "localhost", "127.0.0.1"] {
        guarded_options(&format!("postgres://agent_test@{host}/agent_test"));
    }
    for url in [
        "postgres://agent_test@staging.example/agent_test",
        "postgres://agent_test@prod.example/agent_test",
        "postgres://postgres@agent-testdb/agent_test",
        "postgres://agent_test@agent-testdb/two_bot",
        "postgres://agent_test@agent-testdb/agent_test?host=%2Fsome%2Fsocket",
        "postgres://agent_test@127.0.0.1/agent_test?host=/var/run/postgresql",
    ] {
        assert!(std::panic::catch_unwind(|| guarded_options(url)).is_err());
    }
}
