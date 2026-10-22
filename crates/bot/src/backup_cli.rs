//! Operator CLI: `backup`, `restore`, `backup-upload`,
//! `guild-config-snapshot`, `guild-config-restore` (TOG-9881).
//!
//! Rust port of legacy `scripts/pg-backup.ts`, `scripts/pg-restore.ts`,
//! `scripts/backup-upload-s3.ts`, `scripts/guild-config-snapshot.ts` and
//! `scripts/guild-config-restore.ts`. Run by the timer units in `deploy/`
//! (see `docs/backup.md`); safe while the bot is up — the dump is one
//! `REPEATABLE READ` snapshot.
//!
//! Secret posture: the staging token prefers a systemd credential file,
//! with an explicit env fallback only when absent. Secrets are never logged,
//! never defaulted, never written to the board.
//! Exit codes: 0 success (`RESTORE VERIFIED` / `DRY RUN VERIFIED` on the
//! last line is the only success for restores), 1 failure, 2 usage/guard
//! refusal, 3 tampered guild-config snapshot.

// Operator commands intentionally emit human-readable/JSON output to stdout.
#![allow(clippy::print_stdout)]

use std::env;
use std::path::{Path, PathBuf};

use two_bot_core::backup::{
    dump, dump_file, guild_config, guild_config_api::GuildConfigDiscordApi, guild_config_restore,
    http, retention, s3,
};

pub fn print_server_usage() {
    println!("{}", crate::preflight::USAGE);
    print!("{}", crate::erasure_cli::USAGE);
    print!("{}", crate::moderation_cli::USAGE);
    print!("{}", crate::moderation_release_cli::USAGE);
    print!("{}", crate::member_cli::USAGE);
    print!("{}", crate::event_cli::USAGE);
}

fn env_var(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

/// Read the one TLS policy setting; an unset value is `Required`.
fn tls_policy_from_env() -> Result<two_bot_core::database_tls::TlsPolicy, &'static str> {
    let value = std::env::var_os(two_bot_core::database_tls::POLICY_SETTING);
    // A non-UTF-8 value parses as "" and is refused like any unknown value.
    two_bot_core::database_tls::TlsPolicy::from_setting(
        value.as_ref().map(|v| v.to_str().unwrap_or("")),
    )
}

/// Backup/restore pool shape: max 5 (legacy `TWO_DB_POOL_MAX` parity) with a
/// 10 s acquire timeout and a 60 s statement timeout. The CLI runs sequential
/// backup steps, so one small pool suffices; a hung database refuses with the
/// fixed `cannot connect` constant instead of hanging the operator command.
/// The statement leg is bulk-sized (matching the `legacy_copy` 60 s bulk
/// precedent): dump/restore run whole-table ordered scans, per-table
/// `COUNT(*)` and `LOCK TABLE` that can legitimately exceed the 15 s
/// application bound, so the application value would cancel healthy backups
/// with Postgres 57014.
const BACKUP_POOL_MAX: u32 = 5;
const BACKUP_ACQUIRE_TIMEOUT_SECS: u64 = 10;
const BACKUP_STATEMENT_TIMEOUT_MS: u64 = 60_000;

/// Open a Postgres pool through the shared redacting parse path
/// (`two_bot_core::database_url`): unsupported query keys are refused before
/// the pinned driver's parser can WARN-log their values, the driver's passfile
/// diagnostics stay suppressed for the synchronous parse, and every failure is
/// a constant message that never echoes the URL. Threat-model F6: the
/// `TWO_DATABASE_TLS` policy (unset means `required`) refuses
/// plaintext/unverified modes and the wrong host class before SQLx parses the
/// URL (see `docs/database-tls.md`). The pool carries the documented
/// backup triple (`BACKUP_POOL_MAX`, `BACKUP_ACQUIRE_TIMEOUT_SECS`,
/// `BACKUP_STATEMENT_TIMEOUT_MS`) instead of the SQLx defaults.
async fn open_pool(url: &str) -> Result<sqlx::PgPool, &'static str> {
    open_pool_with_tls(url, tls_policy_from_env()?).await
}

/// [`open_pool`] with an explicit TLS policy (tests pass `LocalOnly`).
async fn open_pool_with_tls(
    url: &str,
    tls: two_bot_core::database_tls::TlsPolicy,
) -> Result<sqlx::PgPool, &'static str> {
    two_bot_core::database_url::validate(url)?;
    // Threat-model F6: refuse plaintext/unverified modes and the wrong host
    // class before SQLx parses the URL (see `docs/database-tls.md`).
    two_bot_core::database_tls::enforce(url, tls)?;
    let options = two_bot_core::database_tls::apply(
        two_bot_core::database_url::connect_options(url).map_err(|_| "invalid database URL")?,
        tls,
    );
    let options = options.options([backup_statement_timeout_option()]);
    backup_pool_options()
        .connect_with(options)
        .await
        .map_err(|_| "cannot connect; database details redacted")
}

/// Typed restore pool-open: `None` is a fixed, non-echoing `&'static str`
/// failure in the same type as [`open_pool`], never a panic. `Some` delegates
/// to [`open_pool`] unchanged (same constants, TLS policy, connection
/// behavior). Defense in depth behind the exit-2 `TWO_RESTORE_URL` guard in
/// `cmd_restore`.
pub(crate) async fn open_restore_pool(url: Option<&str>) -> Result<sqlx::PgPool, &'static str> {
    let Some(url) = url else {
        return Err("missing restore database URL");
    };
    open_pool(url).await
}

/// Backup pool builder: max 5 with a 10 s acquire bound (the pool half of
/// the documented backup triple, applied by [`open_pool_with_tls`]; the
/// statement leg rides the connection options). Split out so tests pin the
/// built values exactly instead of grepping the source.
fn backup_pool_options() -> sqlx::postgres::PgPoolOptions {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(BACKUP_POOL_MAX)
        .acquire_timeout(std::time::Duration::from_secs(BACKUP_ACQUIRE_TIMEOUT_SECS))
}

/// Backup statement-timeout server setting, applied to the connection
/// options by [`open_pool_with_tls`]. Split out so tests pin the exact
/// `60000ms` bulk bound the pool applies.
fn backup_statement_timeout_option() -> (String, String) {
    (
        "statement_timeout".to_owned(),
        format!("{}ms", BACKUP_STATEMENT_TIMEOUT_MS),
    )
}

/// Only an absent credential permits the deliberate environment fallback.
/// Inject the directory and lazy fallback so tests never mutate process env.
fn load_staging_token(
    credentials_dir: Option<&Path>,
    fallback: &dyn Fn() -> Option<String>,
) -> Result<String, String> {
    if let Some(dir) = credentials_dir {
        match std::fs::read_to_string(dir.join("discord_staging_token")) {
            Ok(contents) => {
                let token = contents.trim();
                if token.is_empty() {
                    return Err(
                        "discord_staging_token credential is empty; refusing environment fallback"
                            .to_owned(),
                    );
                }
                if guild_config::check_staging_token(token).is_err() {
                    return Err("discord_staging_token credential is invalid for Owen QA Test; refusing environment fallback".to_owned());
                }
                return Ok(token.to_owned());
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                // Do not include the IO error: invalid UTF-8 and other failures
                // must never surface credential contents in the journal.
                return Err(
                    "cannot read discord_staging_token credential; refusing environment fallback"
                        .to_owned(),
                );
            }
        }
    }
    fallback()
        .map(|token| token.trim().to_owned())
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            "missing discord_staging_token credential and DISCORD_STAGING_BOT_TOKEN fallback"
                .to_owned()
        })
}

fn staging_token() -> Result<String, String> {
    let credentials_dir = env::var_os("CREDENTIALS_DIRECTORY")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from);
    load_staging_token(credentials_dir.as_deref(), &|| {
        env_var("DISCORD_STAGING_BOT_TOKEN")
    })
}

/// Bump-friendly usage text. `--help` after any subcommand prints it.
const USAGE: &str = "\
two-bot operator commands

  two-bot db roles plan
      Print the reviewed role SQL; never connects or applies it.

  two-bot db roles verify
      Read-only privilege drift inspection. Env: TWO_DATABASE_URL (required).

  two-bot backup
      Dump all bot-owned tables (v4 format) to TWO_BACKUP_DIR
      (default ./backups) as two-funnel-<stamp>.ndjson.gz, prune to
      TWO_BACKUP_KEEP newest (default 14), then run TWO_BACKUP_UPLOAD_CMD
      with the file path as its last argument.
      Env: TWO_DATABASE_URL (required), TWO_BACKUP_DIR, TWO_BACKUP_KEEP,
           TWO_BACKUP_UPLOAD_CMD.

  two-bot restore <backup.ndjson.gz> --force [--dry-run]
      --dry-run reads and validates the file, reports what it holds, and
      writes nothing (TWO_RESTORE_URL optional: with it you also get the
      target's current counts). Without --dry-run, --force is required and
      TWO_RESTORE_URL must name the target. The target variable is
      deliberately NOT TWO_DATABASE_URL: restoring replaces target data.
      A target that normalizes to the live database, or whose database
      cannot be proven (name it explicitly), is refused (exit 2,
      code=restore_live_target_refused), even with --force; --dry-run
      still previews without that comparison.
      Requires a fresh migrated target without moderation history. --force
      confirms intent, not authorization or a safety-guard bypass.
      `RESTORE VERIFIED` on the last line, and exit 0, is the only success.

  two-bot restore-drill <backup.ndjson.gz> --confirm-scratch
      Allocate a fresh agent-testdb scratch database, apply shipped migrations
      plus pinned scratch-only legacy archive DDL, then guarded restore.
      Retain prior targets, archives and exclusive receipts; never reuse/drop.
      Inspect dropped_columns in the receipt: verification proves row counts.
      Env: TWO_RESTORE_DRILL_BOOTSTRAP_URL (explicit empty-password test binding),
           TWO_RESTORE_DRILL_EVIDENCE_DIR (pre-existing protected absolute directory).
      No TWO_RESTORE_URL/source credentials or live Discord are used.

  two-bot backup-upload <dump.ndjson.gz>
      PUT one dump to S3-compatible storage (SigV4, single-PUT).
      Env: TWO_BACKUP_S3_ENDPOINT, TWO_BACKUP_S3_BUCKET,
           TWO_BACKUP_S3_ACCESS_KEY_ID, TWO_BACKUP_S3_SECRET_ACCESS_KEY
           (all required), TWO_BACKUP_S3_REGION (default auto),
           TWO_BACKUP_S3_PREFIX, TWO_BACKUP_S3_TIMEOUT_MS (default 300000).

  two-bot guild-config-snapshot
      Capture the staging guild (roles, channels, overwrites, settings,
      emoji via CDN), seal the snapshot (TOG-3513), write it atomically
      plus a drift report against the accepted spec, then upload both.
      A local-only snapshot is an error, not success.
      Token: CREDENTIALS_DIRECTORY/discord_staging_token (Owen QA Test only).
      Env: DISCORD_STAGING_BOT_TOKEN (fallback only if credential absent),
           DISCORD_STAGING_GUILD_ID (required, pinned to TWO Staging),
           GUILD_CONFIG_API_BASE / GUILD_CONFIG_CDN_BASE (loopback test
           seams only), TWO_GUILD_CONFIG_BACKUP_DIR,
           TWO_GUILD_CONFIG_UPLOAD_CMD (or TWO_BACKUP_UPLOAD_CMD, required).

  two-bot guild-config-restore --snapshot FILE [--confirm-staging-guild --apply] [--evidence FILE]
      Plan (default) or apply a sealed snapshot to the staging guild.
      Refuses tampered backups (exit 3) before any Discord call; refuses
      the live guild and any non-staging token. --apply requires
      --confirm-staging-guild. Completion prints per operation only as
      each operation lands; a mid-apply failure names landed vs pending
      operations (code=guild_restore_apply_failed).
";

/// The subcommands whose help is `USAGE`; `db` and `erase-member` parse
/// their own.
const SUBCOMMANDS: [&str; 5] = [
    "backup",
    "restore",
    "backup-upload",
    "guild-config-snapshot",
    "guild-config-restore",
];

/// What `dispatch` does with a subcommand, decided from its arguments alone.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    Help,
    Usage(String),
    Run,
}

