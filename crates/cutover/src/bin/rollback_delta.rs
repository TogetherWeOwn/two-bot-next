// Operator CLI reports intentionally use stdout; runtime/library modules do not.
#![allow(clippy::print_stdout)]

use two_bot_cutover::{
    cli::{open_db, Args, DB_POOL_HELP},
    legacy_copy::options::guarded_target,
    over_cap_message, read_string_capped,
    rollback_delta::{export_delta, file_writer, parse_since, report},
    rollback_import::{apply_import, import_plan, parse_import},
    IntakeReadError,
};

const IMPORT_HELP: &str =
    "Usage: rollback-delta import --file <export.ndjson> [--apply] [--allow-live-target] [--acknowledge-divergent]

Replay a `rollback-delta --export` file into the target from
TWO_DATABASE_URL. Default is a dry run: the file is validated offline
and a row-count plan is printed as JSON on stdout, without opening any
database. `--apply` prints the same plan with `\"mode\": \"apply\"`
before mutating, then replays the rows inside one transaction;
re-running an identical import changes nothing. `--apply` requires
`--allow-live-target` before any connection attempt. Malformed lines,
unknown tables and per-table row-cap overruns refuse with exit 2 before
any write. Rows replay parent-first; `lfg_roles` rows travel as a parent
join with their exported post, so a post-freeze post imports with its
roles and signups. The apply reports inserted vs already-present rows per
table (pre-existing rows keep their values) and advances owned sequences
past the imported maxima. Skipped rows with no identical match are
divergent: the first run that drops updates is reported with its table:key
entries and exits 3 unless `--acknowledge-divergent` is passed. The check is
primary-keyed inside a UTC-pinned transaction. A failed insert names its
table and constraint for integrity violations; other server errors stay
redacted. No migrations are applied. Intake caps: the file must not exceed
8 MiB and no single line may exceed 1 MiB (both refuse with exit 2 before
any parse).";

#[tokio::main]
async fn main() {
    let args = Args::parse(&std::env::args().skip(1).collect::<Vec<_>>());
    if args.positionals.first().is_some_and(|p| p == "import") {
        if args.has("help") {
            println!("{IMPORT_HELP}\n{DB_POOL_HELP}");
            return;
        }
        std::process::exit(run_import(args).await);
    }
    if args.has("help") {
        println!(
            "Usage: rollback-delta --since <RFC3339 T_f> [--export <path>]\n\
             Read-only Next-window delta report; stdout carries the JSON summary. \
             TWO_DATABASE_URL required; no migrations are applied.\n{DB_POOL_HELP}"
        );
        return;
    }
    if let Err(e) = run(args).await {
        eprintln!("rollback-delta: {e}");
        std::process::exit(2);
    }
}

async fn run(args: Args) -> Result<(), String> {
    if args.values.keys().any(|k| k != "since" && k != "export")
        || !args.flags.is_empty()
        || !args.positionals.is_empty()
    {
        return Err("unknown or malformed argument".into());
    }
    let export_path = match args.get("export") {
        Some(path) if path.trim().is_empty() => return Err("--export needs a path".into()),
        path => path.map(str::to_owned),
    };
    let since = parse_since(args.get("since")).map_err(|e| e.to_string())?;
    let bound = since
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|_| "cannot normalize --since timestamp".to_owned())?;
    // Read-only by construction: no migrations, and every query runs inside a
    // READ ONLY snapshot transaction (see rollback_delta).
    let db = open_db(&args, true).await;
    let summary = report(db.pool(), &bound).await.map_err(|e| e.to_string())?;
    println!(
        "{}",
        serde_json::to_string_pretty(&summary).map_err(|_| "cannot encode report")?
    );
    if let Some(path) = export_path {
        let file = std::fs::File::create(&path).map_err(|_| "cannot write export file")?;
        let mut sink = file_writer(file);
        let exported = export_delta(db.pool(), &bound, &mut sink)
            .await
            .map_err(|e| e.to_string())?;
        eprintln!("rollback-delta: exported {exported} rows to {path}");
    }
    db.close().await;
    Ok(())
}

