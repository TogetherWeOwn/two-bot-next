//! Double-logged join/leave dedupe CLI (legacy `scripts/dedupe-events.ts`).
//!
//! The first backfill wrote one row per log entry while several logging bots
//! ran at once, so single joins arrived twice seconds apart from different
//! channels. `backfill` no longer creates them; this clears rows already
//! written. The kept row is always the earliest of each cluster.
//!
//! `dedupe-events --dry-run` counts, changes nothing; without it, deletes
//! the copies in 200-id chunks.

use std::collections::HashSet;
use two_bot_cutover::cli::{open_db, Args};
use two_bot_cutover::{collapse_cross_source_duplicates, DedupableEvent, DEFAULT_TOLERANCE_MS};

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = Args::parse(&argv);
    let dry_run = args.has("dry-run");

    let db = open_db(&args, false).await;
    println!(
        "\nEvent de-duplication{}",
        if dry_run {
            "  (DRY RUN - nothing will be deleted)"
        } else {
            ""
        }
    );
    println!();

    let mut found = 0usize;
    let mut deleted = 0usize;
    for event_type in ["member_join", "member_leave"] {
        let rows: Vec<(i64, Option<String>, String, String)> = sqlx::query_as(
            "SELECT id, member_id, to_char(occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), source FROM events
             WHERE event_type = $1 AND member_id IS NOT NULL",
        )
        .bind(event_type)
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
            let mut qb = sqlx::QueryBuilder::new("DELETE FROM events WHERE id IN (");
            let mut sep = qb.separated(", ");
            for id in chunk {
                sep.push_bind(*id);
            }
            qb.push(")");
            qb.build().execute(db.pool()).await.unwrap_or_else(|e| {
                eprintln!("delete failed: {e}");
                std::process::exit(1);
            });
            deleted += chunk.len();
        }
    }

    let remaining: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM events")
        .fetch_one(db.pool())
        .await
        .unwrap_or((0,));
    println!(
        "\n  {} rows; {} events {} on file\n",
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
