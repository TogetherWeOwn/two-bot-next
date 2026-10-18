//! Strict CLI parsing and a fail-closed target fence. No URL in diagnostics.

use super::{mapping, Table};
use sqlx::postgres::PgConnectOptions;
use sqlx::ConnectOptions;
use two_bot_core::{database_tls, database_url};

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
        if out.groups.iter().any(|name| {
            name != "ready"
                && name != "all"
                && !mapping::GROUPS.iter().any(|group| group.name == name)
        }) {
            return Err("unknown table group (see --plan)");
        }
        Ok(out)
    }

    pub fn tables(&self) -> Result<Vec<Table>, String> {
        mapping::select(&self.groups)
    }

    /// Only the tool-specific variables are read; never TWO_DATABASE_URL,
    /// DATABASE_URL, production bindings, or fallback credentials.
    /// Threat-model F6: both URLs pass the `TWO_DATABASE_TLS` policy
    /// (unset means `required`) before SQLx parses them, and the effective
    /// TLS mode is applied to both option sets (see `docs/database-tls.md`).
    /// Refusals are fixed strings; nothing echoes the URL, host or credential.
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
        let tls = tls_policy_from_env()?;
        for url in [&source_url, &target_url] {
            database_url::validate(url)?;
            database_tls::enforce(url, tls)?;
        }
        let source = database_tls::apply(explicit_connection(&source_url)?, tls);
        let target = database_tls::apply(guarded_target(&target_url, self.allow_live_target)?, tls);
        if source.get_host() == target.get_host()
            && source.get_port() == target.get_port()
            && source.get_database() == target.get_database()
        {
            return Err("source and target must be different databases");
        }
        Ok((source, target))
    }
}

