//! Validate before SQLx parses a credential-bearing URL. SQLx 0.9 logs both
//! key and value for unrecognized query parameters at WARN, even if parsing
//! later fails. An error sanitizer cannot retract that dependency diagnostic.

/// Reject unsupported parameters without passing their values to SQLx or logs.
/// Supported keys mirror the pinned sqlx-postgres 0.9 URL parser; re-audit this
/// allowlist when upgrading SQLx. Known parameter values remain SQLx's concern.
pub fn validate(raw: &str) -> Result<(), &'static str> {
    let url = url::Url::parse(raw).map_err(|_| "invalid database URL")?;
    for (key, _) in url.query_pairs() {
        let supported = matches!(
            key.as_ref(),
            "sslmode"
                | "ssl-mode"
                | "sslrootcert"
                | "ssl-root-cert"
                | "ssl-ca"
                | "sslcert"
                | "ssl-cert"
                | "sslkey"
                | "ssl-key"
                | "statement-cache-capacity"
                | "host"
                | "hostaddr"
                | "port"
                | "dbname"
                | "user"
                | "password"
                | "application_name"
                | "options"
        ) || (key.starts_with("options[") && key.ends_with(']'));
        if !supported {
            return Err("unsupported database URL parameter");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_sqlx_query_keys_remain_accepted() {
        for key in [
            "sslmode",
            "ssl-mode",
            "sslrootcert",
            "ssl-root-cert",
            "ssl-ca",
            "sslcert",
            "ssl-cert",
            "sslkey",
            "ssl-key",
            "statement-cache-capacity",
            "host",
            "hostaddr",
            "port",
            "dbname",
            "user",
            "password",
            "application_name",
            "options",
            "options[statement_timeout]",
        ] {
            assert!(validate(&format!("postgres://agent-testdb/db?{key}=fixture")).is_ok());
        }
    }

    #[test]
    fn unknown_query_keys_fail_without_echoing_keys_or_values() {
        for query in [
            "api_key=fixture-query-secret&sslmode=invalid",
            "fixture-key-secret=fixture-value-secret",
            "api%5Fkey=fixture-query-secret",
            "options[bad=fixture-query-secret",
        ] {
            let error = validate(&format!("postgres://agent-testdb/db?{query}")).unwrap_err();
            assert_eq!(error, "unsupported database URL parameter");
            assert!(!error.contains("fixture"));
        }
    }
}