/// Replay an `--export` file. Exit 2 (refusal, empty stdout) for argument,
/// file-validation and live-target-guard failures; exit 1 for connection or
/// apply failures after the plan was printed; exit 3 when divergent rows were
/// dropped (unless `--acknowledge-divergent`); exit 0 on success.
async fn run_import(args: Args) -> i32 {
    if args.positionals.len() != 1
        || args
            .flags
            .iter()
            .any(|f| f != "apply" && f != "allow-live-target" && f != "acknowledge-divergent")
        || args
            .values
            .keys()
            .any(|k| k != "file" && k != "allow-live-target" && k != "acknowledge-divergent")
    {
        eprintln!("rollback-delta import: unknown or malformed argument");
        return 2;
    }
    if args.values.contains_key("allow-live-target") {
        eprintln!(
            "rollback-delta import: --allow-live-target is a bare opt-in and does not accept a value"
        );
        return 2;
    }
    if args.values.contains_key("acknowledge-divergent") {
        eprintln!(
            "rollback-delta import: --acknowledge-divergent is a bare opt-in and does not accept a value"
        );
        return 2;
    }
    let apply = args.flags.contains("apply");
    let allow_live = args.flags.contains("allow-live-target");
    let file = match args.values.get("file") {
        Some(path) if !path.trim().is_empty() => path.to_owned(),
        _ => {
            eprintln!("rollback-delta import: missing --file <export.ndjson>");
            return 2;
        }
    };
    let contents = match read_string_capped(&file) {
        Ok(contents) => contents,
        Err(IntakeReadError::OverCap) => {
            eprintln!(
                "rollback-delta import: {}",
                over_cap_message("rollback delta import file")
            );
            return 2;
        }
        Err(IntakeReadError::Unreadable(_)) => {
            eprintln!("rollback-delta import: cannot read import file '{file}'");
            return 2;
        }
    };
    // Offline validation first: a malformed file, an unknown table or a
    // row-cap overrun refuses before any write (or any connection).
    let rows = match parse_import(&contents) {
        Ok(rows) => rows,
        Err(error) => {
            eprintln!("rollback-delta import: {error}");
            return 2;
        }
    };
    // The live-target fence precedes every connection attempt and never
    // echoes the URL.
    if apply && !allow_live {
        eprintln!(
            "rollback-delta import: Refusing live/unknown target without --allow-live-target; pass it only for an owner-approved rollback"
        );
        return 2;
    }
    // The row-count plan prints before any mutation, in both modes.
    let mode = if apply { "apply" } else { "dry-run" };
    println!(
        "{}",
        serde_json::to_string_pretty(&import_plan(mode, &rows))
            .unwrap_or_else(|_| "{\"version\":1}".to_owned())
    );
    if !apply {
        return 0;
    }
    let url = std::env::var("TWO_DATABASE_URL").unwrap_or_default();
    if url.trim().is_empty() {
        eprintln!("rollback-delta import: TWO_DATABASE_URL is required.");
        return 2;
    }
    // Shape-check the target URL before connecting (never echoed). The
    // opt-in above already passed, so this only rejects malformed URLs.
    if let Err(error) = guarded_target(&url, true) {
        eprintln!("rollback-delta import: {error}");
        return 2;
    }
    let db = match two_bot_cutover::connect(&url, args.db_pool_max(), true).await {
        Ok(db) => db,
        Err(_) => {
            eprintln!("rollback-delta import: cannot open database; connection details redacted");
            return 1;
        }
    };
    let acknowledged = args.flags.contains("acknowledge-divergent");
    let outcome = match apply_import(db.pool(), &rows).await {
        Ok(outcome) => {
            eprintln!(
                "rollback-delta import: applied {} of {} rows ({} already present, {} divergent)",
                outcome.inserted,
                outcome.total(),
                outcome.skipped,
                outcome.divergent
            );
            for table in &outcome.per_table {
                eprintln!(
                    "rollback-delta import: table {}: inserted {} already present {} divergent {}",
                    table.table, table.inserted, table.skipped, table.divergent
                );
            }
            if outcome.divergent > 0 {
                for key in outcome.divergent_keys.iter().take(50) {
                    eprintln!("rollback-delta import: divergent {key}");
                }
                if acknowledged {
                    eprintln!(
                        "rollback-delta import: divergent rows acknowledged; target values kept"
                    );
                    0
                } else {
                    eprintln!(
                        "rollback-delta import: {} divergent rows need manual reconciliation; re-run with --acknowledge-divergent once reconciled",
                        outcome.divergent
                    );
                    3
                }
            } else {
                0
            }
        }
        Err(two_bot_cutover::rollback_import::ImportApplyError::ConstraintViolation {
            table,
            constraint,
        }) => {
            eprintln!(
                "rollback-delta import: import into '{table}' violates {constraint} (parent row missing or already changed; see docs/cutover.md)"
            );
            1
        }
        Err(_) => {
            eprintln!("rollback-delta import: database error (details omitted)");
            1
        }
    };
    db.close().await;
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(argv: &[&str]) -> Args {
        Args::parse(
            &argv
                .iter()
                .map(|item| (*item).to_owned())
                .collect::<Vec<_>>(),
        )
    }

    fn scratch_path(tag: &str) -> std::path::PathBuf {
        let base = std::env::var_os("PAPERCLIP_RUN_SCRATCH_DIR")
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::var_os("PAPERCLIP_SCRATCH_DIR").map(std::path::PathBuf::from))
            .unwrap_or_else(std::env::temp_dir);
        let _ = std::fs::create_dir_all(&base);
        base.join(format!(
            "rollback-delta-bin-{}-{tag}.ndjson",
            std::process::id()
        ))
    }

    fn fixture(name: &str) -> String {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
            .to_string_lossy()
            .into_owned()
    }

    #[tokio::test]
    async fn unknown_arguments_refuse() {
        assert_eq!(run_import(args(&["import", "extra"])).await, 2);
        assert_eq!(run_import(args(&["import", "--bogus"])).await, 2);
        assert_eq!(
            run_import(args(&["import", "--file", "x", "--bogus", "y"])).await,
            2
        );
    }

    #[tokio::test]
    async fn live_target_opt_in_with_a_value_refuses() {
        assert_eq!(
            run_import(args(&["import", "--allow-live-target=x", "--file", "x"])).await,
            2
        );
    }

    #[tokio::test]
    async fn acknowledge_divergent_with_a_value_refuses() {
        assert_eq!(
            run_import(args(&[
                "import",
                "--acknowledge-divergent=x",
                "--file",
                "x"
            ]))
            .await,
            2
        );
    }

    #[tokio::test]
    async fn dry_run_accepts_acknowledge_divergent_and_succeeds() {
        let file = fixture("rollback_import_valid.ndjson");
        assert_eq!(
            run_import(args(&[
                "import",
                "--file",
                file.as_str(),
                "--acknowledge-divergent"
            ]))
            .await,
            0
        );
    }

    #[tokio::test]
    async fn missing_file_flag_refuses() {
        assert_eq!(run_import(args(&["import"])).await, 2);
        assert_eq!(run_import(args(&["import", "--file", "   "])).await, 2);
    }

    #[tokio::test]
    async fn unreadable_file_refuses() {
        let missing = scratch_path("absent").to_string_lossy().into_owned();
        let _ = std::fs::remove_file(&missing);
        assert_eq!(
            run_import(args(&["import", "--file", missing.as_str()])).await,
            2
        );
    }

    #[tokio::test]
    async fn over_cap_file_refuses_before_parsing() {
        // One byte past the cap, otherwise shaped to sail through every
        // later check, so only the size enforcement can refuse it.
        let line = "{\"table\":\"events\",\"row\":{\"id\":1}}\n";
        let repeats = two_bot_cutover::INTAKE_CAP_BYTES / line.len() + 1;
        let contents = line.repeat(repeats);
        assert!(contents.len() > two_bot_cutover::INTAKE_CAP_BYTES);
        let file = scratch_path("over-cap");
        std::fs::write(&file, &contents).unwrap();
        let path = file.to_string_lossy().into_owned();
        assert_eq!(
            run_import(args(&["import", "--file", path.as_str()])).await,
            2
        );
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn malformed_and_unknown_table_files_refuse() {
        let malformed = scratch_path("malformed");
        std::fs::write(&malformed, "{broken\n").unwrap();
        let path = malformed.to_string_lossy().into_owned();
        assert_eq!(
            run_import(args(&["import", "--file", path.as_str()])).await,
            2
        );

        let unknown = scratch_path("unknown-table");
        std::fs::write(&unknown, "{\"table\":\"no_such_table\",\"row\":{}}\n").unwrap();
        let path = unknown.to_string_lossy().into_owned();
        assert_eq!(
            run_import(args(&["import", "--file", path.as_str()])).await,
            2
        );

        let _ = std::fs::remove_file(&malformed);
        let _ = std::fs::remove_file(&unknown);
    }

    #[tokio::test]
    async fn apply_without_live_target_opt_in_refuses() {
        let file = fixture("rollback_import_valid.ndjson");
        assert_eq!(
            run_import(args(&["import", "--file", file.as_str(), "--apply"])).await,
            2
        );
    }

    #[tokio::test]
    async fn dry_run_validates_offline_and_succeeds() {
        let file = fixture("rollback_import_valid.ndjson");
        assert_eq!(
            run_import(args(&["import", "--file", file.as_str()])).await,
            0
        );
    }
}
