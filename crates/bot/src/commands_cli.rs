//! Dry-run-first operator registry workflow, shared with opt-in boot publication.

// Operator CLI intentionally emits diff/publish reports and help to stdout.
#![allow(clippy::print_stdout)]

use std::collections::HashMap;
use std::sync::Arc;

use two_bot_core::commands::CommandDefinition;
use two_bot_core::feature_commands::FeatureGates;
use two_bot_core::moderation::ModerationGates;
use two_bot_core::router::{InteractionRouter, RouterGates, SurfaceFlags};
use two_bot_core::send_admission::{is_loopback_http, PgSendAdmission};
use two_bot_discord::{publish_commands, ActionExecutor};

pub const USAGE: &str = "\
  two-bot commands diff [--guild-id ID] [--application-id ID] [--allow-live-guild]
  two-bot commands publish [--apply] [--guild-id ID] [--application-id ID] [--allow-live-guild]
      Fetch and compare the guild command list against the compiled, feature-gated
      builtins. Both commands are read-only unless publish receives --apply.
      Env: DISCORD_TOKEN, GUILD_ID, DISCORD_APPLICATION_ID; feature gates as on boot.
      Live Discord targets additionally require TWO_DATABASE_URL (or DATABASE_URL
      when unset), the shared send-admission Postgres. No database is opened for
      loopback DISCORD_API_BASE fixtures, and no gateway is started.
      See docs/command-publish.md before applying a cutover.
";

/// Top-level `--help` surface: the registry usage plus the sibling CLI
/// usages the entrypoint used to print inline. Lives here (not in
/// `main.rs`) so the workspace `print_stdout` deny only touches CLI modules.
pub fn print_all_usage() {
    print!("{USAGE}");
    println!("{}", crate::preflight::USAGE);
    print!("{}", crate::erasure_cli::USAGE);
    print!("{}", crate::moderation_cli::USAGE);
}

#[derive(Debug, PartialEq, Eq)]
struct Options {
    application: u64,
    guild: u64,
    apply: bool,
}

fn snowflake(raw: Option<&str>, name: &str) -> Result<u64, String> {
    raw.and_then(|s| s.parse::<u64>().ok())
        .filter(|id| *id != 0)
        .ok_or_else(|| format!("{name} must be a nonzero Discord snowflake"))
}

fn guard_guild(guild: u64, allow_live: bool) -> Result<(), String> {
    if guild.to_string() == two_bot_cutover::LIVE_GUILD_ID && !allow_live {
        return Err(
            "Refusing live guild. Use --allow-live-guild only for an approved rollout.".into(),
        );
    }
    Ok(())
}

fn parse(args: &[String], vars: &HashMap<String, String>) -> Result<Options, String> {
    let Some(verb) = args.first().map(String::as_str) else {
        return Err(USAGE.into());
    };
    if !matches!(verb, "diff" | "publish") {
        return Err(USAGE.into());
    }
    let mut guild = vars.get("GUILD_ID").cloned();
    let mut application = vars.get("DISCORD_APPLICATION_ID").cloned();
    let mut apply = false;
    let mut allow_live = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--apply" if verb == "publish" => apply = true,
            "--allow-live-guild" => allow_live = true,
            "--guild-id" | "--application-id" => {
                let key = &args[i];
                i += 1;
                let value = args
                    .get(i)
                    .filter(|v| !v.starts_with("--"))
                    .ok_or_else(|| format!("missing value for {key}"))?;
                if key == "--guild-id" {
                    guild = Some(value.clone());
                } else {
                    application = Some(value.clone());
                }
            }
            other => return Err(format!("unknown commands option {other:?}\n{USAGE}")),
        }
        i += 1;
    }
    let guild = snowflake(guild.as_deref(), "GUILD_ID/--guild-id")?;
    // This fence precedes token validation, executor construction and any I/O.
    guard_guild(guild, allow_live)?;
    Ok(Options {
        guild,
        application: snowflake(
            application.as_deref(),
            "DISCORD_APPLICATION_ID/--application-id",
        )?,
        apply,
    })
}