/// Help and usage errors are settled here, before any env, DB or network
/// read: `two-bot backup --help` must never dump, prune or upload, and
/// `backup-upload --help` must never be taken for a path. A `--` ends
/// options (restore's grammar), so `restore -- -h` still names a file.
fn route(subcommand: &str, rest: &[String]) -> Route {
    if !SUBCOMMANDS.contains(&subcommand) {
        return Route::Run;
    }
    if rest
        .iter()
        .take_while(|arg| *arg != "--")
        .any(|arg| arg == "--help" || arg == "-h")
    {
        return Route::Help;
    }
    match (subcommand, rest.first()) {
        ("backup" | "guild-config-snapshot", Some(extra)) => {
            // One stable token per subcommand, kept literal so the frozen
            // source pins keep resolving.
            let code = if subcommand == "backup" {
                "code=backup_usage"
            } else {
                "code=snapshot_usage"
            };
            Route::Usage(format!(
                "{subcommand}: unexpected argument {extra:?}. {code}\n{USAGE}"
            ))
        }
        _ => Route::Run,
    }
}

/// Dispatch `args` (without the program name). Returns the exit code.
/// `serve` is handled by the caller: this returns 100 when no backup
/// subcommand was given so `main` falls through to the gateway path.
pub async fn dispatch(args: &[String]) -> i32 {
    if args.is_empty() || args[0] == "--help" || args[0] == "-h" {
        if args.is_empty() {
            return 100;
        }
        print!("{USAGE}");
        return 0;
    }
    // Bare-boot owed-release override: the documented `two-bot --allow-owed`
    // service-start flag falls through to the boot preflight in `run`
    // (100 = not a backup subcommand). Only the exact single flag; typos,
    // extra arguments and operator subcommands keep exit 2 below.
    if args.len() == 1 && args[0] == two_bot_core::disable_preflight::OVERRIDE_FLAG {
        return 100;
    }
    match route(&args[0], &args[1..]) {
        Route::Help => {
            print!("{USAGE}");
            return 0;
        }
        Route::Usage(message) => {
            eprintln!("{message}");
            return 2;
        }
        Route::Run => {}
    }
    match args[0].as_str() {
        "db" => crate::database_roles_cli::dispatch(&args[1..]).await,
        "erase-member" => crate::erasure_cli::dispatch(&args[1..]).await,
        "reconcile-member" => crate::member_cli::dispatch(&args[1..]).await,
        "reconcile-event" => crate::event_cli::dispatch(&args[1..]).await,
        "backup" => cmd_backup().await,
        "restore" => cmd_restore(&args[1..]).await,
        "restore-drill" => crate::restore_drill::dispatch(&args[1..]).await,
        "backup-upload" => cmd_backup_upload(&args[1..]).await,
        "guild-config-snapshot" => cmd_guild_config_snapshot().await,
        "guild-config-restore" => cmd_guild_config_restore(&args[1..]).await,
        other => {
            eprintln!("unknown subcommand {other:?}.\n{USAGE}");
            2
        }
    }
}

// ---------------------------------------------------------------------------
// backup
// ---------------------------------------------------------------------------

async fn cmd_backup() -> i32 {
    let url = match env_var("TWO_DATABASE_URL") {
        Some(url) if url.starts_with("postgres://") || url.starts_with("postgresql://") => url,
        _ => {
            eprintln!("backup: TWO_DATABASE_URL must be set to a Postgres URL. code=backup_missing_database_url");
            return 1;
        }
    };
    let dest = env_var("TWO_BACKUP_DIR").unwrap_or_else(|| "./backups".to_owned());

    // Checked before the dump, not before the prune: a setting that would
    // delete every backup stops the run while it is still a no-op.
    let keep = match retention::parse_keep(env_var("TWO_BACKUP_KEEP").as_deref()) {
        Ok(keep) => keep,
        Err(err) => {
            eprintln!("backup: {err}. code=backup_retention_refused");
            eprintln!("        Refusing to run: every reading of that value prunes all backups. code=backup_retention_refused");
            return 1;
        }
    };

    if let Err(err) = std::fs::create_dir_all(&dest) {
        eprintln!("backup: cannot create {dest}: {err} code=backup_dest_create_failed");
        return 1;
    }
    let stamp = guild_config::filename_stamp();
    let out = PathBuf::from(&dest).join(format!("two-funnel-{stamp}.ndjson.gz"));

    // Every failure is a bounded constant from `open_pool`: the URL and its
    // query secrets never reach logs or CLI output.
    let pool = match open_pool(&url).await {
        Ok(pool) => pool,
        Err(detail) => {
            eprintln!("backup: {detail} code=backup_pool_open_failed");
            return 1;
        }
    };

    // Semantic acceptance precedes retention and upload: a refused dump
    // must never evict the last usable archive or reach off-box storage.
    // A failed dump publishes nothing (the `.dump-writing-*.tmp` temporary
    // neither matches retention's predicate nor survives its own Drop), so
    // both error paths below leave every existing recovery point untouched.
    match dump::dump(&pool, &out).await {
        Ok(manifest) => {
            for table in &manifest.tables {
                println!("  {:17} {}", table.name, table.count);
            }
            match std::fs::metadata(&out) {
                Ok(meta) => println!(
                    "backup: wrote {} ({:.1} KiB)",
                    out.display(),
                    meta.len() as f64 / 1024.0
                ),
                Err(err) => {
                    eprintln!(
                        "backup: wrote {} but cannot stat it: {err} code=backup_stat_failed",
                        out.display()
                    );
                    return 1;
                }
            }
            if manifest
                .tables
                .iter()
                .find(|t| t.name == "events")
                .map(|t| t.count)
                .unwrap_or(0)
                == 0
            {
                eprintln!(
                    "backup: the event log is empty. Refusing to call this a good backup: \
                     keeping existing archives, skipping upload. code=backup_empty_event_log"
                );
                pool.close().await;
                return 1;
            }
        }
        Err(err) => {
            eprintln!("backup: dump failed: {err} code=backup_dump_failed");
            return 1;
        }
    }
    pool.close().await;

    // Retention: newest `keep` files survive. Done before the upload so a
    // failing upload does not also stop the disk being tidied.
    match prune_backups(Path::new(&dest), keep) {
        Ok(pruned) => {
            for name in pruned {
                println!("backup: pruning {name}");
            }
        }
        Err(err) => {
            eprintln!("backup: prune failed: {err} code=backup_prune_failed");
            return 1;
        }
    }

    match s3::build_upload_argv(
        env_var("TWO_BACKUP_UPLOAD_CMD").as_deref(),
        &out.to_string_lossy(),
    ) {
        Some((cmd, upload_args)) => {
            // Bounded upload: the shared helper gives a hung helper a finite
            // 30-minute budget, then terminates and reaps the direct child it
            // spawned and fails the run without deleting the archive. Argv and
            // child output are never echoed (see docs/backup.md).
            match run_upload_command(&cmd, &upload_args, UPLOAD_COMMAND_TIMEOUT).await {
                UploadOutcome::Success => println!("backup: uploaded via {cmd}"),
                outcome => {
                    if let Some(message) =
                        upload_failure_message("backup", &cmd, &outcome, UPLOAD_COMMAND_TIMEOUT)
                    {
                        eprintln!("{message}");
                    }
                    return 1;
                }
            }
        }
        None => {
            eprintln!(
                "backup: TWO_BACKUP_UPLOAD_CMD is not set - this backup is on the same disk\n\
                 \x20       as the database. That survives corruption and mistakes, not the\n\
                 \x20       loss of the machine. See docs/backup.md."
            );
        }
    }

    println!("backup: done");
    0
}

/// Delete all but the newest `keep` `two-funnel-*.ndjson.gz` files (by
/// mtime). Returns the pruned file names.
fn prune_backups(dir: &Path, keep: usize) -> Result<Vec<String>, String> {
    let mut mine: Vec<(String, std::time::SystemTime)> = Vec::new();
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("cannot list {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot list {}: {e}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !(name.starts_with("two-funnel-") && name.ends_with(".ndjson.gz")) {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        mine.push((name, mtime));
    }
    mine.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.cmp(&a.0)));
    let names: Vec<String> = mine.into_iter().map(|(name, _)| name).collect();
    let mut pruned = Vec::new();
    for old in retention::to_prune(&names, keep) {
        std::fs::remove_file(dir.join(old)).map_err(|e| format!("cannot prune {old}: {e}"))?;
        pruned.push(old.clone());
    }
    Ok(pruned)
}

// ---------------------------------------------------------------------------
// restore
// ---------------------------------------------------------------------------

/// Strict restore grammar: exactly one file positional, and only `--force`
/// and `--dry-run` as options. Unknown options, extra positionals and
/// malformed forms refuse (exit 2) before any file access, connection or
/// destructive branch — a misspelled `--dryrun` must never select the
/// destructive path (TOG-9970 finding 3).
fn parse_restore_args(args: &[String]) -> Result<(String, bool, bool), String> {
    const USAGE: &str = "restore: usage: two-bot restore <backup.ndjson.gz> --force [--dry-run]";
    let mut file: Option<String> = None;
    let mut force = false;
    let mut dry_run = false;
    let mut options_ended = false;
    for arg in args {
        if !options_ended && arg == "--" {
            options_ended = true;
            continue;
        }
        if !options_ended && arg.starts_with("--") {
            match arg.as_str() {
                "--force" => force = true,
                "--dry-run" => dry_run = true,
                unknown => return Err(format!("restore: unknown option {unknown:?}.\n{USAGE}")),
            }
            continue;
        }
        if file.is_some() {
            return Err(format!(
                "restore: unexpected extra argument {arg:?}.\n{USAGE}"
            ));
        }
        file = Some(arg.clone());
    }
    let Some(file) = file else {
        return Err(USAGE.to_owned());
    };
    Ok((file, force, dry_run))
}

async fn cmd_restore(args: &[String]) -> i32 {
    let (file, force, dry_run) = match parse_restore_args(args) {
        Ok(parsed) => parsed,
        Err(usage) => {
            eprintln!("{usage}");
            return 2;
        }
    };
    if !Path::new(&file).is_file() {
        eprintln!("restore: no such file: {file}");
        return 1;
    }
    if !dry_run && !force {
        eprintln!("restore: this wipes the target. Pass --force if that is what you mean.");
        return 2;
    }

    let url = env_var("TWO_RESTORE_URL");
    let have_url = url
        .as_ref()
        .is_some_and(|u| u.starts_with("postgres://") || u.starts_with("postgresql://"));
    if !have_url && !dry_run {
        eprintln!("restore: TWO_RESTORE_URL must be set to a Postgres URL.");
        eprintln!("restore: deliberately not TWO_DATABASE_URL. See docs/backup.md.");
        return 2;
    }

    if dry_run {
        return cmd_restore_dry_run(&file, url.as_deref()).await;
    }

    // A prod URL in TWO_RESTORE_URL plus --force used to wipe production: the
    // guard above only checked presence and scheme, never the target's value.
    // Refuse by normalized value (scheme alias, host case, default ports and
    // credential/query spellings cannot evade it), production database
    // markers, and unprovable targets (an omitted database or an empty
    // override cannot be proven different from live), before any file parse
    // or connection attempt. --force confirms intent, never a live target;
    // --dry-run previews without this comparison.
    // Fixed refusal: the URLs and their secrets never reach output.
    if let Some(target) = url.as_deref() {
        if two_bot_core::database_url::restore_target_is_live(
            target,
            env_var("TWO_DATABASE_URL").as_deref(),
        ) {
            eprintln!(
                "restore: refusing a live database target; restores go to scratch only. code=restore_live_target_refused"
            );
            eprintln!(
                "restore: TWO_RESTORE_URL must not name the live database. See docs/backup.md."
            );
            return 2;
        }
    }

    // Every failure is a bounded constant from `open_restore_pool`: the URL and
    // its query secrets never reach logs or CLI output.
    let pool = match open_restore_pool(url.as_deref()).await {
        Ok(pool) => pool,
        Err(detail) => {
            eprintln!("restore: {detail}");
            return 1;
        }
    };
    // Restore deliberately does not apply migrations. The independently
    // provisioned target must already carry the current cutover schema;
    // dump()/restore() refuse with a named table when it does not.
    match dump::restore(&pool, Path::new(&file)).await {
        Ok(report) => {
            println!("restore: dump taken {}", report.manifest.created_at);
            warn_missing_dump_tables(&report.manifest);
            println!(
                "restore: migrations in dump: {}",
                if report.manifest.schema_migrations.is_empty() {
                    "(none)".to_owned()
                } else {
                    report.manifest.schema_migrations.join(", ")
                }
            );
            for table in &report.manifest.tables {
                let got = report.restored.get(&table.name).copied().unwrap_or(0);
                println!(
                    "  {:17} manifest {:7}  restored {:7}  {}",
                    table.name,
                    table.count,
                    got,
                    if got == table.count { "ok" } else { "MISMATCH" }
                );
            }
            for (table, count) in &report.initialized_tables {
                eprintln!("restore: WARNING: {table}: initialized {count} schema-required baseline row(s), not archived data");
            }
            if report.missing_member_ban_ownership {
                eprintln!(
                    "restore: old v3 dump has no member-ban ownership; acceptance not inferred"
                );
            }
            if report.quarantined_unbans > 0 {
                eprintln!(
                    "restore: {} imported expiries quarantined (including accepted snapshots); moderation must remain off until authoritative reconciliation",
                    report.quarantined_unbans
                );
            }
            for (table, cols) in &report.dropped_columns {
                eprintln!(
                    "restore: {table}: columns in the dump the target does not have: {}",
                    cols.join(", ")
                );
            }
            if !report.ok {
                eprintln!("\nRESTORE FAILED - counts do not match. Treat this backup as lost.");
                return 1;
            }
        }
        Err(err) => {
            eprintln!("\nrestore: {err}");
            eprintln!("RESTORE FAILED");
            return 1;
        }
    }
    pool.close().await;
    println!("\nRESTORE VERIFIED");
    0
}

