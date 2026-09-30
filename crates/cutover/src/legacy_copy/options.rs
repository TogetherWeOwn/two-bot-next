//! Strict CLI parsing and a fail-closed target fence. No URL in diagnostics.

use super::{mapping, Table};
use sqlx::postgres::PgConnectOptions;
use sqlx::ConnectOptions;
use std::str::FromStr;

#[derive(Default)]
pub struct Options {
    pub apply: bool,
    pub allow_live_target: bool,
    pub plan: bool,
    pub help: bool,
    pub groups: Vec<String>,
    pub batch_size: u32,
    source_url: Option<String>,
    target_url: Option<String>,
}

impl Options {
    pub fn parse(argv: &[String]) -> Result<Self, &'static str> {
        let mut out = Self {
            groups: vec!["ready".to_owned()],
            batch_size: 500,
            ..Self::default()
        };
        let mut seen = std::collections::HashSet::new();
        let mut args = argv.iter();
        while let Some(arg) = args.next() {
            let (name, inline) = arg
                .split_once('=')
                .map_or((arg.as_str(), None), |(n, v)| (n, Some(v)));
            if !seen.insert(name.to_owned()) {
                return Err("duplicate argument");
            }
            match name {
                "--apply" | "--allow-live-target" | "--plan" | "--help" => {
                    if inline.is_some() {
                        return Err("boolean flags take no value");
                    }
                    match name {
                        "--apply" => out.apply = true,
                        "--allow-live-target" => out.allow_live_target = true,
                        "--plan" => out.plan = true,
                        _ => out.help = true,
                    }
                }
                "--source-url" | "--target-url" | "--groups" | "--batch-size" => {
                    let value = inline
                        .or_else(|| args.next().map(String::as_str))
                        .filter(|v| !v.is_empty() && !v.starts_with("--"))
                        .ok_or("missing argument value")?;
                    match name {
                        "--source-url" => out.source_url = Some(value.to_owned()),
                        "--target-url" => out.target_url = Some(value.to_owned()),
                        "--groups" => {
                            out.groups = value.split(',').map(str::to_owned).collect();
                            if out.groups.iter().any(String::is_empty) {
                                return Err("empty table group");
                            }
                        }
                        _ => {
                            out.batch_size = value.parse().map_err(|_| "invalid batch size")?;
                            if !(1..=10_000).contains(&out.batch_size) {
                                return Err("batch size must be between 1 and 10000");
                            }
                        }
                    }
                }
                _ => return Err("unknown argument (see --help)"),
            }
        }
        if out.plan && out.apply {
            return Err("--plan cannot be combined with --apply");
        }
        Ok(out)
    }

    pub fn tables(&self) -> Result<Vec<Table>, String> {
        mapping::select(&self.groups)
    }

    /// Only the tool-specific variables are read; never TWO_DATABASE_URL,
    /// DATABASE_URL, production bindings, or fallback credentials.
    pub fn connections(&self) -> Result<(PgConnectOptions, PgConnectOptions), &'static str> {
        let source_url = self
            .source_url
            .clone()
            .or_else(|| std::env::var("LEGACY_COPY_SOURCE_URL").ok())
            .ok_or("source URL required (--source-url or LEGACY_COPY_SOURCE_URL)")?;
        let target_url = self
            .target_url
            .clone()
            .or_else(|| std::env::var("LEGACY_COPY_TARGET_URL").ok())
            .ok_or("target URL required (--target-url or LEGACY_COPY_TARGET_URL)")?;
        let source = explicit_connection(&source_url)?;
        let target = guarded_target(&target_url, self.allow_live_target)?;
        if source.get_host() == target.get_host()
            && source.get_port() == target.get_port()
            && source.get_database() == target.get_database()
        {
            return Err("source and target must be different databases");
        }
        Ok((source, target))
    }
}

