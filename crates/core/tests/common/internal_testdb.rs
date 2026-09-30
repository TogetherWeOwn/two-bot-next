//! Shared internal-action test-container guard. Never consult another credential.
use sqlx::postgres::PgConnectOptions;
use sqlx::ConnectOptions;
use std::str::FromStr;

pub fn test_options(url: &str) -> Result<PgConnectOptions, &'static str> {
    if url.contains(['?', '#']) {
        return Err("no test URL overrides");
    }
    if !(url.starts_with("postgres://agent_test:@") || url.starts_with("postgresql://agent_test:@"))
    {
        return Err("explicit agent_test empty password required");
    }
    let options = PgConnectOptions::from_str(url).map_err(|_| "invalid test URL")?;
    if options.get_host() != "agent-testdb"
        || options.get_port() != 5432
        || options.get_socket().is_some()
        || options.get_username() != "agent_test"
        || options.get_options().is_some()
        || options.get_database() != Some("agent_test")
        || options
            .to_url_lossy()
            .password()
            .is_some_and(|password| !password.is_empty())
    {
        return Err("test-container target only");
    }
    // sqlx may otherwise fall back to inherited PGPASSWORD or .pgpass.
    Ok(options.password(""))
}
