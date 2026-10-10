//! Operator V11 voice configuration apply: the `/import` preview and Confirm
//! for an Operator with the database and bot credentials, so a cutover does
//! not depend on a Discord admin session (`docs/voice-cutover-rehearsal.md`).
//!
//! `voice-config-apply --guild <id> --file <v11.json>` reads a trusted guild
//! inventory from Discord (channels, roles, members; never from the file),
//! snapshots the stored configuration, and prints the same diff `/import`
//! would preview, with its content hash. It writes nothing unless `--apply`
//! is passed with `--expect-hash` set to the hash of the reviewed dry run, so
//! a configuration that changed after the dry run refuses (exit 3) instead of
//! being overwritten. The write is `PgVoiceConfigStore::apply`, whose
//! compare-and-swap also refuses a change between this run's snapshot and the
//! write. The live guild needs `--allow-live-guild`.
//!
//! Env: `DISCORD_TOKEN` (or `DISCORD_BOT_TOKEN`), `TWO_DATABASE_URL`.

// Operator CLI reports intentionally use stdout; runtime/library modules do not.
#![allow(clippy::print_stdout)]

use twilight_model::id::Id;
use two_bot_core::voice_config_diff::diff_configuration;
use two_bot_cutover::cli::{open_db, require_guild, Args};
use two_bot_cutover::voice_config_apply::{
    check_expected_hash, inventory_from_rest, plan_apply, ApplyPlan,
};
use two_bot_cutover::voice_config_store::PgVoiceConfigStore;
use two_bot_cutover::RestClient;

const USAGE: &str = "Usage: voice-config-apply --guild <id> --file <v11.json> \
[--apply --expect-hash <hash>] [--allow-live-guild] [--discord-base <url>]\n\
Prints the /import diff and its hash for the file against the stored voice configuration.\n\
Writes only with --apply, and only when --expect-hash equals the hash of the reviewed dry run.\n\
Env: DISCORD_TOKEN (or DISCORD_BOT_TOKEN), TWO_DATABASE_URL.";

fn usage_error(message: &str) -> ! {
    eprintln!("voice-config-apply: {message}\n{USAGE}");
    std::process::exit(2);
}

fn fail(message: &str) -> ! {
    eprintln!("voice-config-apply: {message}");
    std::process::exit(1);
}

