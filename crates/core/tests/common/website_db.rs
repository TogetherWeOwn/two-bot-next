//! Shared guard for website-contract tests. Never consume DATABASE_URL.

use sqlx::postgres::PgConnectOptions;
use std::str::FromStr;

pub fn test_db_options(url: &str) -> Result<PgConnectOptions, &'static str> {
    // An explicit empty password prevents sqlx from falling back to PG env or
    // pgpass credentials. Reject query/fragment overrides rather than letting
    // hostaddr, socket or options redirect the destructive reset.
    if url.contains(['?', '#']) {
        return Err("test URL must not have query parameters or a fragment");
    }
    let url = if let Some(rest) = url.strip_prefix("postgres://agent_test@") {
        format!("postgres://agent_test:@{rest}")
    } else if let Some(rest) = url.strip_prefix("postgresql://agent_test@") {
        format!("postgresql://agent_test:@{rest}")
    } else if url.starts_with("postgres://agent_test:@")
        || url.starts_with("postgresql://agent_test:@")
    {
        url.to_owned()
    } else {
        return Err("test URL must explicitly use agent_test with an empty password");
    };
    // Inspect parsed fields, never substrings in credentials or URL parameters.
    // https://docs.rs/sqlx/0.9.0/sqlx/postgres/struct.PgConnectOptions.html
    let options = PgConnectOptions::from_str(&url).map_err(|_| "invalid test URL")?;
    if options.get_host() != "agent-testdb"
        || options.get_port() != 5432
        || options.get_socket().is_some()
        || options.get_username() != "agent_test"
        || options.get_options().is_some()
    {
        return Err("test target must be agent-testdb:5432 without socket/startup overrides");
    }
    let database = options.get_database().ok_or("scratch database required")?;
    let prefix = "two_bot_test_tog10090";
    let allowed_name = database == prefix
        || database
            .strip_prefix(prefix)
            .and_then(|s| s.strip_prefix('_'))
            .is_some_and(|suffix| !suffix.is_empty());
    if !allowed_name
        || database.len() > 63
        || !database
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err("database must be the isolated two_bot_test_tog10090 scratch target");
    }
    Ok(options)
}
