//! Local operator-only reconciliation; no gateway, REST client or migrations.

#![allow(clippy::print_stdout)]

use std::collections::HashMap;
use std::io::Write;

use two_bot_core::ChannelModerationStore;

pub const USAGE: &str = "\
  two-bot moderation release-channel --guild <id> --channel <id> --claim-key <key>
      Read-only inspection by default; prints the matching lane/request rows.
      Ownership tokens are redacted; expected_generation binds confirmation.
      To release, also pass --confirm-release --expected-generation <fingerprint>
      --operator <Discord-user-id> --reason <reconciliation-note>.
      First quiesce workers, wait for old REST requests to settle, and read the
      channel's actual Discord overwrites and slowmode. No automatic replay.
      Keeps the lockdown recovery seed and a terminal replay tombstone; audits
      the operator, database login, released key and previous state atomically.
      Env: TWO_DATABASE_URL (or DATABASE_URL only when unset), TWO_DATABASE_TLS.
      Exit 0: inspected/released; 1: refused/DB failure; 2: invalid arguments.
";

struct Options {
    guild: String,
    channel: String,
    claim_key: String,
    confirmation: Option<Confirmation>,
}

struct Confirmation {
    generation: String,
    operator: String,
    reason: String,
}

fn snowflake(value: &str) -> bool {
    value
        .parse::<u64>()
        .is_ok_and(|id| id != 0 && id.to_string() == value)
}

fn parse(args: &[String]) -> Result<Options, &'static str> {
    let mut values = HashMap::new();
    let mut confirm = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--confirm-release" {
            if confirm {
                return Err("duplicate confirmation flag");
            }
            confirm = true;
            continue;
        }
        if !matches!(
            arg.as_str(),
            "--guild"
                | "--channel"
                | "--claim-key"
                | "--expected-generation"
                | "--operator"
                | "--reason"
        ) {
            return Err("unknown release-channel option");
        }
        let value = args.next().ok_or("missing release-channel option value")?;
        if value.starts_with("--") || values.insert(arg.as_str(), value.clone()).is_some() {
            return Err("missing value or duplicate release-channel option");
        }
    }
    let guild = values.remove("--guild").ok_or("--guild is required")?;
    let channel = values.remove("--channel").ok_or("--channel is required")?;
    let claim_key = values
        .remove("--claim-key")
        .ok_or("--claim-key is required")?;
    if !snowflake(&guild) || !snowflake(&channel) {
        return Err("guild and channel must be canonical nonzero snowflakes");
    }
    if claim_key.is_empty()
        || claim_key.len() > 256
        || claim_key
            .chars()
            .any(|c| c.is_control() || c.is_whitespace())
    {
        return Err("claim key must be nonblank, bounded and contain no whitespace");
    }
    let confirmation = if confirm {
        let generation = values
            .remove("--expected-generation")
            .ok_or("confirmation requires --expected-generation from inspection")?;
        let operator = values
            .remove("--operator")
            .ok_or("confirmation requires --operator")?;
        let reason = values
            .remove("--reason")
            .ok_or("confirmation requires --reason")?;
        if generation.len() != 64
            || !generation
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("expected generation must be the inspected SHA-256 fingerprint");
        }
        if !snowflake(&operator) {
            return Err("operator must be a canonical nonzero Discord user snowflake");
        }
        if reason.trim().is_empty() || reason.len() > 1024 || reason.chars().any(char::is_control) {
            return Err("reconciliation reason must be nonblank, bounded and contain no controls");
        }
        Some(Confirmation {
            generation,
            operator,
            reason: reason.trim().to_owned(),
        })
    } else {
        if !values.is_empty() {
            return Err("confirmation options require --confirm-release");
        }
        None
    };
    Ok(Options {
        guild,
        channel,
        claim_key,
        confirmation,
    })
}

pub async fn dispatch(args: &[String]) -> i32 {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{USAGE}");
        return 0;
    }
    let options = match parse(args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("moderation release-channel: {message}\n{USAGE}");
            return 2;
        }
    };
    let url = match std::env::var("TWO_DATABASE_URL") {
        Ok(url) if !url.trim().is_empty() => url,
        Ok(_) | Err(std::env::VarError::NotUnicode(_)) => {
            eprintln!("moderation release-channel: invalid TWO_DATABASE_URL");
            return 2;
        }
        Err(std::env::VarError::NotPresent) => match std::env::var("DATABASE_URL") {
            Ok(url) if !url.trim().is_empty() => url,
            _ => {
                eprintln!("moderation release-channel: database URL required");
                return 2;
            }
        },
    };
    // The existing bounded connector enforces TLS and redacts connection errors.
    // Skipping migrations prevents accidental DDL; inspect never performs DML.
    let database = match two_bot_cutover::connect(&url, 1, true).await {
        Ok(database) => database,
        Err(_) => {
            eprintln!("moderation release-channel: database connection failed (details redacted)");
            return 1;
        }
    };
    let store = ChannelModerationStore::from_pool(database.pool().clone());
    let result = inspect_or_release(&store, &options).await;
    database.pool().close().await;
    result
}

async fn inspect_or_release(store: &ChannelModerationStore, options: &Options) -> i32 {
    let inspection = match store
        .inspect_channel_lane(&options.guild, &options.channel, &options.claim_key)
        .await
    {
        Ok(Some(inspection)) => inspection,
        Ok(None) => {
            eprintln!("moderation release-channel: no matching channel lane and claim");
            return 1;
        }
        Err(_) => {
            eprintln!("moderation release-channel: inspection failed (details redacted)");
            return 1;
        }
    };
    println!("{}", inspection.report());
    if std::io::stdout().flush().is_err() {
        eprintln!("moderation release-channel: could not print inspection; no release attempted");
        return 1;
    }
    let Some(confirmation) = &options.confirmation else {
        println!("INSPECTION ONLY: no changes");
        return 0;
    };
    if inspection.generation() != confirmation.generation || !inspection.releasable() {
        eprintln!(
            "moderation release-channel: stale, completed or inconsistent claim; inspect again"
        );
        return 1;
    }
    match store
        .force_release_channel_lane(&inspection, &confirmation.operator, &confirmation.reason)
        .await
    {
        Ok(Some(audit_id)) => {
            println!(
                "{}",
                serde_json::json!({"released": true, "audit_request_id": audit_id, "recovery_seed": "untouched"})
            );
            0
        }
        Ok(None) => {
            eprintln!("moderation release-channel: generation/state changed; no release committed");
            1
        }
        Err(_) => {
            eprintln!("moderation release-channel: release failed or commit outcome unknown (details redacted); inspect before retrying");
            1
        }
    }
}