fn desired_definitions(
    guild: u64,
    vars: &HashMap<String, String>,
) -> Result<Vec<CommandDefinition>, String> {
    let features = FeatureGates::from_map(vars).map_err(|e| e.to_string())?;
    let moderation = ModerationGates::from_map(vars).map_err(|e| e.to_string())?;
    let surfaces = SurfaceFlags {
        scorecard: vars
            .get("TWO_COMMUNITY_SCORECARD")
            .is_some_and(|v| v == "1"),
        ..SurfaceFlags::default()
    };
    InteractionRouter::new(RouterGates::from_slices(
        Some(guild),
        &features,
        &moderation,
        surfaces,
    ))
    .publish_set(&[])
    .map_err(|e| e.to_string())
}

/// Two-stage admission mirror of `preflight::admission_transport`: live (non-
/// loopback) targets build `PgSendAdmission` from TWO_DATABASE_URL and route
/// the executor through it; the explicit loopback fixture stays offline and
/// credential-free, and a present-but-failing authority never falls back.
/// Async because building admission opens the Postgres pool.
async fn executor(token: &str, vars: &HashMap<String, String>) -> Result<ActionExecutor, String> {
    if token.is_empty() {
        return Err("DISCORD_TOKEN is required".into());
    }
    // Proxy is the existing executor seam; no credentials are accepted as flags.
    let proxy = vars.get("DISCORD_API_BASE").cloned();
    if proxy.as_deref().is_some_and(is_loopback_http) {
        return ActionExecutor::with_proxy(token.to_owned(), proxy);
    }
    // Primary TWO_DATABASE_URL first; DATABASE_URL (the Container runtime's own
    // name for the same authority) as fallback; a set-but-empty primary is a
    // configuration error, never permission to try another credential.
    let url = match vars.get("TWO_DATABASE_URL") {
        Some(url) if !url.trim().is_empty() => url.clone(),
        Some(_) => {
            return Err("TWO_DATABASE_URL is set but empty".to_owned());
        }
        None => vars
            .get("DATABASE_URL")
            .filter(|url| !url.trim().is_empty())
            .cloned()
            .ok_or_else(|| "set TWO_DATABASE_URL before live command checks".to_owned())?,
    };
    let options = two_bot_core::database_url::connect_options(&url)
        .map_err(|_| "cannot configure send-admission Postgres".to_owned())?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options)
        .await
        .map_err(|_| "cannot configure send-admission Postgres".to_owned())?;
    let admission = PgSendAdmission::new(pool, token)
        .map_err(|_| "cannot configure send-admission Postgres".to_owned())?;
    ActionExecutor::with_admission(token.to_owned(), proxy, Arc::new(admission))
}

pub async fn dispatch(args: &[String]) -> i32 {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{USAGE}");
        return 0;
    }
    let vars: HashMap<String, String> = std::env::vars().collect();
    let options = match parse(args, &vars) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("{error}");
            return 2;
        }
    };
    let defs = match desired_definitions(options.guild, &vars) {
        Ok(defs) => defs,
        Err(error) => {
            eprintln!("{error}");
            return 2;
        }
    };
    let executor = match executor(vars.get("DISCORD_TOKEN").map_or("", String::as_str), &vars).await
    {
        Ok(executor) => executor,
        Err(error) => {
            eprintln!("cannot configure command REST client: {error}");
            return 2;
        }
    };
    match executor
        .sync_guild_commands(
            options.application,
            options.guild,
            &publish_commands(&defs),
            options.apply,
        )
        .await
    {
        Ok((diff, applied)) => {
            print!("{}", diff.render());
            if applied {
                println!("Published full guild registry.");
            } else if options.apply {
                println!("Skipped PUT: registry hash matches.");
            } else {
                println!(
                    "Dry run: no commands written. Use commands publish --apply to overwrite."
                );
            }
            0
        }
        Err(_) => {
            eprintln!("command registry request failed; no successful publication claimed");
            1
        }
    }
}

