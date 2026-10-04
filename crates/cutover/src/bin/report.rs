//! Read-only cutover integrity reports (TOG-11152) plus the voice ghost-channel
//! count (TOG-13548).
//!
//! `report voice-reconcile` pairs `voice_session_start` / `voice_session_end`
//! halves per (guild, member) and recovers durations where the stored rows
//! allow it (port of legacy `scripts/voice-reconcile.ts` + `src/analytics/voiceReconcile.ts`).
//! `report leave-gap` classifies members with a `member_join` row, no
//! `member_leave` row, and gone from the roster (port of legacy
//! `scripts/leave-gap.ts` + `src/analytics/memberLeaveGap.ts`).
//! `report voice-ghosts` diffs tracked `voice_rooms` rows against the guild's
//! live voice channels: tracked-present, tracked-gone (the runtime
//! `reconcile` forget class) and untracked-present. Existence only —
//! Discord REST lists channels but not their voice occupants, so occupancy
//! classes belong to a gateway-derived snapshot, not to this report.
//!
//! Read-only by construction: the only SQL is SELECT (the pool opens with
//! migrations off, so the tool cannot build schema by accident), the single
//! roster read is a bounded GET, and the fill rule in the leave-gap report is
//! a proposal, executed nowhere. There is no repair path. On the
//! `--discord-base` loopback path the roster uses the ungoverned transport and
//! the run writes nothing anywhere; on the production path the roster uses the
//! shared durable send admission (one lane claim per request, same as every
//! other REST-calling operator tool), while report data itself is never written.
//!
//! Usage:
//!
//! ```text
//! report voice-reconcile --guild <snowflake> [--days <N>] [--seed]
//! report leave-gap --guild <snowflake> [--days <N>] [--floor <ISO>]
//!   [--discord-base <url>] [--seed]
//! report voice-ghosts --guild <snowflake> [--discord-base <url>] [--seed]
//! ```
//!
//! Env: TWO_DATABASE_URL; leave-gap live mode also needs DISCORD_TOKEN (or
//! DISCORD_BOT_TOKEN); voice-ghosts live mode needs it too (one channel-list
//! GET). Exit 0 on a report (gaps are findings, not failure),
//! 1 on database/roster failure, 2 on usage errors, 3 on a bad --floor.

// Operator CLI reports intentionally use stdout; runtime/library modules do not.
#![allow(clippy::print_stdout)]

use twilight_model::id::marker::GuildMarker;
use twilight_model::id::Id;
use two_bot_core::funnel::{format_iso_millis, parse_iso_millis};
use two_bot_cutover::cli::{now_iso, open_db, require_guild_read, Args};
use two_bot_cutover::leave_gap::{
    build_seed_gap_data, classify_leave_gaps, fetch_leave_gap_feeds, RosterMember,
};
use two_bot_cutover::rest::RestClient;
use two_bot_cutover::voice_ghosts::{build_seed_ghost_data, count_ghosts, is_live_voice_kind};
use two_bot_cutover::voice_reconcile::{
    build_seed_halves, fetch_voice_halves, reconcile_voice_halves,
};
use two_bot_cutover::voice_rooms::PgRoomStore;

/// Roster pages are full member JSON: cap the scan or a large guild costs
/// unbounded reads (legacy `ROSTER_MAX_PAGES = 20`, 1000 members/page).
const ROSTER_MAX_PAGES: u32 = 20;
const ROSTER_MAX_MEMBERS: usize = 20_000;

const USAGE: &str = "Usage:\n  \
    report voice-reconcile --guild <snowflake> [--days <N>] [--seed]\n  \
    report leave-gap --guild <snowflake> [--days <N>] [--floor <ISO>] [--discord-base <url>] [--seed]\n  \
    report voice-ghosts --guild <snowflake> [--discord-base <url>] [--seed]\n  \
    (a bare `report voice-reconcile 30` means the last 30 days, as in legacy)\n\
    Read-only integrity reports as JSON on stdout (SELECTs plus one bounded roster/channel GET at most;\n\
    never writes report data). Env: TWO_DATABASE_URL; leave-gap and voice-ghosts live mode also need\n\
    DISCORD_TOKEN (or DISCORD_BOT_TOKEN). Exit 0 report printed; 1 database/roster failure;\n\
    2 usage error; 3 bad --floor.";

fn usage_error(message: &str) -> ! {
    eprintln!("report: {message}\n{USAGE}");
    std::process::exit(2);
}

