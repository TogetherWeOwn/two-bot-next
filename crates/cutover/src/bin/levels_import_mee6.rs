//! MEE6 XP import CLI (legacy `scripts/levels-import-mee6.ts`).
//!
//! Dry run is the default; `--apply` is the only thing that writes.
//! Exit codes: 0 fine, 1 the export or the write did not reconcile,
//! 2 usage / refused guild.

use two_bot_cutover::cli::{now_iso, open_db, require_guild, require_guild_read, Args};
use two_bot_cutover::{run_mee6_import, ImportError};

fn usage() -> ! {
    eprintln!(
        "Usage: levels-import-mee6 [import] --guild <snowflake> --file <export.json>\n                                  [--apply] [--allow-lower] [--manifest <path>] [--allow-live-guild]\n       levels-import-mee6 inventory --guild <snowflake>\n\
         \nAccepted JSON: an array of players, or {{players:[...]}}; each player needs id/user_id and xp.\n\
         Without --apply nothing is written and the full manifest is still produced."
    );
    std::process::exit(2);
}

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = Args::parse(&argv);
    let command = args
        .positionals
        .first()
        .map(String::as_str)
        .filter(|c| *c == "inventory" || *c == "import")
        .unwrap_or("import");

    if command == "inventory" {
        let guild = require_guild_read(&args, "guild");
        let db = open_db(&args, true).await;
        match two_bot_cutover::mee6_xp_inventory(&db, &guild).await {
            Ok(inv) => println!("{}", serde_json::to_string_pretty(&inv).unwrap_or_default()),
            Err(e) => {
                eprintln!("inventory failed: {e}");
                std::process::exit(1);
            }
        }
        db.close().await;
        return;
    }

    let guild = require_guild(&args, "guild");
    let Some(file_path) = args.values.get("file") else {
        usage();
    };
    let file_bytes = match std::fs::read(file_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("cannot read {file_path}: {e}");
            std::process::exit(2);
        }
    };

    let apply = args.has("apply");
    let db = open_db(&args, !apply).await;
    let manifest = match run_mee6_import(
        &db,
        &guild,
        file_path,
        &file_bytes,
        apply,
        args.has("allow-lower"),
        &now_iso(),
    )
    .await
    {
        Ok(m) => m,
        Err(ImportError::Export(e)) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("import failed: {e}");
            std::process::exit(1);
        }
    };
    db.close().await;

    let rendered = serde_json::to_string_pretty(&manifest).unwrap_or_default();
    if let Some(path) = args.values.get("manifest") {
        if let Err(e) = std::fs::write(path, format!("{rendered}\n")) {
            eprintln!("cannot write manifest {path}: {e}");
            std::process::exit(1);
        }
    }
    println!("{rendered}");
    if !manifest.reconciled {
        eprintln!(
            "Import did not reconcile:\n  {}",
            manifest.reconciliation_errors.join("\n  ")
        );
        std::process::exit(1);
    }
}