fn warn_missing_dump_tables(manifest: &dump_file::DumpManifest) {
    let missing = manifest.missing_tables();
    if !missing.is_empty() {
        eprintln!(
            "restore: WARNING: v{} dump lacks tables that will be cleared (settings revision singleton resets to zero): {}",
            manifest.version,
            missing.join(", ")
        );
    }
}

async fn cmd_restore_dry_run(file: &str, url: Option<&str>) -> i32 {
    // Nothing in this branch writes. A dry run must not be able to become
    // the outage it rehearses for.
    let contents = match dump_file::inspect(Path::new(file)) {
        Ok(contents) => contents,
        Err(err) => {
            eprintln!("restore: {err}");
            eprintln!("DRY RUN FAILED");
            return 1;
        }
    };

    println!("restore: --dry-run of {file}");
    println!("restore: dump taken {}", contents.manifest.created_at);
    warn_missing_dump_tables(&contents.manifest);
    eprintln!("restore: apply refuses a destination with moderation history; preserve it and use a fresh migrated target");
    eprintln!("restore: apply quarantines all executable imported expiries, even accepted snapshots; keep moderation off pending authoritative reconciliation of both histories");
    if !contents
        .manifest
        .tables
        .iter()
        .any(|table| table.name == "moderation_member_bans")
    {
        eprintln!("restore: old v3 has no member-ban ownership; apply does not infer acceptance; keep moderation off pending reconciliation");
    }
    println!(
        "restore: migrations in dump: {}",
        if contents.manifest.schema_migrations.is_empty() {
            "(none)".to_owned()
        } else {
            contents.manifest.schema_migrations.join(", ")
        }
    );

    // Read the target's current rows where we can, but do not create them.
    let mut before: std::collections::BTreeMap<String, String> = Default::default();
    if let Some(url) = url {
        println!("restore: checking configured target");
        // Every probe failure is a bounded constant from `open_pool`: the
        // URL and its query secrets never reach logs or CLI output.
        match open_pool(url).await {
            Ok(probe) => {
                for name in dump_file::DUMP_TABLES {
                    let count: Result<(i64,), _> =
                        sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {name}")))
                            .fetch_one(&probe)
                            .await;
                    before.insert(
                        (*name).to_owned(),
                        match count {
                            Ok((n,)) => n.to_string(),
                            Err(_) => "(no such table - provision matching schema before restore)"
                                .to_owned(),
                        },
                    );
                }
                probe.close().await;
            }
            Err(_) => {
                eprintln!("restore: cannot probe target; database details redacted; checking the file only.");
                for name in dump_file::DUMP_TABLES {
                    before.insert((*name).to_owned(), "(not checked)".to_owned());
                }
            }
        }
    } else {
        println!("restore: no TWO_RESTORE_URL - checking the file only.");
        for name in dump_file::DUMP_TABLES {
            before.insert((*name).to_owned(), "(not checked)".to_owned());
        }
    }

    let mut short = false;
    for table in &contents.manifest.tables {
        let held = contents.buffers.get(&table.name).map(Vec::len).unwrap_or(0) as u64;
        if held != table.count {
            short = true;
        }
        println!(
            "  {:17} manifest {:7}  in file {:7}  {}   target now {}",
            table.name,
            table.count,
            held,
            if held == table.count {
                "ok"
            } else {
                "MISMATCH"
            },
            before.get(&table.name).map(String::as_str).unwrap_or("?"),
        );
    }
    if short {
        eprintln!("\nDRY RUN FAILED - the file does not hold what its manifest claims.");
        return 1;
    }
    println!(
        "\nrestore: {} rows read and verified. Nothing was written.",
        contents.rows
    );
    println!("DRY RUN VERIFIED");
    0
}

// ---------------------------------------------------------------------------
// backup-upload
// ---------------------------------------------------------------------------

async fn cmd_backup_upload(args: &[String]) -> i32 {
    if args.len() != 1 {
        eprintln!(
            "backup-upload-s3: expected exactly one argument (the dump path), got {} code=backup_upload_usage",
            args.len()
        );
        return 2;
    }
    let dump_path = Path::new(&args[0]);
    let size = match std::fs::metadata(dump_path) {
        Ok(meta) if meta.is_file() => meta.len(),
        _ => {
            eprintln!(
                "backup-upload-s3: cannot read {}: not a regular file code=backup_upload_not_regular_file",
                dump_path.display()
            );
            return 1;
        }
    };
    // An empty dump would upload happily and restore to nothing.
    if size == 0 {
        eprintln!(
            "backup-upload-s3: refusing to upload an empty file: {} code=backup_upload_empty_file",
            dump_path.display()
        );
        return 1;
    }
    // Mirror the restore drill: refuse an over-cap dump before buffering it
    // in memory for signing. Strict `>` so an exactly-at-cap dump uploads.
    if size > dump_file::MAX_DUMP_BYTES {
        eprintln!(
            "backup-upload-s3: {} is {size} bytes, exceeding the {}-byte dump cap; refusing rather than buffering it code=backup_upload_dump_too_large",
            dump_path.display(),
            dump_file::MAX_DUMP_BYTES
        );
        return 1;
    }

    let std_env = |name: &str| env_var(name);
    let target = match s3::load_s3_target(&std_env) {
        Ok(target) => target,
        Err(err) => {
            eprintln!("backup-upload-s3: {err} code=backup_upload_s3_target");
            return 1;
        }
    };
    let filename = dump_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "dump.ndjson.gz".to_owned());
    let key = s3::object_key(target.prefix.as_deref(), &filename);
    let body = match std::fs::read(dump_path) {
        Ok(body) => body,
        Err(err) => {
            eprintln!(
                "backup-upload-s3: cannot read {}: {err} code=backup_upload_read_failed",
                dump_path.display()
            );
            return 1;
        }
    };
    // Re-check after the read: a dump that grew past the cap between the
    // metadata precheck and the read is still refused before signing or any
    // S3 PUT. (The read call shape above is pinned by the frozen
    // stable-codes suite, so bounded streaming needs test-author
    // coordination and stays a follow-up.)
    if body.len() as u64 > dump_file::MAX_DUMP_BYTES {
        eprintln!(
            "backup-upload-s3: {} exceeds the {}-byte dump cap; refusing to sign or upload it code=backup_upload_dump_too_large",
            dump_path.display(),
            dump_file::MAX_DUMP_BYTES
        );
        return 1;
    }

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (amz_date, date_stamp) = s3::amz_stamps(now_secs);
    let signed = s3::sign_put(&target, &key, &body, &amz_date, &date_stamp);

    // Log the destination but never the credential: this goes to the journal.
    println!(
        "backup-upload-s3: PUT {}/{} ({size} bytes)",
        target.bucket, key
    );
    let timeout_ms: u64 = env_var("TWO_BACKUP_S3_TIMEOUT_MS")
        .and_then(|v| v.parse().ok())
        .unwrap_or(300_000);
    match http::put(
        signed.url.expose(),
        signed.headers.expose().clone(),
        body,
        timeout_ms.div_ceil(1000),
    )
    .await
    {
        Ok(res) => {
            if !(200..300).contains(&res.status.as_u16()) {
                eprintln!(
                    "backup-upload-s3: PUT {} {}{} code=backup_upload_put_status",
                    res.status.as_u16(),
                    res.status.canonical_reason().unwrap_or(""),
                    res.detail()
                );
                return 1;
            }
            // Even successful response headers can echo signed credentials.
            // The destination is ours; the remote ETag is not a safe log field.
            println!("backup-upload-s3: stored {}/{}", target.bucket, key);
            0
        }
        Err(err) => {
            eprintln!("backup-upload-s3: PUT failed: {err} code=backup_upload_put_failed");
            1
        }
    }
}

// ---------------------------------------------------------------------------
// bounded command upload (shared by `backup` and `guild-config-snapshot`)
// ---------------------------------------------------------------------------

/// Finite budget for one external upload-helper invocation, shared by the
/// `backup` and `guild-config-snapshot` paths. Thirty minutes fits the
/// nightly timer window while still bounding a hung helper.
const UPLOAD_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Outcome of one bounded external upload-helper invocation. The failure
/// lines built by [`upload_failure_message`] name only the program and the
/// budget: argv (which may carry credentials) and the child's own output
/// are never echoed.
#[derive(Debug)]
enum UploadOutcome {
    Success,
    ExitNonzero(std::process::ExitStatus),
    SpawnFailed(String),
    WaitFailed(String),
    TimedOut,
}

/// Render the stderr line for a failed upload, or `None` on success.
/// `scope` is the caller's fixed prefix (`backup`, `guild-config-snapshot`);
/// `cmd` is the program name only, never the argument vector.
fn upload_failure_message(
    scope: &str,
    cmd: &str,
    outcome: &UploadOutcome,
    budget: std::time::Duration,
) -> Option<String> {
    match outcome {
        UploadOutcome::Success => None,
        UploadOutcome::ExitNonzero(status) => {
            Some(format!("{scope}: upload failed: {cmd} exited {status} code=upload_exit_nonzero"))
        }
        UploadOutcome::SpawnFailed(err) => {
            Some(format!("{scope}: upload failed to start ({cmd}): {err} code=upload_spawn_failed"))
        }
        UploadOutcome::WaitFailed(err) => {
            Some(format!("{scope}: upload wait failed ({cmd}): {err} code=upload_wait_failed"))
        }
        UploadOutcome::TimedOut => Some(format!(
            "{scope}: upload timed out after {budget:?}; terminated upload child ({cmd}) code=upload_timed_out"
        )),
    }
}

/// Run one external upload helper to completion within `deadline`.
///
/// On timeout the direct child this invocation spawned is terminated
/// (`start_kill`) and reaped (`wait`) before returning [`UploadOutcome::TimedOut`],
/// so no owned child outlives the call. The deadline covers that direct
/// child only: a helper that detaches grandchildren is outside this
/// guarantee (no process-group policy; killing by name or group would risk
/// unrelated processes). Stdio is inherited, never captured, so remote
/// output cannot leak through the returned outcome.
async fn run_upload_command(
    cmd: &str,
    args: &[String],
    deadline: std::time::Duration,
) -> UploadOutcome {
    let mut child = match tokio::process::Command::new(cmd)
        .args(args)
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(err) => return UploadOutcome::SpawnFailed(err.to_string()),
    };
    match tokio::time::timeout(deadline, child.wait()).await {
        Ok(Ok(status)) if status.success() => UploadOutcome::Success,
        Ok(Ok(status)) => UploadOutcome::ExitNonzero(status),
        Ok(Err(err)) => UploadOutcome::WaitFailed(err.to_string()),
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            UploadOutcome::TimedOut
        }
    }
}

/// Parse the `guild-config-snapshot` upload command once for `path`.
///
/// Single-parse helper for the snapshot upload loop: exactly one
/// `s3::build_upload_argv` call per path. Returns `None` when no uploader is
/// configured, in which case the caller refuses with the existing local-only
/// message and exit 2; `Some` carries byte-identical upload inputs.
fn snapshot_upload_argv(upload_raw: Option<&str>, path: &Path) -> Option<(String, Vec<String>)> {
    s3::build_upload_argv(upload_raw, &path.to_string_lossy())
}

// ---------------------------------------------------------------------------
// guild-config-snapshot
// ---------------------------------------------------------------------------

/// Removes our temp file unless a successful rename disarms it. Every
/// post-create failure path (write, permissions, fsync, rename) drops through
/// here, so no `.tmp` partial survives a failed write. Only our own temp is
/// removed; siblings are never touched. The guard arms only after the temp is
/// created, so a creation failure never deletes a pre-existing path.
struct TmpGuard<'a> {
    tmp: &'a Path,
    armed: bool,
}

