//! Read-only legacy-versus-next verification. Exit 0 equal, 1 mismatch,
//! 2 usage, unsafe mapping, connection or query failure. URLs are never printed.

// Operator commands intentionally emit human-readable/JSON output to stdout.
#![allow(clippy::print_stdout)]

use sqlx::{postgres::PgConnectOptions, ConnectOptions, Connection, PgConnection};
use two_bot_core::database_tls::{self, TlsPolicy};
use two_bot_cutover::legacy_mapping::MappingSpec;
use two_bot_cutover::legacy_verify::{verify, DEFAULT_SAMPLE_LIMIT, MAX_SAMPLE_LIMIT};
use two_bot_cutover::{over_cap_message, read_string_capped, IntakeReadError};
use url::Url;

const USAGE: &str =
    "Usage: legacy_verify --source-url <url> --target-url <url> --mapping <spec.json>\n\
    [--group <name>]... [--sample-limit <0..1000>]\n\
    Prefer --source-url-env <variable> and --target-url-env <variable> to keep\n\
    credentials out of process arguments. Reports JSON to stdout; writes no data.\n\
    Intake cap: the mapping file must not exceed 8 MiB (exit 2, before any connection).\n\
    Exit codes: 0 equal, 1 mismatch, 2 refusal/error. No migrations or apply mode.";

struct Args {
    source: String,
    target: String,
    mapping: String,
    groups: Vec<String>,
    sample_limit: usize,
}

impl Args {
    fn parse(argv: &[String]) -> Result<Self, &'static str> {
        let mut source = None;
        let mut target = None;
        let mut mapping = None;
        let mut groups = Vec::new();
        let mut sample_limit = None;
        let (pairs, remainder) = argv.as_chunks::<2>();
        for [key, value] in pairs {
            match key.as_str() {
                "--source-url" if source.is_none() => source = Some(value.clone()),
                "--target-url" if target.is_none() => target = Some(value.clone()),
                "--source-url-env" if source.is_none() => {
                    source = Some(
                        std::env::var(value).map_err(|_| "source URL variable is unavailable")?,
                    )
                }
                "--target-url-env" if target.is_none() => {
                    target = Some(
                        std::env::var(value).map_err(|_| "target URL variable is unavailable")?,
                    )
                }
                "--mapping" if mapping.is_none() => mapping = Some(value.clone()),
                "--group" => groups.push(value.clone()),
                "--sample-limit" if sample_limit.is_none() => {
                    let limit = value.parse::<usize>().map_err(|_| "invalid sample limit")?;
                    if limit > MAX_SAMPLE_LIMIT {
                        return Err("sample limit must not exceed 1000");
                    }
                    sample_limit = Some(limit);
                }
                _ => return Err("unknown or duplicate argument"),
            }
        }
        if !remainder.is_empty() {
            return Err("argument needs a value");
        }
        Ok(Self {
            source: source.ok_or("source URL is required")?,
            target: target.ok_or("target URL is required")?,
            mapping: mapping.ok_or("mapping file is required")?,
            groups,
            sample_limit: sample_limit.unwrap_or(DEFAULT_SAMPLE_LIMIT),
        })
    }
}

/// Read the one TLS policy setting; an unset value is `Required`.
fn tls_policy_from_env() -> Result<TlsPolicy, String> {
    let value = std::env::var_os(database_tls::POLICY_SETTING);
    // A non-UTF-8 value parses as "" and is refused like any unknown value.
    TlsPolicy::from_setting(value.as_ref().map(|v| v.to_str().unwrap_or("")))
        .map_err(|message| message.to_owned())
}

