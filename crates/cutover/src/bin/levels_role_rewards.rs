//! Level-role rewards set/print CLI (legacy `scripts/levels-role-rewards.ts`).
//!
//! Without `--set`, prints the current rewards. `--set` replaces the full
//! configuration atomically.

use two_bot_cutover::cli::{open_db, require_guild, Args};
use two_bot_cutover::{is_snowflake, replace_role_rewards, role_rewards, LevelRoleReward};

fn usage() -> ! {
    eprintln!(
        "Usage: levels-role-rewards --guild <snowflake> [--set <level:roleId,...>] [--allow-live-guild]\n\
         Without --set, prints the current rewards. --set replaces the full configuration.\n\
         --allow-live-guild is only for an owner-approved rollout."
    );
    std::process::exit(2);
}

fn parse_rewards(spec: &str) -> Vec<LevelRoleReward> {
    if spec.trim().is_empty() {
        return Vec::new();
    }
    spec.split(',')
        .map(|entry| {
            let mut parts = entry.trim().split(':');
            let level: u64 = parts.next().unwrap_or("").parse().unwrap_or(0);
            let role_id = parts.next().unwrap_or("").to_owned();
            if level == 0 || !is_snowflake(&role_id) || parts.next().is_some() {
                usage();
            }
            LevelRoleReward { level, role_id }
        })
        .collect()
}

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = Args::parse(&argv);
    let guild = require_guild(&args, "guild");

    let db = open_db(&args, false).await;
    if let Some(set) = args.values.get("set") {
        if let Err(e) = replace_role_rewards(&db, &guild, &parse_rewards(set)).await {
            eprintln!("replace failed: {e}");
            std::process::exit(1);
        }
    }
    let rewards = match role_rewards(&db, &guild).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("read failed: {e}");
            std::process::exit(1);
        }
    };
    db.close().await;
    let out = serde_json::json!({ "guildId": guild, "rewards": rewards });
    println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
}