/// Last N days as a `since` instant, or None for the full-history sweep.
/// A day count bounds every feed, so a start just before `since` with its end
/// inside reads as no-start-on-file: default to full history to certify.
/// Accepts `--days N` or a bare legacy `N` positional (never both).
fn window_days(args: &Args) -> (Option<i64>, Option<String>) {
    let bare = match args.positionals.as_slice() {
        [_] => None,
        [_, days] => Some(days.clone()),
        _ => usage_error("unexpected positional argument"),
    };
    let raw = match (args.values.get("days"), bare) {
        (None, None) => return (None, None),
        (Some(flag), None) => flag.clone(),
        (None, Some(positional)) => positional,
        (Some(_), Some(_)) => usage_error("pass days once (--days N or a bare N, not both)"),
    };
    match raw.parse::<i64>() {
        Ok(n) if n >= 1 => {
            let now_ms = parse_iso_millis(&now_iso()).unwrap_or(0);
            let since_ms = now_ms.saturating_sub(n.saturating_mul(86_400_000));
            (Some(n), Some(format_iso_millis(since_ms)))
        }
        _ => usage_error(&format!(
            "Bad day count \"{raw}\". Use a positive number of days, e.g. 30."
        )),
    }
}

fn window_json(days: Option<i64>, since: Option<&str>) -> serde_json::Value {
    serde_json::json!({"days": days, "since": since})
}

