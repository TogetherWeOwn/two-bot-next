// Operator CLI reports intentionally use stdout; runtime/library modules do not.
#![allow(clippy::print_stdout)]

use two_bot_cutover::{
    cli::{open_db, Args},
    legacy_copy::options::guarded_target,
    rollback_delta::{export_delta, file_writer, parse_since, report},
    rollback_import::{apply_import, import_plan, parse_import},
};

const IMPORT_HELP: &str =
    "Usage: rollback-delta import --file <export.ndjson> [--apply] [--allow-live-target]

Replay a `rollback-delta --export` file into the target from
TWO_DATABASE_URL. Default is a dry run: the file is validated offline
and a row-count plan is printed as JSON on stdout, without opening any
database. `--apply` prints the same plan with `\"mode\": \"apply\"`
before mutating, then replays the rows inside one transaction;
re-running an identical import changes nothing. `--apply` requires
`--allow-live-target` before any connection attempt. Malformed lines,
unknown tables and per-table row-cap overruns refuse with exit 2 before
any write. No migrations are applied.";

#[tokio::main]
async fn main() {
    let args = Args::parse(&std::env::args().skip(1).collect::<Vec<_>>());
    if args.positionals.first().is_some_and(|p| p == "import") {
        if args.has("help") {
            println!("{IMPORT_HELP}");
            return;
        }
        std::process::exit(run_import(args).await);
    }
    if args.has("help") {
        println!(
            "Usage: rollback-delta --since <RFC3339 T_f> [--export <path>]\n\
             Read-only Next-window delta report; stdout carries the JSON summary. \
             TWO_DATABASE_URL required; no migrations are applied."
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
/// apply failures after the plan was printed; exit 0 on success.
async fn run_import(args: Args) -> i32 {
    if args.positionals.len() != 1
        || args
            .flags
            .iter()
            .any(|f| f != "apply" && f != "allow-live-target")
        || args
            .values
            .keys()
            .any(|k| k != "file" && k != "allow-live-target")
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
    let apply = args.flags.contains("apply");
    let allow_live = args.flags.contains("allow-live-target");
    let file = match args.values.get("file") {
        Some(path) if !path.trim().is_empty() => path.to_owned(),
        _ => {
            eprintln!("rollback-delta import: missing --file <export.ndjson>");
            return 2;
        }
    };
    let contents = match std::fs::read_to_string(&file) {
        Ok(contents) => contents,
        Err(_) => {
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
    let pool_max: u32 = std::env::var("TWO_DB_POOL_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(two_bot_cutover::DB_POOL_MAX_DEFAULT);
    let db = match two_bot_cutover::connect(&url, pool_max, true).await {
        Ok(db) => db,
        Err(_) => {
            eprintln!("rollback-delta import: cannot open database; connection details redacted");
            return 1;
        }
    };
    let outcome = match apply_import(db.pool(), &rows).await {
        Ok(imported) => {
            eprintln!("rollback-delta import: applied {imported} rows");
            0
        }
        Err(_) => {
            eprintln!("rollback-delta import: database error (details omitted)");
            1
        }
    };
    db.close().await;
    outcome
}