fn connection_options(text: &str, tls: TlsPolicy) -> Result<PgConnectOptions, &'static str> {
    // SQLx defaults consult libpq environment. Refuse those settings instead
    // of accidentally using a server credential or a different endpoint.
    for key in [
        "PGHOST",
        "PGHOSTADDR",
        "PGPORT",
        "PGUSER",
        "PGPASSWORD",
        "PGDATABASE",
        "PGPASSFILE",
        "PGSERVICE",
        "PGSERVICEFILE",
        "PGOPTIONS",
        "PGSSLCERT",
        "PGSSLKEY",
        "PGSSLROOTCERT",
        "PGSSLMODE",
    ] {
        if std::env::var_os(key).is_some() {
            return Err("inherited PG connection settings are forbidden; provide explicit URLs");
        }
    }
    let url = Url::parse(text).map_err(|_| "invalid database URL")?;
    if !matches!(url.scheme(), "postgres" | "postgresql")
        || url.host_str().is_none()
        || url.username().is_empty()
        || url.path().trim_start_matches('/').is_empty()
        || url.fragment().is_some()
        || url.query_pairs().any(|(key, _)| key != "sslmode")
    {
        return Err("URL must specify a PostgreSQL host, username and database (only sslmode query is supported)");
    }
    // Threat-model F6: refuse plaintext/unverified modes and the wrong host
    // class before SQLx parses the URL (see `docs/database-tls.md`). The
    // allowlist check runs first so unsupported query values never reach the
    // driver's WARN diagnostics; every refusal below is a fixed string with
    // no URL, host or credential material.
    two_bot_core::database_url::validate(text)?;
    database_tls::enforce(text, tls)?;
    // The sslmode-only check above is a strict subset of the shared allowlist,
    // so the shared parser preserves it while suppressing the passfile
    // diagnostic target during the synchronous parse.
    let mut options =
        two_bot_core::database_url::connect_options(text).map_err(|_| "invalid database URL")?;
    if url.password().is_none() {
        options = options.password("");
    }
    Ok(database_tls::apply(
        options
            .application_name("legacy_verify")
            .disable_statement_logging(),
        tls,
    ))
}