impl Drop for TmpGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(self.tmp);
        }
    }
}

fn atomic_json(path: &Path, value: &serde_json::Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    // Unique temp per call (pid + nanos): exclusive creation can never
    // collide with a stale temp from a crashed run, and `create_new` refuses
    // a planted symlink or file at the temp path instead of following or
    // truncating it (CWE-59). A collision fails closed with the existing
    // "cannot write" error.
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = path.with_extension(format!("{}.{unique}.tmp", std::process::id()));
    // Stays unarmed until the temp exists: a creation failure returns the
    // existing error without removing a pre-existing path.
    let mut guard = TmpGuard {
        tmp: &tmp,
        armed: false,
    };
    let text = serde_json::to_string_pretty(value).map_err(|e| format!("cannot serialise: {e}"))?;
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        guard.armed = true;
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        #[cfg(unix)]
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
        file.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
        file.write_all(b"\n").map_err(|e| e.to_string())?;
        file.sync_all()
            .map_err(|e| format!("cannot fsync {}: {e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot rename to {}: {e}", path.display()))?;
    guard.armed = false;
    if let Some(parent) = path.parent() {
        let dir = std::fs::File::open(parent).map_err(|e| e.to_string())?;
        dir.sync_all().map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Guild-config admission pool builder: the shared admission triple's pool
/// half (max 2 + 10 s acquire), reusing the `website_jobs` constants so the
/// second pool in this file cannot drift from the other admission pools.
fn guild_config_admission_pool_options() -> sqlx::postgres::PgPoolOptions {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(crate::website_jobs::ADMISSION_POOL_MAX)
        .acquire_timeout(std::time::Duration::from_secs(
            crate::website_jobs::ADMISSION_ACQUIRE_TIMEOUT_SECS,
        ))
}

/// Guild-config admission statement-timeout server setting (the 15 s
/// admission bound, `website_jobs` parity), applied to the connection
/// options by `governed_guild_config_api`.
fn guild_config_statement_timeout_option() -> (String, String) {
    (
        "statement_timeout".to_owned(),
        format!("{}ms", crate::website_jobs::ADMISSION_STATEMENT_TIMEOUT_MS),
    )
}

async fn governed_guild_config_api(
    token: String,
    guild_id: String,
) -> Result<GuildConfigDiscordApi, String> {
    // Offline CLI fixtures must explicitly opt in and supply both loopback
    // endpoints. A supplied authority (even empty/invalid) is never bypassed.
    if env::var_os("TWO_DATABASE_URL").is_none()
        && env_var("TWO_GUILD_CONFIG_OFFLINE_TEST").as_deref() == Some("1")
    {
        let api_base =
            env_var("GUILD_CONFIG_API_BASE").ok_or("offline fixture API base required")?;
        let cdn_base =
            env_var("GUILD_CONFIG_CDN_BASE").ok_or("offline fixture CDN base required")?;
        return GuildConfigDiscordApi::new(
            Some(&api_base),
            Some(&cdn_base),
            token,
            guild_config::STAGING_BOT_APPLICATION_ID.to_owned(),
            guild_id,
        )
        .map_err(|error| error.to_string());
    }
    let url = env_var("TWO_DATABASE_URL").ok_or("TWO_DATABASE_URL admission authority required")?;
    // Threat-model F6: the admission pool is a second URL parse in this file;
    // enforce the `TWO_DATABASE_TLS` policy (unset means `required`) before
    // SQLx parses the URL (see `docs/database-tls.md`). A bad setting reads
    // as the same redacted authority error as a refused URL.
    let tls = tls_policy_from_env().map_err(|_| "Discord admission authority unavailable")?;
    two_bot_core::database_url::validate(&url)
        .map_err(|_| "Discord admission authority unavailable")?;
    two_bot_core::database_tls::enforce(&url, tls)
        .map_err(|_| "Discord admission authority unavailable")?;
    let options = two_bot_core::database_tls::apply(
        two_bot_core::database_url::connect_options(&url)
            .map_err(|_| "Discord admission authority unavailable")?,
        tls,
    );
    // The admission pool carries the shared admission triple (max 2 + 10 s
    // acquire + 15 s statement, `website_jobs` parity): a hung database
    // refuses instead of hanging the guild-config command.
    let options = options.options([guild_config_statement_timeout_option()]);
    let pool = guild_config_admission_pool_options()
        .connect_with(options)
        .await
        .map_err(|_| "Discord admission authority unavailable")?;
    let admission = two_bot_core::send_admission::PgSendAdmission::new(pool, &token)
        .map_err(|error| error.to_string())?;
    GuildConfigDiscordApi::with_admission(
        env_var("GUILD_CONFIG_API_BASE").as_deref(),
        env_var("GUILD_CONFIG_CDN_BASE").as_deref(),
        token,
        guild_config::STAGING_BOT_APPLICATION_ID.to_owned(),
        guild_id,
        std::sync::Arc::new(admission),
    )
    .map_err(|error| error.to_string())
}

/// Collapse embedded newlines so a failure stays one physical line.
///
/// The stable failure token is appended to the failure line; a multi-line
/// message would leave the earlier lines uncoded for per-line matchers. Used
/// where the interpolated message is known to embed newlines (every
/// `check_staging_token` refusal does).
fn single_line(message: &str) -> String {
    message.replace(['\n', '\r'], " ")
}

async fn cmd_guild_config_snapshot() -> i32 {
    let token = match staging_token() {
        Ok(token) => token,
        Err(message) => {
            eprintln!("guild-config-snapshot: {message} code=snapshot_token_load_failed");
            return 1;
        }
    };
    if let Err(message) = guild_config::check_staging_token(&token) {
        let message = single_line(&message);
        eprintln!("guild-config-snapshot: {message} code=snapshot_token_check_refused");
        return 2;
    }
    let std_env = |name: &str| env_var(name);
    let guild_id = match guild_config::staging_guild_id(&std_env) {
        Ok(guild_id) => guild_id,
        Err(message) => {
            eprintln!("guild-config-snapshot: {message} code=snapshot_guild_id_failed");
            return 2;
        }
    };

    let api = match governed_guild_config_api(token, guild_id.clone()).await {
        Ok(api) => api,
        Err(err) => {
            eprintln!("guild-config-snapshot: {err} code=snapshot_governed_api_failed");
            return 2;
        }
    };
    if let Err(err) = api.assert_identity().await {
        eprintln!("guild-config-snapshot: {err} code=snapshot_identity_failed");
        return 1;
    }
    // TOG-3513: seal the snapshot at capture so restore can refuse a tampered backup.
    let snapshot = match api.capture().await {
        Ok(snapshot) => guild_config::seal_snapshot(snapshot),
        Err(err) => {
            eprintln!("guild-config-snapshot: capture failed: {err} code=snapshot_capture_failed");
            return 1;
        }
    };
    let report = guild_config::drift_against_accepted_spec(&snapshot);
    let stamp = snapshot
        .get("generatedAt")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .replace([':', '.'], "-");
    let output_dir = PathBuf::from(
        env_var("TWO_GUILD_CONFIG_BACKUP_DIR")
            .unwrap_or_else(|| "./guild-config-backups".to_owned()),
    );
    let snapshot_path = output_dir.join(format!("two-staging-guild-config-{stamp}.json"));
    let drift_path = output_dir.join(format!("two-staging-guild-config-{stamp}.drift.json"));
    for (path, value) in [
        (&snapshot_path, serde_json::Value::Object(snapshot.clone())),
        (&drift_path, report.clone()),
    ] {
        if let Err(err) = atomic_json(path, &value) {
            eprintln!("guild-config-snapshot: {err} code=snapshot_atomic_write_failed");
            return 1;
        }
    }

    // Re-read and re-verify: the hash must survive the write, and the seal
    // must be present on disk.
    let on_disk: serde_json::Value = match std::fs::read_to_string(&snapshot_path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
    {
        Some(value) => value,
        None => {
            eprintln!(
                "guild-config-snapshot: cannot re-read {} code=snapshot_reread_failed",
                snapshot_path.display()
            );
            return 1;
        }
    };
    let on_disk_obj = on_disk.as_object().cloned().unwrap_or_default();
    if guild_config::config_hash(&guild_config::canonical_snapshot(&on_disk_obj))
        != report
            .get("snapshotHash")
            .and_then(|v| v.as_str())
            .unwrap_or("")
    {
        eprintln!(
            "guild-config-snapshot: snapshot hash changed after write: {} code=snapshot_hash_changed",
            snapshot_path.display()
        );
        return 1;
    }
    match guild_config::verify_snapshot_integrity(&on_disk_obj) {
        Ok(guild_config::SealState::Sealed) => {}
        _ => {
            eprintln!(
                "guild-config-snapshot: snapshot at {} is missing its integrity seal code=snapshot_seal_missing",
                snapshot_path.display()
            );
            return 1;
        }
    }
    println!("guild-config-snapshot: stored {}", snapshot_path.display());
    let counts = &report["counts"];
    println!(
        "guild-config-snapshot: hash={} roles={} channels={} overwrites={} emojis={}",
        report
            .get("snapshotHash")
            .and_then(|v| v.as_str())
            .unwrap_or("?"),
        counts.get("roles").and_then(|v| v.as_u64()).unwrap_or(0),
        counts.get("channels").and_then(|v| v.as_u64()).unwrap_or(0),
        counts
            .get("overwrites")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        counts.get("emojis").and_then(|v| v.as_u64()).unwrap_or(0),
    );
    println!(
        "guild-config-snapshot: drift={} report={}",
        counts.get("drift").and_then(|v| v.as_u64()).unwrap_or(0),
        drift_path.display()
    );

    let upload_raw =
        env_var("TWO_GUILD_CONFIG_UPLOAD_CMD").or_else(|| env_var("TWO_BACKUP_UPLOAD_CMD"));
    for path in [&snapshot_path, &drift_path] {
        let Some((cmd, upload_args)) = snapshot_upload_argv(upload_raw.as_deref(), path) else {
            eprintln!("guild-config-snapshot: TWO_GUILD_CONFIG_UPLOAD_CMD (or TWO_BACKUP_UPLOAD_CMD) is required; refusing a local-only snapshot code=snapshot_local_only_refused");
            return 2;
        };
        match run_upload_command(&cmd, &upload_args, UPLOAD_COMMAND_TIMEOUT).await {
            UploadOutcome::Success => {}
            outcome => {
                if let Some(message) = upload_failure_message(
                    "guild-config-snapshot",
                    &cmd,
                    &outcome,
                    UPLOAD_COMMAND_TIMEOUT,
                ) {
                    eprintln!("{message}");
                }
                return 1;
            }
        }
    }
    println!("guild-config-snapshot: snapshot and drift report uploaded");
    0
}

// ---------------------------------------------------------------------------
// guild-config-restore
// ---------------------------------------------------------------------------

/// Operator-supplied guild snapshots refuse past 8 MiB, mirroring
/// `http::MAX_RESPONSE_BYTES`. Guild snapshots are KB-scale; the boundary is
/// strict `>` so an exactly-at-cap snapshot still restores.
const MAX_SNAPSHOT_BYTES: u64 = 8 * 1024 * 1024;

async fn cmd_guild_config_restore(args: &[String]) -> i32 {
    let apply = args.iter().any(|a| a == "--apply");
    let confirmed = args.iter().any(|a| a == "--confirm-staging-guild");
    let snapshot_arg = args
        .iter()
        .position(|a| a == "--snapshot")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let evidence_arg = args
        .iter()
        .position(|a| a == "--evidence")
        .and_then(|i| args.get(i + 1))
        .cloned();

    let Some(snapshot_arg) = snapshot_arg else {
        eprintln!("guild-config-restore: usage: two-bot guild-config-restore --snapshot FILE [--confirm-staging-guild --apply] [--evidence FILE]");
        return 2;
    };
    if apply && !confirmed {
        eprintln!("guild-config-restore: refusing to write without --confirm-staging-guild");
        return 2;
    }

    let token = match staging_token() {
        Ok(token) => token,
        Err(message) => {
            eprintln!("guild-config-restore: {message}");
            return 2;
        }
    };
    if let Err(message) = guild_config::check_staging_token(&token) {
        eprintln!("guild-config-restore: {message}");
        return 2;
    }
    let std_env = |name: &str| env_var(name);
    let guild_id = match guild_config::staging_guild_id(&std_env) {
        Ok(guild_id) => guild_id,
        Err(message) => {
            eprintln!("guild-config-restore: {message}");
            return 2;
        }
    };
    if guild_id == guild_config::LIVE_GUILD_ID {
        eprintln!(
            "guild-config-restore: refusing the live guild {}",
            guild_config::LIVE_GUILD_ID
        );
        return 2;
    }

    // Gate before open: a FIFO opened for reading blocks until a writer
    // connects, so non-regular and over-cap snapshots are refused via
    // metadata first and `File::open` only sees likely-regular files. A
    // missing file falls through to the existing read error below.
    if let Ok(meta) = std::fs::metadata(&snapshot_arg) {
        if !meta.is_file() {
            eprintln!(
                "guild-config-restore: cannot read snapshot {snapshot_arg}: not a regular file"
            );
            return 2;
        }
        if meta.len() > MAX_SNAPSHOT_BYTES {
            eprintln!(
                "guild-config-restore: snapshot {snapshot_arg} is {} bytes, exceeding the {MAX_SNAPSHOT_BYTES}-byte snapshot cap; refusing rather than buffering it",
                meta.len()
            );
            return 2;
        }
    }
    // Open once. The descriptor is re-checked after open for a path swapped
    // between the gate and the open; the bounded `take` read mirrors the
    // restore drill, so a file that grows past the cap mid-read is still
    // refused before parsing.
    let snapshot_file = match std::fs::File::open(&snapshot_arg) {
        Ok(file) => file,
        Err(err) => {
            eprintln!("guild-config-restore: cannot read snapshot {snapshot_arg}: {err}");
            return 2;
        }
    };
    // Descriptor-level recheck: refuse a path swapped for a non-regular or
    // over-cap file between the pre-open gate and the open.
    if let Ok(meta) = snapshot_file.metadata() {
        if !meta.is_file() {
            eprintln!(
                "guild-config-restore: cannot read snapshot {snapshot_arg}: not a regular file"
            );
            return 2;
        }
        if meta.len() > MAX_SNAPSHOT_BYTES {
            eprintln!(
                "guild-config-restore: snapshot {snapshot_arg} is {} bytes, exceeding the {MAX_SNAPSHOT_BYTES}-byte snapshot cap; refusing rather than buffering it",
                meta.len()
            );
            return 2;
        }
    }
    let bytes = {
        use std::io::Read as _;
        let mut limited = snapshot_file.take(MAX_SNAPSHOT_BYTES + 1);
        let mut bytes = Vec::new();
        match limited.read_to_end(&mut bytes) {
            Ok(_) => bytes,
            Err(err) => {
                eprintln!("guild-config-restore: cannot read snapshot {snapshot_arg}: {err}");
                return 2;
            }
        }
    };
    if bytes.len() as u64 > MAX_SNAPSHOT_BYTES {
        eprintln!(
            "guild-config-restore: snapshot {snapshot_arg} grew past the {MAX_SNAPSHOT_BYTES}-byte snapshot cap; refusing to parse it"
        );
        return 2;
    }
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(err) => {
            eprintln!("guild-config-restore: cannot read snapshot {snapshot_arg}: {err}");
            return 2;
        }
    };
    let snapshot: serde_json::Map<String, serde_json::Value> = match serde_json::from_str(&text) {
        Ok(serde_json::Value::Object(map)) => map,
        _ => {
            eprintln!(
                "guild-config-restore: cannot parse snapshot {snapshot_arg} as a JSON object"
            );
            return 2;
        }
    };
    if snapshot.get("version").and_then(|v| v.as_u64()) != Some(1) {
        eprintln!(
            "guild-config-restore: unsupported snapshot version {:?}",
            snapshot.get("version")
        );
        return 2;
    }
    // TOG-3513: refuse a tampered backup before any Discord read or write. A
    // SnapshotIntegrityError is the typed negative signal; legacy pre-seal
    // snapshots restore with a warning so old backups stay usable.
    match guild_config::verify_snapshot_integrity(&snapshot) {
        Ok(guild_config::SealState::Legacy) => {
            eprintln!("guild-config-restore: warning: snapshot has no integrity seal (predates TOG-3513); skipping tamper check");
        }
        Ok(guild_config::SealState::Sealed) => {}
        Err(err) => {
            eprintln!("guild-config-restore: refusing tampered backup: {err}");
            return 3;
        }
    }
    if snapshot.get("guildId").and_then(|v| v.as_str()) != Some(guild_id.as_str()) {
        eprintln!(
            "guild-config-restore: snapshot guild {:?} does not match staging guild {guild_id}",
            snapshot.get("guildId")
        );
        return 2;
    }
    if snapshot.get("applicationId").and_then(|v| v.as_str())
        != Some(guild_config::STAGING_BOT_APPLICATION_ID)
    {
        eprintln!(
            "guild-config-restore: snapshot application {:?} is not Owen QA Test {}",
            snapshot.get("applicationId"),
            guild_config::STAGING_BOT_APPLICATION_ID
        );
        return 2;
    }

    let mut api = match governed_guild_config_api(token, guild_id.clone()).await {
        Ok(api) => api,
        Err(err) => {
            eprintln!("guild-config-restore: {err}");
            return 2;
        }
    };
    if let Err(err) = api.assert_identity().await {
        eprintln!("guild-config-restore: {err}");
        return 1;
    }
    let before = match api.capture().await {
        Ok(before) => before,
        Err(err) => {
            eprintln!("guild-config-restore: capture failed: {err}");
            return 1;
        }
    };
    let plan = match guild_config_restore::plan_restore(&snapshot, &before) {
        Ok(plan) => plan,
        Err(err) => {
            eprintln!("guild-config-restore: cannot plan restore: {err}");
            return 1;
        }
    };
    if apply && plan.counts.operations > 0 {
        // Authority and hierarchy come from the guild now, not the backup.
        if let Err(err) = api.assert_restore_permissions(&before, &plan).await {
            eprintln!("guild-config-restore: {err}");
            return 1;
        }
    }
    println!(
        "guild-config-restore: {} {} operation(s)",
        if apply { "applying" } else { "planned" },
        plan.counts.operations
    );

    if !apply {
        for op in &plan.operations {
            println!("WOULD {}", op.label);
        }
        println!(
            "guild-config-restore: counts={:?}; add --confirm-staging-guild --apply to write",
            (
                plan.counts.roles,
                plan.counts.channels,
                plan.counts.overwrites,
                plan.counts.settings,
                plan.counts.emojis
            )
        );
        return 0;
    }

    // Completion prints only as each operation lands: pre-printing every DID
    // left operators believing pending operations had run when the apply
    // failed halfway. A mid-apply failure names landed vs pending operations
    // (labels only, never request bodies) with a stable code.
    let restored_ids =
        match guild_config_restore::apply_restore_plan_with_progress(&mut api, &plan, |op| {
            println!("DID {}", op.label)
        })
        .await
        {
            Ok(ids) => ids,
            Err((err, landed)) => {
                eprintln!(
                    "guild-config-restore: apply failed: {err} code=guild_restore_apply_failed"
                );
                let (landed_ops, pending_ops) =
                    plan.operations.split_at(landed.min(plan.operations.len()));
                if landed_ops.is_empty() {
                    eprintln!("guild-config-restore: landed (0): none");
                } else {
                    for op in landed_ops {
                        eprintln!("guild-config-restore: landed: {}", op.label);
                    }
                }
                for op in pending_ops {
                    eprintln!("guild-config-restore: pending: {}", op.label);
                }
                return 1;
            }
        };
    let after = match api.capture().await {
        Ok(after) => after,
        Err(err) => {
            eprintln!("guild-config-restore: post-restore capture failed: {err}");
            return 1;
        }
    };
    // Remap BEFORE residual planning: apply captured source→live ids, so a
    // correctly recreated resource (e.g. two same-name roles with new Discord
    // ids) is recognised as done rather than refused as ambiguous
    // (TOG-9970 finding 7).
    let remapped_source = guild_config_restore::remap_snapshot_ids(&snapshot, &restored_ids);
    let remaining = match guild_config_restore::plan_restore(&remapped_source, &after) {
        Ok(remaining) => remaining,
        Err(err) => {
            eprintln!("guild-config-restore: post-restore plan failed: {err}");
            return 1;
        }
    };
    let source_hash = guild_config::config_hash(&guild_config::canonical_snapshot(&snapshot));
    let semantic_source_hash =
        guild_config::config_hash(&guild_config::canonical_snapshot(&remapped_source));
    let after_hash = guild_config::config_hash(&guild_config::canonical_snapshot(&after));
    let (before_roles, before_channels, before_overwrites, before_emojis) =
        guild_config::snapshot_counts(&before);
    let (source_roles, source_channels, source_overwrites, source_emojis) =
        guild_config::snapshot_counts(&snapshot);
    let (after_roles, after_channels, after_overwrites, after_emojis) =
        guild_config::snapshot_counts(&after);
    let hashes_equal = after_hash == semantic_source_hash;
    let evidence = serde_json::json!({
        "version": 1,
        "generatedAt": guild_config::unix_now_iso(),
        "guildId": guild_id,
        "source": snapshot_arg,
        "sourceHash": source_hash,
        "semanticSourceHash": semantic_source_hash,
        "beforeHash": guild_config::config_hash(&guild_config::canonical_snapshot(&before)),
        "afterHash": after_hash,
        "counts": {
            "before": {"roles": before_roles, "channels": before_channels, "overwrites": before_overwrites, "emojis": before_emojis},
            "source": {"roles": source_roles, "channels": source_channels, "overwrites": source_overwrites, "emojis": source_emojis},
            "after": {"roles": after_roles, "channels": after_channels, "overwrites": after_overwrites, "emojis": after_emojis},
        },
        "applied": {"roles": plan.counts.roles, "channels": plan.counts.channels, "overwrites": plan.counts.overwrites, "settings": plan.counts.settings, "emojis": plan.counts.emojis, "operations": plan.counts.operations},
        "hashesEqual": hashes_equal,
        "remaining": {"operations": remaining.counts.operations},
        "remainingOperations": remaining.operations.iter().map(|op| &op.label).collect::<Vec<_>>(),
    });
    if let Some(evidence_path) = evidence_arg {
        let path = Path::new(&evidence_path);
        if atomic_json(path, &evidence).is_err() {
            eprintln!("guild-config-restore: cannot write evidence to {evidence_path}");
            return 1;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
    }
    println!("guild-config-restore: before={} after={after_hash} source={source_hash} semantic-source={semantic_source_hash}",
        guild_config::config_hash(&guild_config::canonical_snapshot(&before)));
    println!(
        "guild-config-restore: applied roles={} channels={} overwrites={} settings={} emojis={} remaining={}",
        plan.counts.roles,
        plan.counts.channels,
        plan.counts.overwrites,
        plan.counts.settings,
        plan.counts.emojis,
        remaining.counts.operations
    );
    if !hashes_equal {
        eprintln!(
            "guild-config-restore: residual drift={:?}",
            remaining
                .operations
                .iter()
                .map(|op| &op.label)
                .collect::<Vec<_>>()
        );
        eprintln!("guild-config-restore: restore is incomplete; post-restore hash does not match source hash ({} operation(s) remain)",
            remaining.counts.operations);
        return 1;
    }
    println!(
        "guild-config-restore: complete with {} Discord write(s)",
        api.writes
    );
    0
}

#[cfg(test)]
mod tests {
    use super::{load_staging_token, prune_backups, route, Route};

    #[test]
    fn admission_query_guard_child() {
        if std::env::var_os("ADMISSION_BOOTSTRAP_PROBE").is_none() {
            return;
        }
        crate::admission_test_support::capture_probe(async {
            let error = super::governed_guild_config_api(
                "fixture-token".to_owned(),
                "fixture-guild".to_owned(),
            )
            .await
            .unwrap_err();
            assert_eq!(error, "Discord admission authority unavailable");
        });
    }

    #[test]
    fn admission_query_guard_redacts_dependency_logs() {
        crate::admission_test_support::run_probe("backup_cli::tests::admission_query_guard_child");
    }
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Public staging application ID encoded as base64; no usable credential.
    const TOKEN: &str = "MTQ2OTEzNzYzNjY2Mzc1ODg4OA.unit-test.not-a-secret";

    struct Credentials(PathBuf);

    impl Credentials {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::var_os("PAPERCLIP_RUN_SCRATCH_DIR")
                .or_else(|| std::env::var_os("PAPERCLIP_SCRATCH_DIR"))
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir);
            let dir = root.join(format!(
                "backup-credential-unit-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Credentials {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn staging_credential_takes_precedence_and_trims_newline() {
        let dir = Credentials::new();
        std::fs::write(dir.0.join("discord_staging_token"), format!("  {TOKEN}\n")).unwrap();
        let token = load_staging_token(Some(&dir.0), &|| {
            panic!("a present credential must not read the fallback")
        })
        .unwrap();
        assert!(token == TOKEN, "credential should be trimmed");
    }

    #[test]
    fn staging_credential_falls_back_only_when_absent() {
        let dir = Credentials::new();
        for path in [None, Some(dir.0.as_path())] {
            let token = load_staging_token(path, &|| Some(format!(" {TOKEN}\n"))).unwrap();
            assert!(token == TOKEN, "explicit fallback should be trimmed");
        }
        assert!(load_staging_token(None, &|| None).is_err());
        assert!(load_staging_token(None, &|| Some(" \n".to_owned())).is_err());
    }

    #[test]
    fn staging_credential_rejects_empty_invalid_and_non_utf8_without_fallback() {
        let dir = Credentials::new();
        let path = dir.0.join("discord_staging_token");
        for contents in [
            &b""[..],
            &b" \n\t"[..],
            &b"invalid-credential-secret-marker"[..],
            // Public LIVE application ID, with dummy suffixes only.
            &b"MTUzOTcxMTY4Mzg5ODExODE1NA.fake.not-a-secret"[..],
            &b"\xff\xfeinvalid-credential-secret-marker"[..],
        ] {
            std::fs::write(&path, contents).unwrap();
            let err = load_staging_token(Some(&dir.0), &|| {
                panic!("invalid credentials must not read the fallback")
            })
            .unwrap_err();
            assert!(err.contains("refusing environment fallback"));
            assert!(!err.contains("secret-marker"));
            assert!(!err.contains("not-a-secret"));
        }
    }

    #[test]
    fn staging_credential_read_error_refuses_without_fallback() {
        let dir = Credentials::new();
        // A directory is unreadable as token text even when tests run as root.
        std::fs::create_dir(dir.0.join("discord_staging_token")).unwrap();
        let err = load_staging_token(Some(&dir.0), &|| {
            panic!("unreadable credentials must not read the fallback")
        })
        .unwrap_err();
        assert!(err.contains("cannot read discord_staging_token"));
        assert!(err.contains("refusing environment fallback"));
    }

    #[tokio::test]
    async fn open_pool_rejects_unknown_query_secrets_without_network_access() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!(
            "postgres://fixture-user:fixture-password@127.0.0.1:{port}/db?api_key=fixture-restore-secret"
        );
        let error = super::open_pool(&url).await.unwrap_err();
        assert_eq!(error, "unsupported database URL parameter");
        assert!(!error.contains("fixture"));
        // Validation precedes any socket: nothing may reach the listener,
        // not even one connection attempt.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), listener.accept())
                .await
                .is_err()
        );
    }

    /// Threat-model F6 refusals happen before SQLx parses the URL or opens a
    /// socket: the error is a fixed string and no URL part reaches any log
    /// level. Mirrors `crates/cutover/tests/secret_connection.rs`.
    #[test]
    fn open_pool_with_tls_refusals_never_echo_urls_or_reach_logs() {
        use two_bot_core::database_tls::TlsPolicy;
        let cases = [
            (
                "postgres://fixture-user:fixture-db-password@ep-fixture-host.us-east-2.aws.neon.tech/fixture-db?sslmode=disable",
                TlsPolicy::Required,
                "database sslmode does not require TLS",
            ),
            (
                "postgres://fixture-user:fixture-db-password@ep-fixture-host.us-east-2.aws.neon.tech/fixture-db",
                TlsPolicy::Required,
                "database URL must set sslmode under the required TLS policy",
            ),
            (
                "postgres://fixture-user:fixture-db-password@ep-fixture-host.us-east-2.aws.neon.tech/fixture-db?sslmode=prefer",
                TlsPolicy::LocalOnly,
                "remote database host is refused under the local-only TLS policy",
            ),
            (
                "postgres://fixture-user:fixture-db-password@fixture-host/fixture-db?sslmode=verify-full",
                TlsPolicy::Required,
                "local database host is refused under the required TLS policy",
            ),
            (
                "postgres://fixture-user:fixture-db-password@fixture-host/fixture-db?sslmode=fixture-mode",
                TlsPolicy::LocalOnly,
                "unsupported database sslmode",
            ),
        ];
        let capture = crate::tracing_capture::Capture::default();
        tracing::subscriber::with_default(capture.clone(), || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                for (url, policy, expected) in cases {
                    let error = super::open_pool_with_tls(url, policy).await.unwrap_err();
                    assert_eq!(error, expected);
                    assert!(!error.contains("fixture"), "TLS refusal echoed the URL");
                }
                tracing::warn!("capture remains active");
            });
        });
        let text = capture.text();
        assert!(text.contains("capture remains active"));
        assert!(!text.contains("fixture"), "TLS refusal reached logs");
    }

    /// Child-process probe for the admission pool's TLS fence: a remote
    /// `sslmode=disable` URL must fail as the same redacted authority error,
    /// with no URL part in the error or the logs. Runs only when
    /// `ADMISSION_BOOTSTRAP_PROBE` is set (see the parent test below).
    #[test]
    fn tls_admission_guard_child() {
        if std::env::var_os("ADMISSION_BOOTSTRAP_PROBE").is_none() {
            return;
        }
        let capture = crate::tracing_capture::Capture::default();
        tracing::subscriber::with_default(capture.clone(), || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let error = super::governed_guild_config_api(
                    "fixture-token".to_owned(),
                    "fixture-guild".to_owned(),
                )
                .await
                .unwrap_err();
                assert_eq!(error, "Discord admission authority unavailable");
                assert!(!error.contains("fixture"));
                tracing::warn!("capture remains active");
            });
        });
        let text = capture.text();
        assert!(text.contains("capture remains active"));
        assert!(!text.contains("fixture"), "TLS refusal reached logs");
    }

    /// Parent: re-run only the probe above in an env-cleared child with a
    /// remote `sslmode=disable` fixture URL and no `TWO_DATABASE_TLS` (so the
    /// policy is `Required`). A separate process avoids process-global
    /// environment races with the parallel suite.
    #[test]
    fn tls_admission_guard_redacts_dependency_logs() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .env_clear()
            .args([
                "backup_cli::tests::tls_admission_guard_child",
                "--exact",
                "--nocapture",
            ])
            .env("ADMISSION_BOOTSTRAP_PROBE", "1")
            .env(
                "TWO_DATABASE_URL",
                "postgres://fixture-user:fixture-db-password@ep-fixture-host.us-east-2.aws.neon.tech/fixture-db?sslmode=disable",
            )
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "isolated TLS admission probe failed: {stderr}"
        );
        assert!(stdout.contains("running 1 test"));
        for text in [stdout, stderr] {
            assert!(!text.contains("fixture"));
        }
    }

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn help_after_any_subcommand_routes_to_usage() {
        for subcommand in super::SUBCOMMANDS {
            for rest in [
                &["--help"][..],
                &["-h"],
                &["extra", "--help"],
                &["--snapshot", "-h"],
                &["--force", "--dry-run", "--help"],
            ] {
                assert_eq!(
                    route(subcommand, &strings(rest)),
                    Route::Help,
                    "{subcommand} {rest:?}"
                );
            }
        }
    }

    #[test]
    fn help_after_option_terminator_is_an_operand() {
        // restore reads `-- -h` as a file named `-h`; backup-upload refuses
        // two arguments itself, so `--help` is still never its path.
        for subcommand in ["restore", "backup-upload", "guild-config-restore"] {
            assert_eq!(
                route(subcommand, &strings(&["--", "-h"])),
                Route::Run,
                "{subcommand}"
            );
        }
    }

    #[test]
    fn extra_argument_to_argumentless_subcommand_is_a_usage_error() {
        for subcommand in ["backup", "guild-config-snapshot"] {
            // Literal tokens: the frozen vocabulary scan reads this file, so
            // the assertion spells out the same literals production emits.
            let code = if subcommand == "backup" {
                "code=backup_usage"
            } else {
                "code=snapshot_usage"
            };
            for extra in ["now", "--force", "--"] {
                let Route::Usage(message) = route(subcommand, &strings(&[extra])) else {
                    panic!("{subcommand} {extra} must refuse");
                };
                assert!(message.starts_with(&format!("{subcommand}: unexpected argument")));
                assert!(message.contains(&format!("{extra:?}")));
                assert!(message.ends_with(super::USAGE));
                let first = message.lines().next().unwrap();
                assert!(
                    first.ends_with(code),
                    "{subcommand} {extra}: first line must end with the code: {first:?}"
                );
            }
            assert_eq!(route(subcommand, &[]), Route::Run);
        }
    }

    #[test]
    fn operand_subcommands_and_foreign_help_run_unchanged() {
        assert_eq!(
            route("restore", &strings(&["dump.ndjson.gz", "--dry-run"])),
            Route::Run
        );
        assert_eq!(
            route("backup-upload", &strings(&["dump.ndjson.gz"])),
            Route::Run
        );
        assert_eq!(
            route("guild-config-restore", &strings(&["--snapshot", "s.json"])),
            Route::Run
        );
        // `db` and `erase-member` own their help; unknown names reach the
        // unknown-subcommand refusal.
        for subcommand in ["db", "erase-member", "serve"] {
            assert_eq!(route(subcommand, &strings(&["--help"])), Route::Run);
        }
    }

    #[tokio::test]
    async fn bare_boot_allow_owed_falls_through_to_service_preflight() {
        // The exact documented service-start flag is not a backup
        // subcommand: 100 tells `main` to run the boot preflight in `run`.
        assert_eq!(
            super::dispatch(&strings(&[two_bot_core::disable_preflight::OVERRIDE_FLAG])).await,
            100
        );
    }

    #[tokio::test]
    async fn bare_boot_near_misses_and_extra_args_stay_usage_errors() {
        // Only the exact single flag falls through; typos, unknown flags
        // and the flag beside another argument keep exit 2 without touching
        // env, DB or network.
        for args in [
            &["--allow-owwed"][..],
            &["--allow-owed-releases"][..],
            &["--allow-owed=1"][..],
            &["--allow_owed"][..],
            &["--frobnicate"][..],
            &["--allow-owed", "--frobnicate"][..],
            &["backup", "--allow-owed"][..],
        ] {
            assert_eq!(super::dispatch(&strings(args)).await, 2, "{args:?}");
        }
    }

    #[test]
    fn retention_counts_only_published_archive_names() {
        let dir = Credentials::new();
        let published = dir.0.join("two-funnel-previous.ndjson.gz");
        std::fs::write(&published, b"previous archive").unwrap();
        let interrupted = [
            ".two-funnel-interrupted.ndjson.gz.tmp",
            "two-funnel-interrupted.ndjson.gz.partial",
        ];
        for name in interrupted {
            std::fs::write(dir.0.join(name), b"partial output").unwrap();
        }
        assert!(prune_backups(&dir.0, 1).unwrap().is_empty());
        assert_eq!(std::fs::read(&published).unwrap(), b"previous archive");

        let next = dir.0.join("two-funnel-next.ndjson.gz");
        std::fs::write(&next, b"next archive").unwrap();
        assert_eq!(prune_backups(&dir.0, 1).unwrap().len(), 1);
        assert_eq!(
            usize::from(published.exists()) + usize::from(next.exists()),
            1
        );
        for name in interrupted {
            assert_eq!(std::fs::read(dir.0.join(name)).unwrap(), b"partial output");
        }
    }

    /// Serialises the bounded upload-helper fixtures. Other suites can
    /// still fork while a script is open for writing, so these tests run
    /// scripts through `/bin/sh` rather than exec the freshly written file:
    /// an inherited write fd must not cause a spurious ETXTBSY spawn failure.
    #[cfg(unix)]
    static UPLOAD_FIXTURE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Task-owned temporary executable fixtures for the bounded upload
    /// helper. Paths are baked into the scripts so tests never mutate
    /// process-global environment (the parallel suite shares it).
    #[cfg(unix)]
    fn write_upload_fixture(dir: &Credentials, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.0.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[cfg(unix)]
    const UPLOAD_SECRET_MARKER: &str = "fixture-credential-marker";
    #[cfg(unix)]
    const UPLOAD_REMOTE_MARKER: &str = "fixture-remote-output-marker";

    #[cfg(unix)]
    fn secret_argv(file: &str) -> Vec<String> {
        vec![
            "--token".to_owned(),
            UPLOAD_SECRET_MARKER.to_owned(),
            file.to_owned(),
        ]
    }

    #[cfg(unix)]
    fn upload_fixture_argv(helper: &std::path::Path, file: &str) -> Vec<String> {
        let mut args = vec![helper.to_string_lossy().into_owned()];
        args.extend(secret_argv(file));
        args
    }

    #[test]
    fn upload_helper_default_budget_is_thirty_minutes() {
        assert_eq!(
            super::UPLOAD_COMMAND_TIMEOUT,
            std::time::Duration::from_secs(30 * 60)
        );
    }

    /// The snapshot upload loop parses once per path: `None` refuses with the
    /// existing local-only message and exit 2, `Some` keeps byte-identical
    /// upload inputs with the file appended last.
    #[test]
    fn snapshot_upload_argv_parses_once_per_path() {
        use std::path::PathBuf;
        let snap = PathBuf::from("two-staging-guild-config-stamp.json");
        let drift = PathBuf::from("two-staging-guild-config-stamp.drift.json");
        assert!(super::snapshot_upload_argv(None, &snap).is_none());
        assert!(super::snapshot_upload_argv(Some("  "), &snap).is_none());
        assert!(super::snapshot_upload_argv(Some(""), &drift).is_none());
        let (cmd, args) = super::snapshot_upload_argv(Some("/bin/true"), &snap).unwrap();
        assert_eq!(cmd, "/bin/true");
        assert_eq!(args, vec![snap.to_string_lossy().into_owned()]);
        let (cmd, args) = super::snapshot_upload_argv(Some("cp -t /srv/backups"), &drift).unwrap();
        assert_eq!(cmd, "cp");
        assert_eq!(
            args,
            vec![
                "-t".to_owned(),
                "/srv/backups".to_owned(),
                drift.to_string_lossy().into_owned()
            ]
        );
        let (_, snap_args) =
            super::snapshot_upload_argv(Some("cp -t /srv/backups"), &snap).unwrap();
        let (_, drift_args) =
            super::snapshot_upload_argv(Some("cp -t /srv/backups"), &drift).unwrap();
        assert_ne!(snap_args, drift_args);
        assert!(snap_args.last().unwrap().ends_with(".json"));
        assert!(drift_args.last().unwrap().ends_with(".drift.json"));
        assert_eq!(snap_args.len(), 3);
        assert_eq!(drift_args.len(), 3);
    }

    /// Prompt success stays success and produces no failure line.
    #[cfg(unix)]
    #[tokio::test]
    async fn upload_helper_success_reports_no_failure() {
        let _fixture_guard = UPLOAD_FIXTURE_LOCK.lock().await;
        let dir = Credentials::new();
        let helper = write_upload_fixture(
            &dir,
            "upload-ok.sh",
            "#!/bin/sh\necho fixture-remote-output-marker\nexit 0\n",
        );
        let outcome = super::run_upload_command(
            "/bin/sh",
            &upload_fixture_argv(&helper, "archive.ndjson.gz"),
            std::time::Duration::from_secs(30),
        )
        .await;
        assert!(matches!(outcome, super::UploadOutcome::Success));
        assert_eq!(
            super::upload_failure_message(
                "backup",
                "upload-ok.sh",
                &outcome,
                std::time::Duration::from_secs(30),
            ),
            None
        );
    }

    /// A nonzero exit keeps its status and never echoes argv or the child's
    /// own stdout back through the failure line.
    #[cfg(unix)]
    #[tokio::test]
    async fn upload_helper_nonzero_exit_reports_status_without_args() {
        let _fixture_guard = UPLOAD_FIXTURE_LOCK.lock().await;
        let dir = Credentials::new();
        let helper = write_upload_fixture(
            &dir,
            "upload-fail.sh",
            "#!/bin/sh\necho fixture-remote-output-marker\nexit 3\n",
        );
        // Model a writable fd inherited by an unrelated concurrent child.
        let _writer = std::fs::OpenOptions::new()
            .write(true)
            .open(&helper)
            .unwrap();
        let outcome = super::run_upload_command(
            "/bin/sh",
            &upload_fixture_argv(&helper, "archive.ndjson.gz"),
            std::time::Duration::from_secs(30),
        )
        .await;
        let super::UploadOutcome::ExitNonzero(status) = &outcome else {
            panic!("nonzero helper must report ExitNonzero, got {outcome:?}");
        };
        assert_eq!(status.code(), Some(3));
        let message = super::upload_failure_message(
            "backup",
            "upload-fail.sh",
            &outcome,
            std::time::Duration::from_secs(30),
        )
        .unwrap();
        assert!(message.contains("3"), "failure must name the status");
        for leaked in [
            UPLOAD_SECRET_MARKER,
            UPLOAD_REMOTE_MARKER,
            "archive.ndjson.gz",
        ] {
            assert!(
                !message.contains(leaked),
                "failure line must not echo argv or remote output: {message}"
            );
        }
    }

    /// An unstartable command stays a spawn failure with no argv echo.
    #[cfg(unix)]
    #[tokio::test]
    async fn upload_helper_spawn_failure_reports_without_args() {
        // No fixture write here, but this spawn would still fork while a
        // sibling holds a fixture script open for writing, so join the same
        // lock to keep the write-and-spawn windows disjoint.
        let _fixture_guard = UPLOAD_FIXTURE_LOCK.lock().await;
        let program = "/nonexistent-fixture-upload-helper";
        let outcome = super::run_upload_command(
            program,
            &secret_argv("archive.ndjson.gz"),
            std::time::Duration::from_secs(30),
        )
        .await;
        let super::UploadOutcome::SpawnFailed(_) = &outcome else {
            panic!("missing helper must report SpawnFailed, got {outcome:?}");
        };
        let message = super::upload_failure_message(
            "backup",
            program,
            &outcome,
            std::time::Duration::from_secs(30),
        )
        .unwrap();
        assert!(
            message.contains(program),
            "spawn failure must name the configured program: {message}"
        );
        for leaked in [UPLOAD_SECRET_MARKER, "archive.ndjson.gz"] {
            assert!(
                !message.contains(leaked),
                "spawn failure must not echo argv: {message}"
            );
        }
    }

    /// A hung helper is terminated and reaped within the injected deadline:
    /// the fixture records its own PID, `exec`s into `sleep 3600` so the
    /// recorded PID *is* the owned direct child and natural exit cannot
    /// explain a quick return. The elapsed bound pins the kill (without it
    /// the call would hang until the fixture exits) and the immediate
    /// `/proc` absence pins the reap before the orphan reaper can hide a
    /// zombie. (A leaked zombie would still own a `/proc` entry until
    /// reaped.)
    #[cfg(unix)]
    #[tokio::test]
    async fn upload_helper_timeout_terminates_and_reaps_owned_child() {
        let _fixture_guard = UPLOAD_FIXTURE_LOCK.lock().await;
        let dir = Credentials::new();
        let pidfile = dir.0.join("hung.pid");
        let helper = write_upload_fixture(
            &dir,
            "upload-hang.sh",
            &format!(
                "#!/bin/sh\necho $$ > '{}'\nexec sleep 3600\n",
                pidfile.to_string_lossy()
            ),
        );
        let started = std::time::Instant::now();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            super::run_upload_command(
                "/bin/sh",
                &upload_fixture_argv(&helper, "archive.ndjson.gz"),
                std::time::Duration::from_millis(300),
            ),
        )
        .await
        .expect("bounded helper must return well before the fixture natural exit");
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "hung helper must be terminated within seconds, took {elapsed:?}"
        );
        assert!(
            matches!(outcome, super::UploadOutcome::TimedOut),
            "hung helper must report TimedOut, got {outcome:?}"
        );
        let message = super::upload_failure_message(
            "backup",
            "upload-hang.sh",
            &outcome,
            std::time::Duration::from_millis(300),
        )
        .unwrap();
        assert!(
            message.contains("timed out"),
            "timeout must say so: {message}"
        );
        assert!(
            message.contains("300ms"),
            "timeout must name the injected budget: {message}"
        );
        for leaked in [
            UPLOAD_SECRET_MARKER,
            UPLOAD_REMOTE_MARKER,
            "archive.ndjson.gz",
        ] {
            assert!(
                !message.contains(leaked),
                "timeout line must not echo argv or remote output: {message}"
            );
        }
        let pid = std::fs::read_to_string(&pidfile).unwrap();
        let pid = pid.trim().to_owned();
        assert!(!pid.is_empty(), "hung fixture must record its PID");
        let proc = PathBuf::from(format!("/proc/{pid}"));
        assert!(
            !proc.exists(),
            "timed-out upload child {pid} must already be reaped on return"
        );
        let quiet_until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while proc.exists() && std::time::Instant::now() < quiet_until {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            !proc.exists(),
            "timed-out upload child {pid} must be terminated and reaped"
        );
    }

    // Stable-error patch coverage: the acceptance suite pins tokens via source
    // text, which llvm-cov does not count as execution. These runtime tests
    // execute each changed failure line so the patch gate measures them.
    // Env-mutating tests share one lock; no other unit test reads these vars.
    static STABLE_CODE_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn clear_stable_code_env() {
        for name in [
            "TWO_DATABASE_URL",
            "TWO_BACKUP_KEEP",
            "TWO_BACKUP_DIR",
            "TWO_BACKUP_S3_ENDPOINT",
            "TWO_BACKUP_S3_BUCKET",
            "TWO_BACKUP_S3_ACCESS_KEY_ID",
            "TWO_BACKUP_S3_SECRET_ACCESS_KEY",
            "CREDENTIALS_DIRECTORY",
            "DISCORD_STAGING_BOT_TOKEN",
            "DISCORD_STAGING_GUILD_ID",
            "TWO_GUILD_CONFIG_OFFLINE_TEST",
            "GUILD_CONFIG_API_BASE",
            "GUILD_CONFIG_CDN_BASE",
            "TWO_GUILD_CONFIG_BACKUP_DIR",
            "TWO_GUILD_CONFIG_UPLOAD_CMD",
            "TWO_BACKUP_UPLOAD_CMD",
        ] {
            std::env::remove_var(name);
        }
    }

    #[test]
    fn stable_codes_cover_upload_wait_failed() {
        let message = super::upload_failure_message(
            "backup",
            "upload-wait.sh",
            &super::UploadOutcome::WaitFailed("interrupted".to_owned()),
            std::time::Duration::from_secs(30),
        )
        .unwrap();
        assert!(message.contains("upload wait failed"));
    }

    #[tokio::test]
    async fn stable_codes_cover_backup_missing_database_url() {
        let _env = STABLE_CODE_ENV_LOCK.lock().await;
        clear_stable_code_env();
        let exit = super::cmd_backup().await;
        assert_eq!(exit, 1);
        clear_stable_code_env();
    }

    #[tokio::test]
    async fn stable_codes_cover_backup_retention_refused() {
        let _env = STABLE_CODE_ENV_LOCK.lock().await;
        clear_stable_code_env();
        std::env::set_var("TWO_DATABASE_URL", "postgres://127.0.0.1:1/db");
        std::env::set_var("TWO_BACKUP_KEEP", "0");
        let exit = super::cmd_backup().await;
        assert_eq!(exit, 1);
        clear_stable_code_env();
    }

    #[tokio::test]
    async fn stable_codes_cover_backup_dest_create_failed() {
        let _env = STABLE_CODE_ENV_LOCK.lock().await;
        clear_stable_code_env();
        std::env::set_var("TWO_DATABASE_URL", "postgres://127.0.0.1:1/db");
        let dir = Credentials::new();
        let blocker = dir.0.join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let bad_dest = blocker.join("subdir");
        std::env::set_var("TWO_BACKUP_DIR", bad_dest.to_str().unwrap());
        let exit = super::cmd_backup().await;
        assert_eq!(exit, 1);
        clear_stable_code_env();
    }

    #[tokio::test]
    async fn stable_codes_cover_backup_pool_open_failed() {
        let _env = STABLE_CODE_ENV_LOCK.lock().await;
        clear_stable_code_env();
        std::env::set_var("TWO_DATABASE_URL", "postgres://127.0.0.1:1/db");
        let dir = Credentials::new();
        std::env::set_var("TWO_BACKUP_DIR", dir.0.to_str().unwrap());
        let exit = super::cmd_backup().await;
        assert_eq!(exit, 1);
        clear_stable_code_env();
    }

    #[tokio::test]
    async fn stable_codes_cover_upload_s3_target() {
        let _env = STABLE_CODE_ENV_LOCK.lock().await;
        clear_stable_code_env();
        let dir = Credentials::new();
        let path = dir.0.join("dump.ndjson.gz");
        std::fs::write(&path, b"fake-dump").unwrap();
        let args = vec![path.to_string_lossy().into_owned()];
        let exit = super::cmd_backup_upload(&args).await;
        assert_eq!(exit, 1);
        clear_stable_code_env();
    }

    #[tokio::test]
    async fn stable_codes_cover_upload_put_failed() {
        let _env = STABLE_CODE_ENV_LOCK.lock().await;
        clear_stable_code_env();
        std::env::set_var("TWO_BACKUP_S3_ENDPOINT", "http://127.0.0.1:1");
        std::env::set_var("TWO_BACKUP_S3_BUCKET", "test-bucket");
        std::env::set_var("TWO_BACKUP_S3_ACCESS_KEY_ID", "test-key");
        std::env::set_var("TWO_BACKUP_S3_SECRET_ACCESS_KEY", "test-secret");
        let dir = Credentials::new();
        let path = dir.0.join("dump.ndjson.gz");
        std::fs::write(&path, b"fake-dump").unwrap();
        let args = vec![path.to_string_lossy().into_owned()];
        let exit = super::cmd_backup_upload(&args).await;
        assert_eq!(exit, 1);
        clear_stable_code_env();
    }

    #[tokio::test]
    async fn stable_codes_cover_snapshot_token_load_failed() {
        let _env = STABLE_CODE_ENV_LOCK.lock().await;
        clear_stable_code_env();
        let exit = super::cmd_guild_config_snapshot().await;
        assert_eq!(exit, 1);
        clear_stable_code_env();
    }

    #[tokio::test]
    async fn stable_codes_cover_snapshot_token_check_refused() {
        let _env = STABLE_CODE_ENV_LOCK.lock().await;
        clear_stable_code_env();
        std::env::set_var("DISCORD_STAGING_BOT_TOKEN", "invalid-token");
        let exit = super::cmd_guild_config_snapshot().await;
        assert_eq!(exit, 2);
        clear_stable_code_env();
    }

    #[test]
    fn single_line_flattens_every_token_check_refusal() {
        // Every `check_staging_token` refusal embeds a newline; the snapshot
        // site flattens it so the stable token lands on the only line.
        // Segments are base64 app ids; no usable credential.
        for token in [
            "MTUzOTcxMTY4Mzg5ODExODE1NA.unit-test.not-a-secret",
            "MTUzNzYyOTY4MjQ0OTY0OTcyNA.unit-test.not-a-secret",
            "MTIzNDU2Nzg5MDEyMzQ1Njc4OQ.unit-test.not-a-secret",
            "invalid-token",
        ] {
            let message =
                two_bot_core::backup::guild_config::check_staging_token(token).unwrap_err();
            assert!(message.contains('\n'), "{token}: expected a newline hazard");
            let flat = super::single_line(&message);
            assert!(!flat.contains('\n') && !flat.contains('\r'), "{flat:?}");
            assert!(flat.contains("Refusing to run"), "{flat:?}");
        }
    }

    #[test]
    fn single_line_keeps_single_line_text_verbatim() {
        assert_eq!(super::single_line("plain message"), "plain message");
        assert_eq!(super::single_line("a\nb\rc"), "a b c");
    }

    #[tokio::test]
    async fn stable_codes_cover_snapshot_guild_id_failed() {
        let _env = STABLE_CODE_ENV_LOCK.lock().await;
        clear_stable_code_env();
        std::env::set_var("DISCORD_STAGING_BOT_TOKEN", TOKEN);
        let exit = super::cmd_guild_config_snapshot().await;
        assert_eq!(exit, 2);
        clear_stable_code_env();
    }

    #[tokio::test]
    async fn stable_codes_cover_snapshot_governed_api_failed() {
        let _env = STABLE_CODE_ENV_LOCK.lock().await;
        clear_stable_code_env();
        std::env::set_var("DISCORD_STAGING_BOT_TOKEN", TOKEN);
        std::env::set_var(
            "DISCORD_STAGING_GUILD_ID",
            two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID,
        );
        let exit = super::cmd_guild_config_snapshot().await;
        assert_eq!(exit, 2);
        clear_stable_code_env();
    }

    #[tokio::test]
    async fn stable_codes_cover_snapshot_identity_failed() {
        let _env = STABLE_CODE_ENV_LOCK.lock().await;
        clear_stable_code_env();
        std::env::set_var("DISCORD_STAGING_BOT_TOKEN", TOKEN);
        std::env::set_var(
            "DISCORD_STAGING_GUILD_ID",
            two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID,
        );
        std::env::set_var("TWO_GUILD_CONFIG_OFFLINE_TEST", "1");
        std::env::set_var("GUILD_CONFIG_API_BASE", "http://127.0.0.1:1");
        std::env::set_var("GUILD_CONFIG_CDN_BASE", "http://127.0.0.1:1");
        let exit = super::cmd_guild_config_snapshot().await;
        assert_eq!(exit, 1);
        clear_stable_code_env();
    }

    #[tokio::test]
    async fn read_cap_cover_restore_missing_snapshot() {
        // A missing --snapshot keeps the pre-existing read error (exit 2):
        // the open-once refactor must not change that message.
        let _env = STABLE_CODE_ENV_LOCK.lock().await;
        clear_stable_code_env();
        std::env::set_var("DISCORD_STAGING_BOT_TOKEN", TOKEN);
        std::env::set_var(
            "DISCORD_STAGING_GUILD_ID",
            two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID,
        );
        let dir = Credentials::new();
        let missing = dir.0.join("no-such-snapshot.json");
        let args = vec![
            "--snapshot".to_owned(),
            missing.to_string_lossy().into_owned(),
        ];
        let exit = super::cmd_guild_config_restore(&args).await;
        assert_eq!(exit, 2);
        clear_stable_code_env();
    }

    #[tokio::test]
    async fn read_cap_cover_restore_non_regular_snapshot() {
        // A directory passed as --snapshot is refused before any read: the
        // metadata check sees a non-regular file and exits 2.
        let _env = STABLE_CODE_ENV_LOCK.lock().await;
        clear_stable_code_env();
        std::env::set_var("DISCORD_STAGING_BOT_TOKEN", TOKEN);
        std::env::set_var(
            "DISCORD_STAGING_GUILD_ID",
            two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID,
        );
        let dir = Credentials::new();
        let args = vec![
            "--snapshot".to_owned(),
            dir.0.to_string_lossy().into_owned(),
        ];
        let exit = super::cmd_guild_config_restore(&args).await;
        assert_eq!(exit, 2);
        clear_stable_code_env();
    }

    #[tokio::test]
    async fn read_cap_cover_restore_invalid_utf8_snapshot() {
        // Non-UTF-8 bytes keep the pre-existing read error (exit 2): the
        // bounded take-read plus from_utf8 must match read_to_string here.
        let _env = STABLE_CODE_ENV_LOCK.lock().await;
        clear_stable_code_env();
        std::env::set_var("DISCORD_STAGING_BOT_TOKEN", TOKEN);
        std::env::set_var(
            "DISCORD_STAGING_GUILD_ID",
            two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID,
        );
        let dir = Credentials::new();
        let path = dir.0.join("binary.json");
        std::fs::write(&path, [0xffu8, 0xfe, 0x00, 0x62]).unwrap();
        let args = vec!["--snapshot".to_owned(), path.to_string_lossy().into_owned()];
        let exit = super::cmd_guild_config_restore(&args).await;
        assert_eq!(exit, 2);
        clear_stable_code_env();
    }

    struct StableCodeFake {
        base: String,
        task: tokio::task::JoinHandle<()>,
    }

    async fn stable_code_fake_handler(
        axum::extract::State(fail_guild): axum::extract::State<
            std::sync::Arc<std::sync::atomic::AtomicBool>,
        >,
        request: axum::extract::Request,
    ) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
        let path = request.uri().path().to_owned();
        let guild_id = two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID;
        let app_id = two_bot_core::backup::guild_config::STAGING_BOT_APPLICATION_ID;
        let guild_path = format!("/guilds/{guild_id}");
        let roles_path = format!("{guild_path}/roles");
        let channels_path = format!("{guild_path}/channels");
        if path == "/users/@me" {
            let body = serde_json::json!({"id": app_id});
            return (axum::http::StatusCode::OK, axum::Json(body));
        }
        if path == "/users/@me/guilds" {
            let body = serde_json::json!([{"id": guild_id}]);
            return (axum::http::StatusCode::OK, axum::Json(body));
        }
        if path == guild_path {
            if fail_guild.load(std::sync::atomic::Ordering::Relaxed) {
                let body = serde_json::json!({"message": "fixture failure"});
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(body),
                );
            }
            let body = serde_json::json!({"id": guild_id, "name": "Loopback guild"});
            return (axum::http::StatusCode::OK, axum::Json(body));
        }
        if path == roles_path {
            let body = serde_json::json!([]);
            return (axum::http::StatusCode::OK, axum::Json(body));
        }
        if path == channels_path {
            let body = serde_json::json!([]);
            return (axum::http::StatusCode::OK, axum::Json(body));
        }
        let body = serde_json::json!([]);
        (axum::http::StatusCode::OK, axum::Json(body))
    }

    impl StableCodeFake {
        async fn start(fail_guild: bool) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(fail_guild));
            let app = axum::Router::new()
                .fallback(stable_code_fake_handler)
                .with_state(flag);
            let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            Self { base, task }
        }
    }

    impl Drop for StableCodeFake {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    #[tokio::test]
    async fn stable_codes_cover_snapshot_capture_failed() {
        let _env = STABLE_CODE_ENV_LOCK.lock().await;
        clear_stable_code_env();
        let fake = StableCodeFake::start(true).await;
        let dir = Credentials::new();
        std::env::set_var("DISCORD_STAGING_BOT_TOKEN", TOKEN);
        std::env::set_var(
            "DISCORD_STAGING_GUILD_ID",
            two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID,
        );
        std::env::set_var("TWO_GUILD_CONFIG_OFFLINE_TEST", "1");
        std::env::set_var("GUILD_CONFIG_API_BASE", &fake.base);
        std::env::set_var("GUILD_CONFIG_CDN_BASE", &fake.base);
        std::env::set_var("TWO_GUILD_CONFIG_BACKUP_DIR", dir.0.to_str().unwrap());
        let exit = super::cmd_guild_config_snapshot().await;
        assert_eq!(exit, 1);
        clear_stable_code_env();
    }

    #[tokio::test]
    async fn stable_codes_cover_snapshot_atomic_write_failed() {
        let _env = STABLE_CODE_ENV_LOCK.lock().await;
        clear_stable_code_env();
        let fake = StableCodeFake::start(false).await;
        let dir = Credentials::new();
        let blocker = dir.0.join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let bad_dir = blocker.join("captured");
        std::env::set_var("DISCORD_STAGING_BOT_TOKEN", TOKEN);
        std::env::set_var(
            "DISCORD_STAGING_GUILD_ID",
            two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID,
        );
        std::env::set_var("TWO_GUILD_CONFIG_OFFLINE_TEST", "1");
        std::env::set_var("GUILD_CONFIG_API_BASE", &fake.base);
        std::env::set_var("GUILD_CONFIG_CDN_BASE", &fake.base);
        std::env::set_var("TWO_GUILD_CONFIG_BACKUP_DIR", bad_dir.to_str().unwrap());
        let exit = super::cmd_guild_config_snapshot().await;
        assert_eq!(exit, 1);
        clear_stable_code_env();
    }

    #[tokio::test]
    async fn stable_codes_cover_snapshot_local_only_refused() {
        let _env = STABLE_CODE_ENV_LOCK.lock().await;
        clear_stable_code_env();
        let fake = StableCodeFake::start(false).await;
        let dir = Credentials::new();
        std::env::set_var("DISCORD_STAGING_BOT_TOKEN", TOKEN);
        std::env::set_var(
            "DISCORD_STAGING_GUILD_ID",
            two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID,
        );
        std::env::set_var("TWO_GUILD_CONFIG_OFFLINE_TEST", "1");
        std::env::set_var("GUILD_CONFIG_API_BASE", &fake.base);
        std::env::set_var("GUILD_CONFIG_CDN_BASE", &fake.base);
        std::env::set_var("TWO_GUILD_CONFIG_BACKUP_DIR", dir.0.to_str().unwrap());
        let exit = super::cmd_guild_config_snapshot().await;
        assert_eq!(exit, 2);
        clear_stable_code_env();
    }
}

#[cfg(test)]
#[path = "backup_pool_timeout_acceptance_tests.rs"]
mod pool_timeout_acceptance_tests;

#[cfg(test)]
#[path = "backup_pool_triple_pin_tests.rs"]
mod pool_triple_pin_tests;
