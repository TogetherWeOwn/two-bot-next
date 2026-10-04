// Operator CLI reports intentionally use stdout; runtime/library modules do not.
#![allow(clippy::print_stdout)]

//! Ghost-channel cleanup (TOG-13564): reconcile tracked voice rooms against a
//! live-channel snapshot and clean up safely, dry-run first.
//!
//! The snapshot is a JSON array of `{"channel_id", "human_occupants",
//! "manageable"}` (the sibling read-only ghost count, TOG-13548, produces
//! it; Discord REST cannot report voice occupants, so it cannot be fetched
//! here). Tracked-but-gone rows are forgotten; empty manageable tracked
//! rooms are deleted; occupied, suspended and untracked channels are only
//! listed — untracked channels are never deleted by this tool.

use std::io::Read;
use std::path::Path;

use two_bot_core::raid_removal::{validate_execute_count, RemovalMode};
use two_bot_core::voice_ghost_cleanup::plan_ghost_cleanup;
use two_bot_cutover::cli::{open_db, require_guild, Args};
use two_bot_cutover::ghost_cleanup::{
    apply_deletes, apply_forgets, build_seed_ghost_cleanup, parse_snapshot, render_report,
    snapshot_to_seen, GhostApplyOutcome,
};
use two_bot_cutover::rest::RestClient;
use two_bot_cutover::voice_rooms::PgRoomStore;

const USAGE: &str = "Usage: ghost-cleanup --guild <ID> --channels <file|-> [--execute --expect N --reason <text>] [--allow-live-guild] [--seed]\nDefault: dry run (reads the tracked rows, prints the plan, writes nothing, needs no token). Execute deletes planned channels and forgets planned rows; --expect must match the plan action count. Untracked channels are listed, never deleted.";

fn usage_error(message: &str) -> ! {
    eprintln!("ghost-cleanup: {message}\n{USAGE}");
    std::process::exit(2);
}

async fn run(args: Args) -> Result<i32, String> {
    if !args.positionals.is_empty()
        || args
            .flags
            .iter()
            .any(|k| !["execute", "allow-live-guild", "seed"].contains(&k.as_str()))
        || args
            .values
            .keys()
            .any(|k| !["guild", "channels", "expect", "reason"].contains(&k.as_str()))
    {
        return Err("unknown or malformed argument".into());
    }
    let mode = if args.flags.contains("execute") {
        RemovalMode::Execute
    } else {
        RemovalMode::DryRun
    };
    if args.has("seed") && mode == RemovalMode::Execute {
        return Err("--seed never executes; drop --execute for the demo".into());
    }
    let expected = args
        .get("expect")
        .map(|v| v.parse::<usize>().map_err(|_| "invalid --expect"))
        .transpose()?;
    if mode == RemovalMode::DryRun && expected.is_some() {
        return Err("--expect is only meaningful with --execute".into());
    }

    if args.has("seed") {
        let (tracked, live) = build_seed_ghost_cleanup();
        let plan = plan_ghost_cleanup(&tracked, &snapshot_to_seen(&live));
        let report = render_report(
            "seed-guild",
            RemovalMode::DryRun,
            tracked.len(),
            &plan,
            &GhostApplyOutcome::default(),
        );
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|_| "cannot encode report")?
        );
        return Ok(0);
    }

    // Live-guild fence BEFORE any database or file access.
    let guild = require_guild(&args, "guild");
    let guild_id: u64 = guild
        .parse()
        .map_err(|_| "--guild must be a Discord snowflake")?;

    let source = args.get("channels").ok_or("--channels required")?;
    let reader: Box<dyn Read> = if source == "-" {
        Box::new(std::io::stdin())
    } else {
        Box::new(std::fs::File::open(Path::new(source)).map_err(|_| "cannot open snapshot")?)
    };
    let mut text = String::new();
    reader
        .take(8 * 1024 * 1024 + 1)
        .read_to_string(&mut text)
        .map_err(|_| "cannot read snapshot")?;
    if text.len() > 8 * 1024 * 1024 {
        return Err("snapshot exceeds 8 MiB".into());
    }
    let snapshot = parse_snapshot(&text)?;
    let seen = snapshot_to_seen(&snapshot);

    let db = open_db(&args, true).await;
    let store = PgRoomStore::new(db.pool().clone());
    let tracked = store.rooms_in_guild(guild_id).await.map_err(|_| {
        "tracked-room query failed; check authorized database/schema (no migrations are applied)"
    })?;

    let plan = plan_ghost_cleanup(&tracked, &seen);
    validate_execute_count(mode, expected, plan.action_count()).map_err(|e| e.to_owned())?;

    if mode == RemovalMode::DryRun {
        // The store holds a clone of this pool, so it stays open until the
        // last write lands; dry run writes nothing, so it can close here.
        db.close().await;
        let report = render_report(
            &guild,
            mode,
            tracked.len(),
            &plan,
            &GhostApplyOutcome::default(),
        );
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|_| "cannot encode report")?
        );
        return Ok(0);
    }

    let reason = args.get("reason").ok_or("execute requires --reason")?;
    // Discord counts audit reasons in UTF-16 units, like the moderation seam.
    if reason.trim().is_empty() || reason.encode_utf16().count() > 512 {
        return Err(
            "reason must be nonempty and within 512 characters (Discord audit bound)".into(),
        );
    }
    // No client or token access in dry-run mode. No alternate credentials.
    let token = std::env::var("DISCORD_TOKEN").map_err(|_| {
        "DISCORD_TOKEN required; ask its authorized provisioner, do not substitute credentials"
    })?;
    if token.trim().is_empty() {
        return Err("DISCORD_TOKEN is empty".into());
    }
    let client = RestClient::from_env(token, None)
        .await
        .map_err(|_| "cleanup client unavailable; check admission authority")?;

    let (forgot, mut failures) = apply_forgets(&store, guild_id, &plan.forget).await;
    let (deleted, delete_failures) =
        apply_deletes(&client, &store, guild_id, &plan.delete, reason).await;
    failures.extend(delete_failures);
    db.close().await;
    let outcome = GhostApplyOutcome {
        forgot,
        deleted,
        failed: failures,
        discord_requests: client.requests(),
    };
    let failed = !outcome.failed.is_empty();
    let report = render_report(&guild, mode, tracked.len(), &plan, &outcome);
    println!(
        "{}",
        serde_json::to_string_pretty(&report).map_err(|_| "cannot encode report")?
    );
    Ok(i32::from(failed))
}

#[tokio::main]
async fn main() {
    let args = Args::parse(&std::env::args().skip(1).collect::<Vec<_>>());
    if args.has("help") {
        println!("{USAGE}");
        return;
    }
    match run(args).await {
        Ok(code) => std::process::exit(code),
        Err(e) => usage_error(&e),
    }
}