fn explicit_connection(url: &str) -> Result<PgConnectOptions, &'static str> {
    let rest = url
        .strip_prefix("postgres://")
        .or_else(|| url.strip_prefix("postgresql://"))
        .ok_or("Postgres URL required")?;
    let (credentials, endpoint) = rest
        .split_once('@')
        .ok_or("explicit user and password required")?;
    let (user, password) = credentials
        .split_once(':')
        .ok_or("explicit user and password required (empty allowed)")?;
    if user.is_empty() || endpoint.split('/').next().is_none_or(str::is_empty) {
        return Err("explicit host and user required");
    }
    let options = PgConnectOptions::from_str(url).map_err(|_| "invalid Postgres URL")?;
    if options.get_database().is_none_or(str::is_empty)
        || options.get_socket().is_some()
        || options.get_options().is_some()
    {
        return Err("explicit database required; sockets/startup overrides forbidden");
    }
    // Explicit empty means trust-authentication, not PGPASSWORD or ~/.pgpass.
    // Source: docs.rs/sqlx/0.9.0/sqlx/postgres/struct.PgConnectOptions.html
    Ok(if password.is_empty() {
        options.password("")
    } else {
        options
    }
    .disable_statement_logging())
}

/// Everything not proven disposable is treated as live, including staging and
/// loopback. Matching a 'test' substring or Neon hostname is never sufficient.
/// This flag is an operator fence, NOT authorization to execute on live data.
pub fn guarded_target(url: &str, allow_live: bool) -> Result<PgConnectOptions, &'static str> {
    let options = explicit_connection(url)?;
    if !allow_live {
        let database = options.get_database().unwrap_or_default();
        if url.contains(['?', '#'])
            || !(url.starts_with("postgres://agent_test:@")
                || url.starts_with("postgresql://agent_test:@"))
            || options.get_host() != "agent-testdb"
            || options.get_port() != 5432
            || options.get_username() != "agent_test"
            || !database.starts_with("two_bot_test_")
            || database.len() > 63
            || !database
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            return Err("Refusing live/unknown target; --allow-live-target requires separately approved cutover");
        }
    }
    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(args: &[&str]) -> Result<Options, &'static str> {
        Options::parse(&args.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn default_dry_run_and_strict_flags() {
        let options = parse(&[]).unwrap();
        assert!(!options.apply);
        assert_eq!(options.batch_size, 500);
        for args in [
            vec!["--apply=false"],
            vec!["--apply", "false"],
            vec!["--batch-size", "0"],
            vec!["--batch-size=10001"],
            vec!["--groups", ""],
            vec!["--apply", "--apply"],
            vec!["--plan", "--apply"],
            vec!["--unknown"],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
        assert!(
            parse(&["--apply", "--groups=funnel,leveling", "--batch-size", "1"])
                .unwrap()
                .apply
        );
    }

    #[test]
    fn target_guard_refuses_overrides_and_unknown_endpoints_before_connecting() {
        let safe = "postgres://agent_test:@agent-testdb:5432/two_bot_test_copy_target";
        assert!(guarded_target(safe, false).is_ok());
        for url in [
            "postgres://agent_test:@production:5432/two_bot_test_copy_target",
            "postgres://agent_test:@agent-testdb:5432/production",
            "postgres://agent_test:@localhost:5432/two_bot_test_copy_target",
            "postgres://agent_test:@agent-testdb:5433/two_bot_test_copy_target",
            "postgres://agent_test:secret@agent-testdb:5432/two_bot_test_copy_target",
            "postgres://other:@agent-testdb:5432/two_bot_test_copy_target",
        ] {
            assert!(guarded_target(url, false).is_err());
        }
        for suffix in [
            "?host=production",
            "?hostaddr=127.0.0.1",
            "?dbname=production",
            "?user=other",
            "?options=-csearch_path=other",
            "#fragment",
        ] {
            assert!(guarded_target(&format!("{safe}{suffix}"), false).is_err());
        }
        assert!(guarded_target("postgres://operator:explicit@live.invalid:5432/bot", true).is_ok());
    }

    #[test]
    fn identical_databases_and_pending_groups_refuse() {
        let options = parse(&[
            "--source-url=postgres://agent_test:@agent-testdb:5432/two_bot_test_copy_same",
            "--target-url=postgres://agent_test:@agent-testdb:5432/two_bot_test_copy_same",
        ])
        .unwrap();
        assert!(options.connections().is_err());
        let options = parse(&["--groups=tickets"]).unwrap();
        assert!(options
            .tables()
            .unwrap_err()
            .contains("pending group tickets"));
    }
}