async fn voice_reconcile(args: Args) -> i32 {
    for key in args.values.keys() {
        if !matches!(key.as_str(), "guild" | "days") {
            usage_error(&format!("unknown argument --{key}"));
        }
    }
    for flag in &args.flags {
        if flag != "seed" {
            usage_error(&format!("unknown argument --{flag}"));
        }
    }
    // window_days validates the positionals (subcommand plus optional days).
    let seeded = args.has("seed");
    let (days, since) = window_days(&args);

    if seeded {
        let now_ms = parse_iso_millis(&now_iso()).unwrap_or(0);
        let halves = build_seed_halves(now_ms);
        let result = reconcile_voice_halves(&halves.starts, &halves.ends, &halves.leaves);
        let report = serde_json::json!({
            "tool": "voice-reconcile",
            "mode": "seeded-demo",
            "guild": "seed-guild",
            "window": window_json(days, since.as_deref()),
            "resolved": result.resolved,
            "unresolvable": result.unresolvable,
            "complete": result.complete,
            "skipped": result.skipped,
            "discordRequests": 0,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
        return 0;
    }

    let guild = require_guild_read(&args, "guild");
    let db = open_db(&args, true).await;
    let halves = fetch_voice_halves(db.pool(), Some(guild.as_str()), since.as_deref())
        .await
        .unwrap_or_else(|_| {
            eprintln!("report: query failed; check authorized database/schema (no migrations are applied)");
            std::process::exit(1);
        });
    db.close().await;
    let result = reconcile_voice_halves(&halves.starts, &halves.ends, &halves.leaves);
    let report = serde_json::json!({
        "tool": "voice-reconcile",
        "guild": guild,
        "window": window_json(days, since.as_deref()),
        "resolved": result.resolved,
        "unresolvable": result.unresolvable,
        "complete": result.complete,
        "skipped": result.skipped,
        "discordRequests": 0,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&report).unwrap_or_default()
    );
    0
}

/// One bounded roster read: at most [`ROSTER_MAX_PAGES`] pages /
/// [`ROSTER_MAX_MEMBERS`] members. A ceiling hit or an unreadable page is an
/// error, never a partial roster presented as complete. Returns the roster
/// plus the run's own Discord request count.
async fn bounded_roster(guild: &str, proxy: Option<String>) -> (Vec<RosterMember>, u64) {
    let guild_id: Id<GuildMarker> = match guild.parse::<u64>().map(Id::new) {
        Ok(id) => id,
        Err(_) => {
            eprintln!("report: --guild must be a Discord snowflake");
            std::process::exit(2);
        }
    };
    // Loopback mock: ungoverned transport, zero writes. Production: the
    // shared durable send admission, same as every REST-calling operator tool.
    let rest = match proxy {
        Some(base) => RestClient::with_proxy("report-roster-read".to_owned(), Some(base)),
        None => {
            let token = std::env::var("DISCORD_TOKEN")
                .or_else(|_| std::env::var("DISCORD_BOT_TOKEN"))
                .unwrap_or_default();
            if token.trim().is_empty() {
                eprintln!("report: DISCORD_TOKEN (or DISCORD_BOT_TOKEN) is not set. The roster read needs it.");
                std::process::exit(1);
            }
            match RestClient::from_env(token, None).await {
                Ok(client) => client,
                Err(_) => {
                    eprintln!("report: roster client unavailable; check admission authority");
                    std::process::exit(1);
                }
            }
        }
    };
    match rest
        .fetch_all_members_bounded(guild_id, ROSTER_MAX_PAGES, ROSTER_MAX_MEMBERS)
        .await
    {
        Ok(Some(members)) => {
            let out: Vec<RosterMember> = members
                .iter()
                .map(|m| RosterMember {
                    guild_id: guild.to_owned(),
                    member_id: Some(m.user.id.get().to_string()),
                })
                .collect();
            let requests = rest.requests();
            eprintln!(
                "report: roster read complete ({} members, {requests} discord requests)",
                out.len(),
            );
            (out, requests)
        }
        Ok(None) => {
            eprintln!(
                "report: roster read failed - refusing the empty result instead of counting it."
            );
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("report: roster read refused ({e}) - a partial roster is never presented as complete.");
            std::process::exit(1);
        }
    }
}

async fn leave_gap(args: Args) -> i32 {
    for key in args.values.keys() {
        if !matches!(key.as_str(), "guild" | "days" | "floor" | "discord-base") {
            usage_error(&format!("unknown argument --{key}"));
        }
    }
    for flag in &args.flags {
        if flag != "seed" {
            usage_error(&format!("unknown argument --{flag}"));
        }
    }
    // window_days validates the positionals (subcommand plus optional days).
    let seeded = args.has("seed");
    let (days, since) = window_days(&args);
    // The log floor comes from the flag, not a guess: without it every gap
    // reads as log-miss with the floor-unknown note rather than a fabricated
    // pre-coverage label.
    let floor: Option<String> = match args.values.get("floor") {
        None => None,
        Some(raw) => match parse_iso_millis(raw) {
            Some(_) => Some(raw.clone()),
            None => {
                eprintln!("report: Bad --floor \"{raw}\". Pass the backfill's oldest scanned timestamp, e.g. --floor=2023-01-01T00:00:00.000Z.");
                std::process::exit(3);
            }
        },
    };

    if seeded {
        let (joins, leaves, roster, seed_floor) = build_seed_gap_data();
        let result = classify_leave_gaps(&joins, &leaves, &roster, Some(seed_floor.as_str()));
        let fills: usize = result.gaps.iter().map(|g| g.fills.len()).sum();
        let report = serde_json::json!({
            "tool": "leave-gap",
            "mode": "seeded-demo",
            "guild": "seed-guild",
            "window": window_json(days, since.as_deref()),
            "logFloor": seed_floor,
            "gaps": result.gaps,
            "present": result.present,
            "resolved": result.resolved,
            "skipped": result.skipped,
            "fillsProposed": fills,
            "discordRequests": 0,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
        return 0;
    }

    let guild = require_guild_read(&args, "guild");
    let proxy = args.values.get("discord-base").cloned();
    let db = open_db(&args, true).await;
    let feeds = fetch_leave_gap_feeds(db.pool(), Some(guild.as_str()), since.as_deref())
        .await
        .unwrap_or_else(|_| {
            eprintln!("report: query failed; check authorized database/schema (no migrations are applied)");
            std::process::exit(1);
        });
    // Close before the network: the DB feeds are in hand, and a stalled
    // roster must not hold pool connections.
    db.close().await;
    let (roster, discord_requests) = bounded_roster(&guild, proxy).await;
    let result = classify_leave_gaps(&feeds.joins, &feeds.leaves, &roster, floor.as_deref());
    let fills: usize = result.gaps.iter().map(|g| g.fills.len()).sum();
    let mut report = serde_json::json!({
        "tool": "leave-gap",
        "guild": guild,
        "window": window_json(days, since.as_deref()),
        "logFloor": floor.as_deref(),
        "gaps": result.gaps,
        "present": result.present,
        "resolved": result.resolved,
        "skipped": result.skipped,
        "fillsProposed": fills,
        "discordRequests": discord_requests,
    });
    if since.is_some() {
        report["note"] = serde_json::Value::String(
            "windowed sweep: a join before the window with its leave inside it reads as a gap here. \
             Certify with the full-history sweep, not a narrowed one."
                .to_owned(),
        );
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&report).unwrap_or_default()
    );
    0
}

/// One bounded channel listing: the guild's channel ids filtered to live
/// voice kinds. A ceiling hit is impossible here (one page), but an
/// unreadable listing refuses loudly instead of diffing against a partial
/// listing presented as complete. Returns the ids plus the run's own
/// Discord request count.
async fn bounded_live_voice(guild: &str, proxy: Option<String>) -> (Vec<u64>, u64) {
    let guild_id: Id<GuildMarker> = match guild.parse::<u64>().map(Id::new) {
        Ok(id) => id,
        Err(_) => {
            eprintln!("report: --guild must be a Discord snowflake");
            std::process::exit(2);
        }
    };
    // Loopback mock: ungoverned transport, zero writes. Production: the
    // shared durable send admission, same as every REST-calling operator tool.
    let rest = match proxy {
        Some(base) => RestClient::with_proxy("report-channels-read".to_owned(), Some(base)),
        None => {
            let token = std::env::var("DISCORD_TOKEN")
                .or_else(|_| std::env::var("DISCORD_BOT_TOKEN"))
                .unwrap_or_default();
            if token.trim().is_empty() {
                eprintln!("report: DISCORD_TOKEN (or DISCORD_BOT_TOKEN) is not set. The channel read needs it.");
                std::process::exit(1);
            }
            match RestClient::from_env(token, None).await {
                Ok(client) => client,
                Err(_) => {
                    eprintln!("report: channel client unavailable; check admission authority");
                    std::process::exit(1);
                }
            }
        }
    };
    match rest.guild_channels(guild_id).await {
        Ok(Some(channels)) => {
            let live: Vec<u64> = channels
                .iter()
                .filter(|c| is_live_voice_kind(c.kind))
                .map(|c| c.id.get())
                .collect();
            let requests = rest.requests();
            eprintln!(
                "report: channel read complete ({} channels, {} live voice, {requests} discord requests)",
                channels.len(),
                live.len(),
            );
            (live, requests)
        }
        Ok(None) => {
            eprintln!(
                "report: channel read failed - refusing the empty result instead of counting it."
            );
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("report: channel read refused ({e}) - a partial listing is never presented as complete.");
            std::process::exit(1);
        }
    }
}

async fn voice_ghosts(args: Args) -> i32 {
    for key in args.values.keys() {
        if !matches!(key.as_str(), "guild" | "discord-base") {
            usage_error(&format!("unknown argument --{key}"));
        }
    }
    for flag in &args.flags {
        if flag != "seed" {
            usage_error(&format!("unknown argument --{flag}"));
        }
    }
    if !args.positionals.iter().all(|p| p == "voice-ghosts") {
        usage_error("unexpected positional argument");
    }
    let seeded = args.has("seed");
    if seeded {
        let (tracked, live) = build_seed_ghost_data();
        let counts = count_ghosts(&tracked, &live);
        let report = serde_json::json!({
            "tool": "voice-ghosts",
            "mode": "seeded-demo",
            "guild": "seed-guild",
            "tracked_rooms": tracked.len(),
            "tracked_present": counts.tracked_present.iter().map(u64::to_string).collect::<Vec<_>>(),
            "tracked_gone": counts.tracked_gone.iter().map(u64::to_string).collect::<Vec<_>>(),
            "untracked_present": counts.untracked_present.iter().map(u64::to_string).collect::<Vec<_>>(),
            "clean": counts.is_clean(),
            "discordRequests": 0,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
        return 0;
    }

    let guild = require_guild_read(&args, "guild");
    let proxy = args.values.get("discord-base").cloned();
    let db = open_db(&args, true).await;
    let store = PgRoomStore::new(db.pool().clone());
    let guild_id: u64 = guild.parse().unwrap_or_else(|_| {
        eprintln!("report: --guild must be a Discord snowflake");
        std::process::exit(2);
    });
    let tracked = store.rooms_in_guild(guild_id).await.unwrap_or_else(|_| {
        eprintln!(
            "report: query failed; check authorized database/schema (no migrations are applied)"
        );
        std::process::exit(1);
    });
    // Close before the network: the tracked rows are in hand, and a stalled
    // channel read must not hold pool connections.
    db.close().await;
    let (live, discord_requests) = bounded_live_voice(&guild, proxy).await;
    let counts = count_ghosts(&tracked, &live);
    let report = serde_json::json!({
        "tool": "voice-ghosts",
        "guild": guild,
        "tracked_rooms": tracked.len(),
        "live_voice_channels": live.len(),
        "tracked_present": counts.tracked_present.iter().map(u64::to_string).collect::<Vec<_>>(),
        "tracked_gone": counts.tracked_gone.iter().map(u64::to_string).collect::<Vec<_>>(),
        "untracked_present": counts.untracked_present.iter().map(u64::to_string).collect::<Vec<_>>(),
        "clean": counts.is_clean(),
        "discordRequests": discord_requests,
        "note": "existence only: occupant and manageability classes need a gateway-derived snapshot, not this report.",
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&report).unwrap_or_default()
    );
    0
}

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = Args::parse(&argv);
    if args.has("help") || argv.is_empty() {
        println!("{USAGE}");
        return;
    }
    // Owned first: the arms move `args`, so the subcommand must not borrow it.
    let subcommand = args.positionals.first().cloned().unwrap_or_default();
    let code = match subcommand.as_str() {
        "voice-reconcile" => voice_reconcile(args).await,
        "leave-gap" => leave_gap(args).await,
        "voice-ghosts" => voice_ghosts(args).await,
        "" => usage_error("pass a report (voice-reconcile | leave-gap | voice-ghosts)"),
        other => usage_error(&format!(
            "unknown report \"{other}\" (voice-reconcile | leave-gap | voice-ghosts)"
        )),
    };
    std::process::exit(code);
}
