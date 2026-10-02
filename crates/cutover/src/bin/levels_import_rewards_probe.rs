//! Reward-role dry-run probe CLI (legacy
//! `scripts/levels-import-rewards-probe.ts`).
//!
//! There is no `--apply`: no apply path exists in this binary or in the
//! module behind it, which is what makes "zero writes" a property rather
//! than a promise. The only database statement is the SELECT behind the
//! stored-rewards read, opened with migrations off. `--no-db` skips even
//! that. Roles come from a snapshot file, never the network.
//!
//! Exit codes: 0 fine, 1 the export is not importable or the report does
//! not balance, 2 usage or a refused guild.

// Operator CLI reports intentionally use stdout; runtime/library modules do not.
#![allow(clippy::print_stdout)]

use two_bot_cutover::cli::{open_db, require_guild, Args};
use two_bot_cutover::{
    is_snowflake, parse_mee6_role_rewards, parse_roles_snapshot, plan_reward_role_import,
    role_rewards,
};

fn usage() -> ! {
    eprintln!(
        "Usage: levels-import-rewards-probe --guild <snowflake> --file <export.json>\n           --roles <roles.json> --bot-id <snowflake> [--owner-id <snowflake>]\n           [--report <path>] [--require-all-mapped] [--no-db] [--allow-live-guild]\n\
         \nReads the role_rewards section of a MEE6 export and reports which rewards map onto\n\
         roles the bot can actually grant in the target guild. Writes nothing, ever."
    );
    std::process::exit(2);
}

fn required(args: &Args, name: &str) -> String {
    match args.values.get(name) {
        Some(v) => v.clone(),
        None => usage(),
    }
}

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = Args::parse(&argv);

    let guild = require_guild(&args, "guild");
    let bot_id = required(&args, "bot-id");
    if !is_snowflake(&bot_id) {
        usage();
    }
    if let Some(owner) = args.values.get("owner-id") {
        if !is_snowflake(owner) {
            usage();
        }
    }
    let file_path = required(&args, "file");
    let roles_path = required(&args, "roles");

    let export_text = match std::fs::read_to_string(&file_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("cannot read {file_path}: {e}");
            std::process::exit(2);
        }
    };
    let rewards = match parse_mee6_role_rewards(&export_text) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let roles_text = match std::fs::read_to_string(&roles_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("cannot read {roles_path}: {e}");
            std::process::exit(2);
        }
    };
    let roles = match parse_roles_snapshot(&roles_text) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("--roles {roles_path} is malformed: {e}");
            std::process::exit(1);
        }
    };

    let stored = if args.has("no-db") {
        None
    } else {
        let db = open_db(&args, true).await;
        let stored = match role_rewards(&db, &guild).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("stored rewards read failed: {e}");
                std::process::exit(1);
            }
        };
        db.close().await;
        Some(stored)
    };

    let report = plan_reward_role_import(
        &guild,
        &rewards,
        &roles,
        &bot_id,
        args.values.get("owner-id").map(String::as_str),
        stored.as_deref(),
    );
    let rendered = serde_json::to_string_pretty(&report).unwrap_or_default();
    if let Some(path) = args.values.get("report") {
        if let Err(e) = std::fs::write(path, format!("{rendered}\n")) {
            eprintln!("cannot write report {path}: {e}");
            std::process::exit(1);
        }
    }
    println!("{rendered}");

    if !report.counts.balances {
        eprintln!(
            "Report does not balance: {} rewards in, {} mapped + {} unmapped.",
            report.counts.rewards_in, report.counts.mapped, report.counts.unmapped
        );
        std::process::exit(1);
    }
    if args.has("require-all-mapped") && report.counts.unmapped > 0 {
        eprintln!(
            "{} of {} reward roles are unmapped:\n  {}",
            report.counts.unmapped,
            report.counts.rewards_in,
            report
                .unmapped
                .iter()
                .map(|r| format!("level {} ({:?}): {}", r.level, r.reason, r.detail))
                .collect::<Vec<_>>()
                .join("\n  ")
        );
        std::process::exit(1);
    }
}