/// Publication did not previously run on boot. Keep it opt-in while the shared
/// interaction runtime is being wired; the default server remains unchanged.
/// Live opt-in is separate from boot opt-in and checked before DB or REST I/O.
pub async fn publish_on_boot(token: &str, guild: u64) -> Result<(), String> {
    let vars: HashMap<String, String> = std::env::vars().collect();
    if vars
        .get("TWO_COMMANDS_PUBLISH_ON_BOOT")
        .is_none_or(|v| v != "1")
    {
        return Ok(());
    }
    guard_guild(
        guild,
        vars.get("TWO_COMMANDS_ALLOW_LIVE_GUILD")
            .is_some_and(|v| v == "1"),
    )?;
    let application = snowflake(
        vars.get("DISCORD_APPLICATION_ID").map(String::as_str),
        "DISCORD_APPLICATION_ID",
    )?;
    let defs = desired_definitions(guild, &vars)?;
    let executor = executor(token, &vars)
        .await
        .map_err(|_| "cannot configure command REST client")?;
    let (diff, applied) = executor
        .sync_guild_commands(application, guild, &publish_commands(&defs), true)
        .await
        .map_err(|_| "boot command registry synchronization failed")?;
    tracing::info!(hash = %diff.compiled_hash, applied, "boot command registry synchronized");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).to_owned()).collect()
    }
    fn vars() -> HashMap<String, String> {
        HashMap::from([
            ("GUILD_ID".into(), "2222".into()),
            ("DISCORD_APPLICATION_ID".into(), "1111".into()),
        ])
    }

    #[test]
    fn dry_run_default_and_explicit_apply_only() {
        for verb in ["diff", "publish"] {
            assert!(!parse(&args(&[verb]), &vars()).unwrap().apply);
        }
        assert!(
            parse(&args(&["publish", "--apply"]), &vars())
                .unwrap()
                .apply
        );
        assert!(parse(&args(&["diff", "--apply"]), &vars()).is_err());
        assert!(parse(&args(&["publish", "--aply"]), &vars()).is_err());
        assert!(parse(&args(&["publish", "--guild-id"]), &vars()).is_err());
    }

    #[test]
    fn live_fence_applies_even_to_reads_and_leading_zero_ids() {
        for verb in ["diff", "publish"] {
            for id in [
                two_bot_cutover::LIVE_GUILD_ID.to_owned(),
                format!("0{}", two_bot_cutover::LIVE_GUILD_ID),
            ] {
                let opts = args(&[verb, "--guild-id", &id]);
                assert!(parse(&opts, &vars())
                    .unwrap_err()
                    .contains("Refusing live guild"));
                let mut allowed = opts;
                allowed.push("--allow-live-guild".into());
                assert!(parse(&allowed, &vars()).is_ok());
            }
        }
    }

    #[tokio::test]
    async fn live_target_without_database_fails_closed_before_io() {
        // No DISCORD_API_BASE and no DB vars: must refuse with the admission
        // requirement, never "shared durable Discord send admission required".
        let error = executor("synthetic-token", &vars()).await.unwrap_err();
        assert!(
            error.contains("TWO_DATABASE_URL"),
            "fail-closed admission requirement, got: {error}"
        );
    }

    #[tokio::test]
    async fn set_but_empty_primary_is_a_configuration_error() {
        let mut env = vars();
        env.insert("TWO_DATABASE_URL".into(), "   ".into());
        env.insert("DATABASE_URL".into(), "postgres://db/a".into());
        let error = executor("synthetic-token", &env).await.unwrap_err();
        assert!(
            error.contains("set but empty"),
            "empty primary never falls back, got: {error}"
        );
    }

    #[tokio::test]
    async fn invalid_database_url_fails_before_any_socket() {
        // Unparseable URL: connect_options rejects it with no I/O.
        let mut env = vars();
        env.insert("DATABASE_URL".into(), "bogus".into());
        let error = executor("synthetic-token", &env).await.unwrap_err();
        assert!(
            error.contains("cannot configure send-admission Postgres"),
            "fail-closed on bad URL, got: {error}"
        );
    }

    #[tokio::test]
    async fn loopback_fixture_needs_no_database_or_token_admission() {
        let mut env = vars();
        env.insert("DISCORD_API_BASE".into(), "http://127.0.0.1:9".into());
        // No TWO_DATABASE_URL/DATABASE_URL: the offline seam must still build.
        assert!(executor("synthetic-token", &env).await.is_ok());
        assert!(executor("", &env)
            .await
            .unwrap_err()
            .contains("DISCORD_TOKEN"));
    }

    #[test]
    fn desired_registry_uses_existing_feature_gates() {
        let mut env = vars();
        assert_eq!(desired_definitions(2222, &env).unwrap().len(), 3);
        env.insert("TWO_AUTOMATIONS".into(), "1".into());
        env.insert("TWO_ANNOUNCEMENTS".into(), "1".into());
        env.insert("TWO_COMMUNITY_SCORECARD".into(), "1".into());
        env.insert("TWO_MODERATION".into(), "1".into());
        env.insert("TWO_OWEN_USER_ID".into(), "123456789012345678".into());
        assert_eq!(desired_definitions(2222, &env).unwrap().len(), 28);
    }
}
