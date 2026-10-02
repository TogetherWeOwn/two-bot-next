//! On-demand rules-gate stuck report (replaces the dropped
//! `two-bot-rules-gate-timeout.timer` daily runtime as the post-cutover
//! operator query; parity matrix section 4 row + section 9 drop 8).
//!
//! Legacy `scripts/rules-gate-timeout.ts` (`src/moderation/rulesGateTimeout.ts`)
//! selected members with Discord `pending === true` and Discord's own
//! `joined_at` at least fourteen days old (inclusive; TOG-412, TOG-479). The
//! two-bot-next `members` projection carries the same facts as `joined_at`
//! (arrival) and `gate_cleared_at` (first through-gate observation,
//! earliest-wins): a member is stuck when they joined, never cleared, and the
//! join is past the threshold. Members with no join timestamp cannot be
//! evaluated against the threshold, so they are listed separately as unknown
//! instead of guessed at (legacy held invalid `joined_at` back and named it).
//!
//! Read-only by construction: the only statements are SELECTs, the pool opens
//! with migrations off (a read-only tool must not build schema by accident),
//! and there is no `--execute` path -- moderation writes are out of scope.
//! Metadata only: member IDs plus pending-since timestamps. Never writes,
//! DMs, or pings.
//!
//! Usage: gate_stuck_report --guild <snowflake> [--threshold-days <n>]
//!   Env: TWO_DATABASE_URL. Exit 0 on a report (including clean), 1 on
//!   database failure, 2 on usage errors.

use two_bot_cutover::cli::{open_db, require_guild_read, Args};

/// Legacy `RULES_GATE_TIMEOUT_DAYS`: two weekends.
const DEFAULT_THRESHOLD_DAYS: i64 = 14;

fn usage() -> ! {
    eprintln!(
        "Usage: gate_stuck_report --guild <snowflake> [--threshold-days <n>]\n\
         Lists members stuck behind the rules gate past the threshold, oldest first.\n\
         Read-only: SELECTs only, no migrations, no writes, no DMs, no pings.\n\
         Env: TWO_DATABASE_URL."
    );
    std::process::exit(2);
}

fn threshold_days(args: &Args) -> i64 {
    let Some(raw) = args.values.get("threshold-days") else {
        return DEFAULT_THRESHOLD_DAYS;
    };
    match raw.parse::<i64>() {
        Ok(n) if n >= 1 => n,
        _ => {
            eprintln!("--threshold-days must be a positive integer number of days");
            std::process::exit(2);
        }
    }
}

struct StuckRow {
    member_id: String,
    /// Join time as pending-since, or None when the member has no join
    /// timestamp and cannot be evaluated against the threshold.
    pending_since: Option<String>,
}

async fn stuck_report(
    db: &two_bot_cutover::CutoverDb,
    guild: &str,
    threshold: i64,
) -> Vec<StuckRow> {
    // One statement: stuck rows carry their join time as pending-since;
    // rows with no join timestamp ride along as unknown. Bots and members
    // who left are out of scope (legacy scanned the live roster, where
    // neither appears as a pending target).
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT member_id,
                to_char(joined_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')
           FROM members
          WHERE guild_id = $1
            AND gate_cleared_at IS NULL
            AND left_at IS NULL
            AND NOT is_bot
            AND (joined_at IS NULL OR joined_at <= now() - ($2 * INTERVAL '1 day'))
          ORDER BY joined_at ASC NULLS LAST, member_id ASC",
    )
    .bind(guild)
    .bind(threshold)
    .fetch_all(db.pool())
    .await
    .unwrap_or_else(|e| {
        eprintln!("stuck report query failed: {e}");
        std::process::exit(1);
    });
    rows.into_iter()
        .map(|(member_id, pending_since)| StuckRow {
            member_id,
            pending_since,
        })
        .collect()
}

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = Args::parse(&argv);
    if args.has("help") {
        usage();
    }
    // Read-only inventory of member metadata: like the rewards probe's
    // stock-take, no live-guild refusal.
    let guild = require_guild_read(&args, "guild");
    let threshold = threshold_days(&args);
    let db = open_db(&args, true).await;
    let rows = stuck_report(&db, &guild, threshold).await;
    db.close().await;

    println!("Rules-gate stuck report -- guild {guild} (threshold: {threshold} days, read-only)");
    let mut stuck = 0usize;
    let mut unknown = 0usize;
    for row in &rows {
        match &row.pending_since {
            Some(since) => {
                stuck += 1;
                println!("  STUCK {} pending since {since}", row.member_id);
            }
            None => {
                unknown += 1;
                println!(
                    "  UNKNOWN-JOIN {} (no join timestamp; cannot evaluate)",
                    row.member_id
                );
            }
        }
    }
    if rows.is_empty() {
        println!("Clean: no members stuck past {threshold} days.");
    } else {
        println!(
            "{stuck} stuck past {threshold} days, {unknown} without a join timestamp. \
             Report only: no writes, kicks, DMs or pings performed."
        );
    }
}