async fn rest_client(proxy: Option<String>) -> RestClient {
    match proxy {
        Some(base) => RestClient::with_proxy("voice-config-apply-read".to_owned(), Some(base)),
        None => {
            let token = std::env::var("DISCORD_TOKEN")
                .or_else(|_| std::env::var("DISCORD_BOT_TOKEN"))
                .unwrap_or_default();
            if token.trim().is_empty() {
                fail(
                    "DISCORD_TOKEN (or DISCORD_BOT_TOKEN) is not set; the inventory read needs it",
                );
            }
            RestClient::from_env(token, None)
                .await
                .unwrap_or_else(|_| fail("Discord client unavailable; check admission authority"))
        }
    }
}

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = Args::parse(&argv);
    if args.has("help") || argv.is_empty() {
        println!("{USAGE}");
        return;
    }
    for key in args.values.keys() {
        if !matches!(
            key.as_str(),
            "guild" | "file" | "discord-base" | "expect-hash"
        ) {
            usage_error(&format!("unknown argument --{key}"));
        }
    }
    for flag in &args.flags {
        if !matches!(flag.as_str(), "apply" | "allow-live-guild") {
            usage_error(&format!("unknown argument --{flag}"));
        }
    }
    if !args.positionals.is_empty() {
        usage_error("unexpected positional argument");
    }
    // Canonical snowflake first (no sign, no leading zero), so the live-guild
    // fence and every later read/write see the same id.
    let guild = require_guild(&args, "guild");
    let guild_id: u64 = guild
        .parse()
        .unwrap_or_else(|_| usage_error("--guild must be a Discord snowflake"));
    let path = args
        .get("file")
        .unwrap_or_else(|| usage_error("--file is required"))
        .to_owned();
    let bytes = std::fs::read(&path).unwrap_or_else(|_| fail("cannot read --file"));
    let apply = args.has("apply");
    let expected_hash = args.values.get("expect-hash").cloned();
    if apply && expected_hash.is_none() {
        usage_error("--apply needs --expect-hash <hash> from the reviewed dry run");
    }

    // Trusted inventory first: a partial read refuses rather than validating
    // against an incomplete guild.
    let rest = rest_client(args.values.get("discord-base").cloned()).await;
    let guild_key = Id::new(guild_id);
    let discord_guild = match rest.guild(guild_key).await {
        Ok(Some(guild)) => guild,
        _ => fail("guild read refused; nothing was changed"),
    };
    let channels = match rest.guild_channels(guild_key).await {
        Ok(Some(channels)) => channels,
        _ => fail("channel read refused; nothing was changed"),
    };
    let members = match rest.fetch_all_members(guild_key).await {
        Ok(Some(members)) => members,
        _ => fail("member read refused or incomplete; nothing was changed"),
    };
    let inventory = inventory_from_rest(
        guild_id,
        &channels,
        discord_guild.roles.iter().map(|role| role.id.get()),
        members.iter().map(|member| member.user.id.get()),
    );
    let discord_requests = rest.requests();

    let db = open_db(&args, true).await;
    let store = PgVoiceConfigStore::new(db.pool().clone());
    let current = store
        .snapshot(guild_id)
        .await
        .unwrap_or_else(|_| fail("configuration snapshot failed; check database/schema"));

    let plan = plan_apply(&current, &bytes, &inventory);
    let (status, code) = match &plan {
        ApplyPlan::Refuse { message } => {
            eprintln!("voice-config-apply: refused: {message}");
            ("refused", 1)
        }
        ApplyPlan::NoChanges { text, .. } => {
            println!("{text}");
            ("no_changes", 0)
        }
        ApplyPlan::Changes { text, .. } => {
            println!("{text}");
            (if apply { "applying" } else { "dry_run" }, 0)
        }
    };
    let mut report = serde_json::json!({
        "tool": "voice-config-apply",
        "guild": guild,
        "status": status,
        "discordRequests": discord_requests,
        "inventory": {
            "channels": inventory.channels.len(),
            "roles": inventory.roles.len(),
            "members": inventory.members.len(),
        },
    });
    if let ApplyPlan::NoChanges {
        skipped_unknown_channels,
        ..
    }
    | ApplyPlan::Changes {
        skipped_unknown_channels,
        ..
    } = &plan
    {
        report["skipped_unknown_channels"] = serde_json::json!(skipped_unknown_channels);
    }
    if let ApplyPlan::Changes {
        candidate,
        hash,
        change_count,
        ..
    } = &plan
    {
        report["changes"] = serde_json::json!(change_count);
        report["hash"] = serde_json::json!(hash);
        if apply {
            if let Err(message) = check_expected_hash(hash, expected_hash.as_deref()) {
                report["status"] = serde_json::json!("conflict");
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).unwrap_or_default()
                );
                eprintln!("voice-config-apply: {message}");
                db.close().await;
                std::process::exit(3);
            }
            match store.apply(guild_id, candidate, &current).await {
                Ok(()) => {
                    let after = store
                        .snapshot(guild_id)
                        .await
                        .unwrap_or_else(|_| fail("applied, but the read-back snapshot failed"));
                    let residual = diff_configuration(&after, candidate, &inventory).change_count();
                    report["status"] = serde_json::json!("applied");
                    report["readback_residual_changes"] = serde_json::json!(residual);
                    if residual != 0 {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&report).unwrap_or_default()
                        );
                        db.close().await;
                        std::process::exit(1);
                    }
                }
                Err(sqlx::Error::RowNotFound) => {
                    report["status"] = serde_json::json!("conflict");
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&report).unwrap_or_default()
                    );
                    eprintln!(
                        "voice-config-apply: the stored configuration changed after the snapshot; nothing was changed, re-run for a fresh diff"
                    );
                    db.close().await;
                    std::process::exit(3);
                }
                Err(_) => {
                    db.close().await;
                    fail("apply failed and rolled back; nothing was changed");
                }
            }
        }
    }
    db.close().await;
    println!(
        "{}",
        serde_json::to_string_pretty(&report).unwrap_or_default()
    );
    std::process::exit(code);
}