/// Read the one TLS policy setting; an unset value is `Required`.
fn tls_policy_from_env() -> Result<database_tls::TlsPolicy, &'static str> {
    let value = std::env::var_os(database_tls::POLICY_SETTING);
    // A non-UTF-8 value parses as "" and is refused like any unknown value.
    database_tls::TlsPolicy::from_setting(value.as_ref().map(|v| v.to_str().unwrap_or("")))
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
    // Fail-closed query fence before any driver parsing, with the passfile
    // diagnostic target suppressed during the synchronous parse. The shared
    // parser validates the allowlist and strips `channel_binding`; the
    // explicit database/socket/options checks below still apply.
    let options = database_url::connect_options(url).map_err(|_| "invalid Postgres URL")?;
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
///
/// This guard does not enforce the TLS policy itself: `Options::connections`
/// runs `database_url::validate` plus `database_tls::enforce`/`apply` around
/// it. New connection-building callers must fence the TLS policy the same way.
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

    /// Serializes the env-mutating TLS-fence assertions below: `connections`
    /// reads `TWO_DATABASE_TLS` (and the tool-specific URL fallback), so no
    /// other test in this module may touch those variables concurrently.
    static TLS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct SavedTlsEnv(Vec<(String, Option<std::ffi::OsString>)>);

    impl SavedTlsEnv {
        fn set(value: Option<&str>) -> (std::sync::MutexGuard<'static, ()>, Self) {
            let guard = TLS_ENV_LOCK.lock().unwrap();
            let mut saved = Vec::new();
            for key in [
                database_tls::POLICY_SETTING,
                "LEGACY_COPY_SOURCE_URL",
                "LEGACY_COPY_TARGET_URL",
            ] {
                saved.push((key.to_owned(), std::env::var_os(key)));
                std::env::remove_var(key);
            }
            if let Some(v) = value {
                std::env::set_var(database_tls::POLICY_SETTING, v);
            }
            (guard, Self(saved))
        }
    }

    impl Drop for SavedTlsEnv {
        fn drop(&mut self) {
            for (key, value) in self.0.drain(..) {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    fn parse_urls(source: &str, target: &str, allow_live: bool) -> Options {
        let mut argv = vec![
            format!("--source-url={source}"),
            format!("--target-url={target}"),
        ];
        if allow_live {
            argv.push("--allow-live-target".to_owned());
        }
        Options::parse(&argv).unwrap()
    }

    #[test]
    fn tls_policy_fence_refuses_before_connect_with_fixed_strings() {
        let remote =
            "postgres://fixture-user:fixture-secret@ep-fixture-host.us-east-2.aws.neon.tech";
        {
            // Unset means `required`: weak and missing modes refuse, and the
            // disposable local endpoints refuse with the local-host string.
            let (_lock, _env) = SavedTlsEnv::set(None);
            for (source, target, expected) in [
                (
                    format!("{remote}/fixture-source?sslmode=disable"),
                    format!("{remote}/fixture-target?sslmode=disable"),
                    "database sslmode does not require TLS",
                ),
                (
                    format!("{remote}/fixture-source"),
                    format!("{remote}/fixture-target"),
                    "database URL must set sslmode under the required TLS policy",
                ),
                (
                    "postgres://agent_test:@agent-testdb:5432/two_bot_test_tls_source".to_owned(),
                    "postgres://agent_test:@agent-testdb:5432/two_bot_test_tls_target".to_owned(),
                    "local database host is refused under the required TLS policy",
                ),
            ] {
                let options = parse_urls(&source, &target, true);
                let error = options.connections().unwrap_err();
                assert_eq!(error, expected);
                assert!(
                    !error.contains("fixture"),
                    "TLS refusal echoed URL material: {error}"
                );
            }
        }
        {
            // Explicit `local-only`: the disposable endpoints pass the fence,
            // while a remote host refuses.
            let (_lock, _env) = SavedTlsEnv::set(Some("local-only"));
            let options = parse_urls(
                "postgres://agent_test:@agent-testdb:5432/two_bot_test_tls_source",
                "postgres://agent_test:@agent-testdb:5432/two_bot_test_tls_target",
                false,
            );
            assert!(options.connections().is_ok());
            let options = parse_urls(
                &format!("{remote}/fixture-source?sslmode=require"),
                &format!("{remote}/fixture-target?sslmode=require"),
                true,
            );
            let error = options.connections().unwrap_err();
            assert_eq!(
                error,
                "remote database host is refused under the local-only TLS policy"
            );
            assert!(
                !error.contains("fixture"),
                "TLS refusal echoed URL material: {error}"
            );
        }
        {
            // An unknown policy value refuses without echoing the value.
            let (_lock, _env) = SavedTlsEnv::set(Some("fixture-invalid-setting"));
            let options = parse_urls(
                &format!("{remote}/fixture-source?sslmode=disable"),
                &format!("{remote}/fixture-target?sslmode=disable"),
                true,
            );
            let error = options.connections().unwrap_err();
            assert_eq!(error, "invalid TWO_DATABASE_TLS setting");
            assert!(
                !error.contains("fixture"),
                "TLS refusal echoed the policy value: {error}"
            );
        }
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

    /// Thread-local log capture for the fail-closed query fence: hostile
    /// keys/values must never reach SQLx WARN diagnostics.
    #[derive(Clone, Default)]
    struct Capture(std::sync::Arc<std::sync::Mutex<String>>);

    impl Capture {
        fn text(&self) -> String {
            self.0.lock().unwrap().clone()
        }
    }

    struct CaptureFields<'a>(&'a mut String);

    impl tracing::field::Visit for CaptureFields<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            use std::fmt::Write as _;
            write!(self.0, " {}={value:?}", field.name()).unwrap();
        }
    }

    impl tracing::Subscriber for Capture {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            *metadata.level() <= tracing::Level::DEBUG
        }

        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            use std::fmt::Write as _;
            let mut text = self.0.lock().unwrap();
            write!(
                text,
                "{} {}",
                event.metadata().level(),
                event.metadata().target()
            )
            .unwrap();
            event.record(&mut CaptureFields(&mut text));
            text.push('\n');
        }
    }

    #[test]
    fn hostile_query_keys_rejected_before_driver_parse() {
        let capture = Capture::default();
        tracing::subscriber::with_default(capture.clone(), || {
            // Uppercase and percent-encoded hostile keys: `query_pairs`
            // decodes `%5F` to `_`, so the decoded key is still unknown.
            for query in [
                "API_KEY=fixture-query-secret",
                "Api_Key=fixture-query-secret",
                "SSLMODE=require",
                "api%5Fkey=fixture-query-secret",
                "API%5FKEY=fixture-query-secret",
                "api%5fkey=fixture-query-secret",
                "sslmode=require&API_KEY=fixture-query-secret",
            ] {
                let url = format!("postgres://operator:explicit@live.invalid:5432/bot?{query}");
                for result in [
                    explicit_connection(&url).map(|_| ()),
                    guarded_target(&url, true).map(|_| ()),
                ] {
                    let error = result.unwrap_err();
                    assert!(
                        !error.contains("fixture"),
                        "rejection echoed the query value for {query}"
                    );
                }
            }
            tracing::warn!("capture remains active");
        });
        let text = capture.text();
        assert!(text.contains("capture remains active"));
        assert!(
            !text.contains("fixture-query-secret"),
            "hostile query value reached logs"
        );
        assert!(!text.contains("API_KEY"));
        assert!(!text.contains("api_key"));
        assert!(!text.contains("ignoring unrecognized connect parameter"));
    }

    #[test]
    fn valid_operator_query_behavior_preserved() {
        let live = "postgres://operator:explicit@live.invalid:5432/bot";
        assert!(explicit_connection(live).is_ok());
        assert!(guarded_target(live, true).is_ok());
        assert!(explicit_connection(&format!("{live}?sslmode=require")).is_ok());
        assert!(guarded_target(&format!("{live}?sslmode=require"), true).is_ok());
    }
}