async fn run(args: Args) -> Result<bool, String> {
    let text = match read_string_capped(&args.mapping) {
        Ok(text) => text,
        Err(IntakeReadError::OverCap) => return Err(over_cap_message("mapping file")),
        Err(IntakeReadError::Unreadable(_)) => {
            return Err("cannot read mapping file".to_owned());
        }
    };
    let spec = MappingSpec::parse(&text).map_err(|e| e.to_string())?;
    spec.select(&args.groups).map_err(|e| e.to_string())?;
    // Validate BOTH endpoints before connecting. Stop on the first connection
    // failure; never retry with another credential or an inherited app URL.
    // The TLS policy (unset means `required`) fences both URLs before SQLx
    // parses them; a refusal is a fixed non-echoing string and exit code 2.
    let tls = tls_policy_from_env()?;
    let source_options = connection_options(&args.source, tls)?;
    let target_options = connection_options(&args.target, tls)?;
    let mut source = PgConnection::connect_with(&source_options)
        .await
        .map_err(|_| "source connection failed (details omitted)")?;
    let mut target = PgConnection::connect_with(&target_options)
        .await
        .map_err(|_| "target connection failed (details omitted)")?;
    let report = verify(
        &mut source,
        &mut target,
        &spec,
        &args.groups,
        args.sample_limit,
    )
    .await
    .map_err(|e| e.to_string())?;
    println!(
        "{}",
        serde_json::to_string_pretty(&report).map_err(|_| "cannot serialize report")?
    );
    source
        .close()
        .await
        .map_err(|_| "source connection close failed")?;
    target
        .close()
        .await
        .map_err(|_| "target connection close failed")?;
    Ok(report.matches)
}

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv == ["--help"] {
        println!("{USAGE}");
        return;
    }
    let args = match Args::parse(&argv) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}\n{USAGE}");
            std::process::exit(2);
        }
    };
    match run(args).await {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Thread-local log capture for the sslmode-only fence.
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

    /// Inherited libpq settings under test. Serializes the env-mutating
    /// connection tests in this binary (the only tests here that touch PG*).
    static PG_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    const PG_KEYS: [&str; 14] = [
        "PGHOST",
        "PGHOSTADDR",
        "PGPORT",
        "PGUSER",
        "PGPASSWORD",
        "PGDATABASE",
        "PGPASSFILE",
        "PGSERVICE",
        "PGSERVICEFILE",
        "PGOPTIONS",
        "PGSSLCERT",
        "PGSSLKEY",
        "PGSSLROOTCERT",
        "PGSSLMODE",
    ];

    struct SavedEnv(Vec<(String, Option<std::ffi::OsString>)>);

    impl SavedEnv {
        fn clear_pg_and_isolate_home() -> (std::sync::MutexGuard<'static, ()>, Self, TempHome) {
            let guard = PG_ENV_LOCK.lock().unwrap();
            let mut saved = Vec::new();
            for key in PG_KEYS {
                saved.push((key.to_owned(), std::env::var_os(key)));
                std::env::remove_var(key);
            }
            saved.push(("HOME".to_owned(), std::env::var_os("HOME")));
            let dir = std::env::temp_dir().join(format!(
                "two-bot-legacy-verify-home-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).expect("synthetic HOME");
            std::env::set_var("HOME", &dir);
            let home_guard = TempHome { dir };
            (guard, Self(saved), home_guard)
        }
    }

    impl Drop for SavedEnv {
        fn drop(&mut self) {
            for (key, value) in self.0.drain(..) {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    struct TempHome {
        dir: std::path::PathBuf,
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    fn offline_args(mapping: &str) -> Args {
        Args {
            source: "postgres://operator:explicit@agent-testdb:5432/bot".to_owned(),
            target: "postgres://operator:explicit@agent-testdb:5432/bot".to_owned(),
            mapping: mapping.to_owned(),
            groups: Vec::new(),
            sample_limit: DEFAULT_SAMPLE_LIMIT,
        }
    }

    fn scratch_mapping(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("legacy-verify-cap-{}-{tag}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir.join("spec.json")
    }

    #[tokio::test]
    async fn over_cap_mapping_refuses_before_any_connection() {
        // Content never parses: the cap fires on byte length alone.
        let path = scratch_mapping("over-cap");
        std::fs::write(&path, vec![b' '; two_bot_cutover::INTAKE_CAP_BYTES + 1]).unwrap();
        let err = run(offline_args(&path.to_string_lossy()))
            .await
            .unwrap_err();
        assert!(err.contains("exceeds"), "{err}");
        assert!(err.contains("mapping"), "{err}");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn missing_mapping_reports_cannot_read() {
        let err = run(offline_args("/nonexistent-legacy-verify-mapping-spec.json"))
            .await
            .unwrap_err();
        assert_eq!(err, "cannot read mapping file");
    }

    #[test]
    fn unknown_flags_and_invalid_sample_limits_are_refused() {
        for argv in [
            vec!["--apply", "yes"],
            vec!["--sample-limit", "1001"],
            vec!["--sample-limit", "-1"],
            vec!["--mapping"],
        ] {
            assert!(Args::parse(&argv.into_iter().map(str::to_owned).collect::<Vec<_>>()).is_err());
        }
    }

    #[test]
    fn parses_groups_and_zero_samples() {
        let argv = [
            "--source-url",
            "postgres://agent_test@agent-testdb/source",
            "--target-url",
            "postgres://agent_test@agent-testdb/target",
            "--mapping",
            "spec.json",
            "--group",
            "first",
            "--group",
            "second",
            "--sample-limit",
            "0",
        ]
        .map(str::to_owned);
        let args = Args::parse(&argv).unwrap();
        assert_eq!(args.groups, ["first", "second"]);
        assert_eq!(args.sample_limit, 0);
    }

    #[test]
    fn hostile_query_keys_rejected_before_driver_parse() {
        let (_lock, _env, _home) = SavedEnv::clear_pg_and_isolate_home();
        let capture = Capture::default();
        tracing::subscriber::with_default(capture.clone(), || {
            for query in [
                "API_KEY=fixture-query-secret",
                "Api_Key=fixture-query-secret",
                "SSLMODE=require",
                "api%5Fkey=fixture-query-secret",
                "API%5FKEY=fixture-query-secret",
                "sslmode=require&API_KEY=fixture-query-secret",
                "options=-csearch_path=other",
            ] {
                let url = format!("postgres://operator:explicit@live.invalid:5432/bot?{query}");
                let error = connection_options(&url, TlsPolicy::Required).unwrap_err();
                assert!(
                    !error.contains("fixture"),
                    "rejection echoed the query value for {query}"
                );
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
        assert!(!text.contains("ignoring unrecognized connect parameter"));
    }

    #[test]
    fn tls_policy_gates_hosts_and_modes_before_parsing() {
        let (_lock, _env, _home) = SavedEnv::clear_pg_and_isolate_home();
        // `LocalOnly` admits the disposable service host with any known mode,
        // preserving the sslmode query behavior operators rely on.
        let local = "postgres://operator:explicit@agent-testdb:5432/bot";
        assert!(connection_options(local, TlsPolicy::LocalOnly).is_ok());
        assert!(
            connection_options(&format!("{local}?sslmode=require"), TlsPolicy::LocalOnly).is_ok()
        );
        assert!(
            connection_options(&format!("{local}?sslmode=disable"), TlsPolicy::LocalOnly).is_ok()
        );
        // `Required` (the unset default) refuses the local host outright and
        // admits a remote host only with an explicit TLS mode.
        let remote =
            "postgres://operator:explicit@ep-fixture-host.us-east-2.aws.neon.tech:5432/bot";
        assert_eq!(
            connection_options(local, TlsPolicy::Required).unwrap_err(),
            "local database host is refused under the required TLS policy"
        );
        assert_eq!(
            connection_options(remote, TlsPolicy::Required).unwrap_err(),
            "database URL must set sslmode under the required TLS policy"
        );
        assert!(
            connection_options(&format!("{remote}?sslmode=require"), TlsPolicy::Required).is_ok()
        );
        assert_eq!(
            connection_options(&format!("{remote}?sslmode=disable"), TlsPolicy::Required)
                .unwrap_err(),
            "database sslmode does not require TLS"
        );
        // `LocalOnly` never reaches a remote host, whatever the mode.
        assert_eq!(
            connection_options(&format!("{remote}?sslmode=require"), TlsPolicy::LocalOnly)
                .unwrap_err(),
            "remote database host is refused under the local-only TLS policy"
        );
    }

    #[test]
    fn inherited_pgpassfile_is_forbidden() {
        let (_lock, _env, _home) = SavedEnv::clear_pg_and_isolate_home();
        std::env::set_var("PGPASSFILE", "/tmp/synthetic-passfile");
        let error = connection_options(
            "postgres://operator:explicit@live.invalid:5432/bot",
            TlsPolicy::Required,
        )
        .unwrap_err();
        assert_eq!(
            error,
            "inherited PG connection settings are forbidden; provide explicit URLs"
        );
    }

    #[test]
    fn missing_url_password_clears_fallback_without_reading_host_passfile() {
        let (_lock, _env, _home) = SavedEnv::clear_pg_and_isolate_home();
        // Synthetic HOME is empty (no `~/.pgpass`) and all PG* are absent,
        // so no host credential can supply a fallback. The explicit
        // passwordless URL must still parse to a cleared password.
        let options = connection_options(
            "postgres://operator@agent-testdb:5432/bot",
            TlsPolicy::LocalOnly,
        )
        .unwrap();
        // Cleared to trust-authentication, never a fallback credential.
        // `PgConnectOptions` derives `Debug` with the password field visible
        // in-memory; the assertion message itself carries no credential.
        let debug = format!("{options:?}");
        assert!(
            debug.contains("password: Some(\"\")"),
            "passwordless URL did not clear to trust-auth"
        );
    }
}
