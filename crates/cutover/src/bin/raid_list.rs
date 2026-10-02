use two_bot_cutover::{
    cli::{open_db, require_guild, Args},
    raid_tools::{cohort_csv, list_flagged, window},
};

#[tokio::main]
async fn main() {
    let args = Args::parse(&std::env::args().skip(1).collect::<Vec<_>>());
    if args.has("help") {
        println!("Usage: raid-list --guild <ID> --from <RFC3339> --to <RFC3339> [--format json|csv] [--allow-live-guild]\nRead-only flagged join evidence; stdout is the target file. TWO_DATABASE_URL required.");
        return;
    }
    let guild = require_guild(&args, "guild");
    if let Err(e) = run(args, guild).await {
        eprintln!("raid-list: {e}");
        std::process::exit(2);
    }
}
async fn run(args: Args, guild: String) -> Result<(), String> {
    if args
        .values
        .keys()
        .any(|k| !["guild", "from", "to", "format"].contains(&k.as_str()))
        || args.flags.iter().any(|k| k != "allow-live-guild")
        || !args.positionals.is_empty()
    {
        return Err("unknown or malformed argument".into());
    }
    let from = args.get("from").ok_or("--from required")?;
    let to = args.get("to").ok_or("--to required")?;
    window(from, to)?;
    let format = args.get("format").unwrap_or("json");
    if !["json", "csv"].contains(&format) {
        return Err("--format must be json or csv".into());
    }
    let db = open_db(&args, true).await;
    let rows = list_flagged(db.pool(), &guild, from, to)
        .await
        .map_err(|_| {
            "query failed; check authorized database/schema (no migrations are applied)"
        })?;
    let output = if format == "csv" {
        cohort_csv(&rows)?
    } else {
        serde_json::to_string_pretty(&rows).map_err(|_| "cannot encode report")?
    };
    db.close().await;
    println!("{output}");
    Ok(())
}
