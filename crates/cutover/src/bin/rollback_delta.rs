use two_bot_cutover::{
    cli::{open_db, Args},
    rollback_delta::{export_delta, file_writer, parse_since, report},
};

#[tokio::main]
async fn main() {
    let args = Args::parse(&std::env::args().skip(1).collect::<Vec<_>>());
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
