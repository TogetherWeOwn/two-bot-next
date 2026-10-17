//! On-demand read-only aggregate game-activity operator report.
//!
//! Prints one guild's per-game aggregates (game, total play ms, in-voice
//! ms, session counts) as one JSON object on stdout: the object shape
//! always prints, an empty guild adds a hint, and an unscoped invocation
//! (no `--guild`) prints the hint and exits 0 without opening a database.
//! Read-only: the only statement is the guild-scoped aggregate SELECT, the
//! pool opens with migrations off, and there is no scheduling, Discord
//! call, or write. Aggregates carry no per-person column, so neither does
//! this report.
//!
//! `game-activity-report --guild <ID> [--allow-live-guild]`
//! Env: `TWO_DATABASE_URL` (required when `--guild` is given).

use std::io::Write as _;

use two_bot_core::game_activity::{render_game_activity_report, render_game_activity_unscoped};
use two_bot_core::game_activity_store::read_guild_aggregates;
use two_bot_cutover::cli::{open_db, require_guild, Args};

// On-demand operator report: the JSON goes to stdout by contract (written
// through an explicit handle, not the print macros the workspace denies);
// diagnostics and usage errors use stderr.
fn emit(text: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

#[tokio::main]
async fn main() {
    let args = Args::parse(&std::env::args().skip(1).collect::<Vec<_>>());
    if args.has("help") {
        emit("Usage: game-activity-report --guild <ID> [--allow-live-guild]\nOn-demand read-only game-activity aggregate report (never scheduled): per-game totals for one guild as JSON on stdout. TWO_DATABASE_URL required. Without --guild: empty report plus hint, exit 0.\n");
        return;
    }
    if args.values.keys().any(|k| k != "guild")
        || args.flags.iter().any(|k| k != "allow-live-guild")
        || !args.positionals.is_empty()
    {
        eprintln!("game-activity-report: unknown or malformed argument");
        std::process::exit(2);
    }
    // Unscoped: exit 0 with the empty-state hint; no database is opened.
    if !args.values.contains_key("guild") {
        emit(&render_game_activity_unscoped());
        return;
    }
    let guild = require_guild(&args, "guild");
    if let Err(e) = run(&args, &guild).await {
        eprintln!("game-activity-report: {e}");
        std::process::exit(2);
    }
}

async fn run(args: &Args, guild: &str) -> Result<(), String> {
    let db = open_db(args, true).await;
    let rows = read_guild_aggregates(db.pool(), guild).await.map_err(|_| {
        "query failed; check authorized database/schema (no migrations are applied)"
    })?;
    let output = render_game_activity_report(guild, &rows);
    db.close().await;
    emit(&output);
    Ok(())
}
