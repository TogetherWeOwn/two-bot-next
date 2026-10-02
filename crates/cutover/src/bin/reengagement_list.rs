//! On-demand reengagement list (parity §4/§9: on-demand CLI only, never
//! scheduled; §9 drops the scheduled runtime but keeps the on-demand query).
//!
//! Prints the inactivity selector's quiet members for one guild as CSV on
//! stdout (header always; a `#`-prefixed hint line and exit 0 when empty).
//! Read-only: no `member_inactive` events, no `inactive_flagged_at`
//! projection, no timer/service/job registration — an operator runs this once
//! and it exits. Nothing here messages anybody (parity forbids DMs).
//!
//! `reengagement-list --guild <ID> [--days <N>] [--now <RFC3339>] [--allow-live-guild]`
//! Env: `TWO_DATABASE_URL` (required), `TWO_INACTIVITY_DAYS` (fallback for
//! `--days`, legacy default 14).

use two_bot_core::inactivity::{parse_inactivity_days, render_reengagement_csv, InactivityOutcome};
use two_bot_cutover::{
    cli::{now_iso, open_db, require_guild, Args},
    reengagement::list_reengagement,
};

#[tokio::main]
async fn main() {
    let args = Args::parse(&std::env::args().skip(1).collect::<Vec<_>>());
    if args.has("help") {
        println!("Usage: reengagement-list --guild <ID> [--days <N>] [--now <RFC3339>] [--allow-live-guild]\nOn-demand reengagement list (never scheduled): quiet members past the inactivity cutoff as CSV on stdout. TWO_DATABASE_URL required.");
        return;
    }
    let guild = require_guild(&args, "guild");
    if let Err(e) = run(args, guild).await {
        eprintln!("reengagement-list: {e}");
        std::process::exit(2);
    }
}

async fn run(args: Args, guild: String) -> Result<(), String> {
    if args
        .values
        .keys()
        .any(|k| !["guild", "days", "now"].contains(&k.as_str()))
        || args.flags.iter().any(|k| k != "allow-live-guild")
        || !args.positionals.is_empty()
    {
        return Err("unknown or malformed argument".into());
    }
    // Explicit `--days` wins; otherwise the legacy `TWO_INACTIVITY_DAYS`
    // gate; otherwise the 14-day default — one parser for all three.
    let days_raw = args
        .get("days")
        .map(str::to_owned)
        .or_else(|| std::env::var("TWO_INACTIVITY_DAYS").ok());
    let days = parse_inactivity_days(days_raw.as_deref())
        .map_err(|_| "--days must be a non-negative integer of days")?;
    let now = args.get("now").unwrap_or_default();
    let now = if now.is_empty() {
        now_iso()
    } else {
        two_bot_core::funnel::parse_iso_millis(now)
            .map(|_| now.to_owned())
            .ok_or("--now must be RFC3339")?
    };
    let db = open_db(&args, true).await;
    let rows = list_reengagement(db.pool(), &guild, &now, days)
        .await
        .map_err(|_| {
            "query failed; check authorized database/schema (no migrations are applied)"
        })?;
    // Fail closed on non-snowflake IDs: the renderer enforces the snowflake
    // contract and returns the error here (exit 2), emitting nothing.
    let output = render_reengagement_csv(&InactivityOutcome { flagged: rows })
        .map_err(|_| "refusing to emit a non-snowflake ID")?;
    db.close().await;
    print!("{output}");
    Ok(())
}
