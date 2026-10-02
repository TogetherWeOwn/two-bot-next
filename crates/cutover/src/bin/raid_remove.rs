// Operator CLI reports intentionally use stdout; runtime/library modules do not.
#![allow(clippy::print_stdout)]

use std::{collections::HashSet, io::Read, path::Path};
use two_bot_core::{
    moderation::require_moderation_reason,
    raid_removal::{validate_execute_count, validate_targets, RemovalMode},
};
use two_bot_cutover::{
    cli::{require_guild, Args},
    raid_tools::{parse_targets, remove_accounts, FileAudit, RemovalRun},
};
use two_bot_discord::executor::ActionExecutor;

#[tokio::main]
async fn main() {
    let args = Args::parse(&std::env::args().skip(1).collect::<Vec<_>>());
    if args.has("help") {
        println!("Usage: raid-remove --guild <ID> --ids-from <file|-> --audit <JSONL> [--execute --expect N --reason <text>] [--protected-roles <IDs,comma-separated>] [--allow-live-guild]\nDefault: network-free dry run. Execute uses only DISCORD_TOKEN; kick only, never ban.");
        return;
    }
    let guild = require_guild(&args, "guild");
    match run(args, guild).await {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("raid-remove: {e}");
            std::process::exit(2);
        }
    }
}
async fn run(args: Args, guild: String) -> Result<i32, String> {
    if !args.positionals.is_empty()
        || args
            .flags
            .iter()
            .any(|k| !["execute", "allow-live-guild"].contains(&k.as_str()))
        || args.values.keys().any(|k| {
            ![
                "guild",
                "ids-from",
                "audit",
                "expect",
                "reason",
                "protected-roles",
            ]
            .contains(&k.as_str())
        })
    {
        return Err("unknown or malformed argument".into());
    }
    let mode = if args.flags.contains("execute") {
        RemovalMode::Execute
    } else {
        RemovalMode::DryRun
    };
    let source = args.get("ids-from").ok_or("--ids-from required")?;
    let audit = args.get("audit").ok_or("--audit required")?;
    if source == audit {
        return Err("input and audit must be different files".into());
    }
    let reader: Box<dyn Read> = if source == "-" {
        Box::new(std::io::stdin())
    } else {
        Box::new(std::fs::File::open(source).map_err(|_| "cannot open target list")?)
    };
    let mut text = String::new();
    reader
        .take(8 * 1024 * 1024 + 1)
        .read_to_string(&mut text)
        .map_err(|_| "cannot read target list")?;
    if text.len() > 8 * 1024 * 1024 {
        return Err("target file exceeds 8 MiB".into());
    }
    let ids = parse_targets(&text, &guild)?;
    let expected = args
        .get("expect")
        .map(|v| v.parse::<usize>().map_err(|_| "invalid --expect"))
        .transpose()?;
    validate_execute_count(mode, expected, ids.len())?;
    let reason = args.get("reason").unwrap_or("Raid cohort dry run");
    if mode == RemovalMode::Execute && args.get("reason").is_none() {
        return Err("execute requires --reason".into());
    }
    require_moderation_reason(reason)
        .map_err(|_| "reason must be nonempty and within the moderation audit bound")?;
    let protected: HashSet<_> = match args.get("protected-roles") {
        Some(v) => validate_targets(v.split(',').map(str::to_owned).collect())?
            .into_iter()
            .collect(),
        None => HashSet::new(),
    };
    let mut audit = FileAudit::open(Path::new(audit), &guild)?;
    // No client or token access in dry-run mode. No alternate credentials.
    let executor = if mode == RemovalMode::Execute {
        let token = std::env::var("DISCORD_TOKEN").map_err(|_| {
            "DISCORD_TOKEN required; ask its authorized provisioner, do not substitute credentials"
        })?;
        if token.trim().is_empty() {
            return Err("DISCORD_TOKEN is empty".into());
        }
        Some(ActionExecutor::new(token).map_err(|_| "cannot initialize Discord executor")?)
    } else {
        None
    };
    let done = audit.done.clone();
    let run_id = format!("{}-{}", two_bot_cutover::cli::now_iso(), std::process::id());
    let summary = remove_accounts(
        RemovalRun {
            guild: &guild,
            ids: &ids,
            mode,
            reason,
            run_id: &run_id,
            done: &done,
            protected: &protected,
        },
        executor.as_ref(),
        |r| audit.append(r),
    )
    .await?;
    println!(
        "{}",
        serde_json::to_string(&summary).map_err(|_| "cannot encode summary")?
    );
    Ok(i32::from(summary.failed > 0 || summary.aborted))
}
