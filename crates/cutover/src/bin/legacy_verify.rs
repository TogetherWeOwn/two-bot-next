//! Read-only legacy-versus-next verification. Exit 0 equal, 1 mismatch,
//! 2 usage, unsafe mapping, connection or query failure. URLs are never printed.

// Operator commands intentionally emit human-readable/JSON output to stdout.
#![allow(clippy::print_stdout)]

use std::str::FromStr;

use sqlx::{postgres::PgConnectOptions, ConnectOptions, Connection, PgConnection};
use two_bot_cutover::legacy_mapping::MappingSpec;
use two_bot_cutover::legacy_verify::{verify, DEFAULT_SAMPLE_LIMIT, MAX_SAMPLE_LIMIT};
use url::Url;

const USAGE: &str =
    "Usage: legacy_verify --source-url <url> --target-url <url> --mapping <spec.json>\n\
    [--group <name>]... [--sample-limit <0..1000>]\n\
    Prefer --source-url-env <variable> and --target-url-env <variable> to keep\n\
    credentials out of process arguments. Reports JSON to stdout; writes no data.\n\
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

fn connection_options(text: &str) -> Result<PgConnectOptions, &'static str> {
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
    let mut options = PgConnectOptions::from_str(text).map_err(|_| "invalid database URL")?;
    if url.password().is_none() {
        options = options.password("");
    }
    Ok(options
        .application_name("legacy_verify")
        .disable_statement_logging())
}

async fn run(args: Args) -> Result<bool, String> {
    let text = std::fs::read_to_string(&args.mapping)
        .map_err(|_| "cannot read mapping file".to_owned())?;
    let spec = MappingSpec::parse(&text).map_err(|e| e.to_string())?;
    spec.select(&args.groups).map_err(|e| e.to_string())?;
    // Validate BOTH endpoints before connecting. Stop on the first connection
    // failure; never retry with another credential or an inherited app URL.
    let source_options = connection_options(&args.source)?;
    let target_options = connection_options(&args.target)?;
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
}
