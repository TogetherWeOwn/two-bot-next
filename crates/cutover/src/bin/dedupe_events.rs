//! Double-logged join/leave dedupe CLI (legacy `scripts/dedupe-events.ts`).
//!
//! The first backfill wrote one row per log entry while several logging bots
//! ran at once, so single joins arrived twice seconds apart from different
//! channels. `backfill` no longer creates them; this clears rows already
//! written. The kept row is always the earliest of each cluster.
//!
//! `dedupe-events --guild <snowflake>` counts without writing or migrating.
//! Deleting copies in 200-id chunks requires bare `--apply`; the live guild
//! additionally requires bare `--allow-live-guild` even for a dry run.

// Operator CLI reports intentionally use stdout; runtime/library modules do not.
#![allow(clippy::print_stdout)]

use std::collections::HashSet;
use two_bot_cutover::cli::{open_db, require_guild, Args};
use two_bot_cutover::{collapse_cross_source_duplicates, DedupableEvent, DEFAULT_TOLERANCE_MS};

// Validate raw argv before the shared parser can fold duplicates or turn
// `--apply=false` into presence. This leaf accepts no valued opt-ins.
fn validate_options(argv: &[String]) -> Result<(), &'static str> {
    let mut seen = HashSet::new();
    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];
        let name = arg.split('=').next().unwrap_or_default();
        if !seen.insert(name) {
            return Err("duplicate option");
        }
        match name {
            "--guild" => {
                let guild = if let Some(value) = arg.strip_prefix("--guild=") {
                    value
                } else {
                    i += 1;
                    argv.get(i).map(String::as_str).unwrap_or_default()
                };
                if !two_bot_cutover::is_snowflake(guild)
                    || guild.starts_with('0')
                    || guild.parse::<u64>().is_err()
                {
                    return Err("--guild must be a Discord snowflake");
                }
            }
            "--apply" | "--dry-run" | "--allow-live-guild" => {
                if arg != name {
                    return Err("--apply, --dry-run and --allow-live-guild must be bare flags");
                }
            }
            _ => return Err("unknown option or unexpected positional argument"),
        }
        i += 1;
    }
    if !seen.contains("--guild") {
        return Err("missing --guild <snowflake>");
    }
    if seen.contains("--apply") && seen.contains("--dry-run") {
        return Err("--apply and --dry-run cannot be combined");
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if let Err(message) = validate_options(&argv) {
        eprintln!("{message}");
        std::process::exit(2);
    }
    let args = Args::parse(&argv);
    let guild = require_guild(&args, "guild");
    let dry_run = !args.flags.contains("apply");

    // A maintenance tool operates on existing schema, never migrates it.
    let db = open_db(&args, true).await;
    println!(
        "\nEvent de-duplication for guild {guild}{}",
        if dry_run {
            "  (DRY RUN - nothing will be deleted)"
        } else {
            ""
        }
    );
    println!();

    let mut found = 0usize;
    let mut deleted = 0u64;
    for event_type in ["member_join", "member_leave"] {
        let rows: Vec<(i64, Option<String>, String, String)> = sqlx::query_as(
            "SELECT id, member_id, to_char(occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), source FROM events
             WHERE event_type = $1 AND guild_id = $2 AND member_id IS NOT NULL
             ORDER BY occurred_at, id",
        )
        .bind(event_type)
        .bind(&guild)
        .fetch_all(db.pool())
        .await
        .unwrap_or_else(|e| {
            eprintln!("read failed: {e}");
            std::process::exit(1);
        });

        let events: Vec<DedupableEvent> = rows
            .iter()
            .map(|(_, member_id, occurred_at, source)| DedupableEvent {
                event_type: event_type.to_owned(),
                member_id: member_id.clone(),
                occurred_at: occurred_at.clone(),
                source: source.clone(),
            })
            .collect();
        let result = collapse_cross_source_duplicates(&events, DEFAULT_TOLERANCE_MS);
        let keep: HashSet<usize> = result.kept.into_iter().collect();
        let drop: Vec<i64> = rows
            .iter()
            .enumerate()
            .filter(|(i, _)| !keep.contains(i))
            .map(|(_, (id, _, _, _))| *id)
            .collect();
        let people: HashSet<_> = rows.iter().filter_map(|(_, m, _, _)| m.clone()).collect();
        found += drop.len();
        println!(
            "  {:13} {:>5} rows  {:>5} people  ->  {:>4} duplicate rows",
            event_type,
            rows.len(),
            people.len(),
            drop.len()
        );

        if dry_run || drop.is_empty() {
            continue;
        }
        for chunk in drop.chunks(200) {
            let mut qb = sqlx::QueryBuilder::new("DELETE FROM events WHERE guild_id = ");
            qb.push_bind(&guild).push(" AND id IN (");
            let mut sep = qb.separated(", ");
            for id in chunk {
                sep.push_bind(*id);
            }
            qb.push(")");
            let result = qb.build().execute(db.pool()).await.unwrap_or_else(|e| {
                eprintln!("delete failed: {e}");
                std::process::exit(1);
            });
            deleted += result.rows_affected();
        }
    }

    let remaining: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM events WHERE guild_id = $1")
        .bind(&guild)
        .fetch_one(db.pool())
        .await
        .unwrap_or_else(|e| {
            eprintln!("count failed: {e}");
            std::process::exit(1);
        });
    println!(
        "\n  {} rows; {} events {} on file for guild {guild}\n",
        if dry_run {
            format!("would delete {found}")
        } else {
            format!("deleted {deleted}")
        },
        remaining.0,
        if dry_run { "currently" } else { "now" }
    );
    db.close().await;
}
