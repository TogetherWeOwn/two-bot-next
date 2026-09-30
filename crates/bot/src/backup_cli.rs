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

use std::env;
use std::path::{Path, PathBuf};

use two_bot_core::backup::{
    dump, dump_file, guild_config, guild_config_api::GuildConfigDiscordApi, guild_config_restore,
    http, retention, s3,
};

fn env_var(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
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
two-bot backup & restore (TOG-9881)

  two-bot backup
      Dump all bot-owned tables (v3 format) to TWO_BACKUP_DIR
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
      deliberately NOT TWO_DATABASE_URL: restoring truncates the target,
      so aiming it at production must be said twice, on purpose.
      `RESTORE VERIFIED` on the last line, and exit 0, is the only success.

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
      --confirm-staging-guild.
";

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
    match args[0].as_str() {
        "backup" => cmd_backup().await,
        "restore" => cmd_restore(&args[1..]).await,
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
            eprintln!("backup: TWO_DATABASE_URL must be set to a Postgres URL.");
            return 1;
        }
    };
    let dest = env_var("TWO_BACKUP_DIR").unwrap_or_else(|| "./backups".to_owned());

    // Checked before the dump, not before the prune: a setting that would
    // delete every backup stops the run while it is still a no-op.
    let keep = match retention::parse_keep(env_var("TWO_BACKUP_KEEP").as_deref()) {
        Ok(keep) => keep,
        Err(err) => {
            eprintln!("backup: {err}.");
            eprintln!("        Refusing to run: every reading of that value prunes all backups.");
            return 1;
        }
    };

    if let Err(err) = std::fs::create_dir_all(&dest) {
        eprintln!("backup: cannot create {dest}: {err}");
        return 1;
    }
    let stamp = guild_config::filename_stamp();
    let out = PathBuf::from(&dest).join(format!("two-funnel-{stamp}.ndjson.gz"));

    let pool = match sqlx::PgPool::connect(&url).await {
        Ok(pool) => pool,
        Err(_) => {
            eprintln!("backup: cannot connect; database details redacted");
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
                    eprintln!("backup: wrote {} but cannot stat it: {err}", out.display());
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
                     keeping existing archives, skipping upload."
                );
                pool.close().await;
                return 1;
            }
        }
        Err(err) => {
            eprintln!("backup: dump failed: {err}");
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
            eprintln!("backup: prune failed: {err}");
            return 1;
        }
    }

    match s3::build_upload_argv(
        env_var("TWO_BACKUP_UPLOAD_CMD").as_deref(),
        &out.to_string_lossy(),
    ) {
        Some((cmd, upload_args)) => {
            match tokio::process::Command::new(&cmd)
                .args(&upload_args)
                .status()
                .await
            {
                Ok(status) if status.success() => println!("backup: uploaded via {cmd}"),
                Ok(status) => {
                    eprintln!("backup: upload failed: {cmd} exited {status}");
                    return 1;
                }
                Err(err) => {
                    eprintln!("backup: upload failed to start ({cmd}): {err}");
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

    let pool = match sqlx::PgPool::connect(url.as_ref().expect("checked")).await {
        Ok(pool) => pool,
        Err(_) => {
            eprintln!("restore: cannot connect; database details redacted");
            return 1;
        }
    };
    // NOTE: two-bot-next migrations land under S6 (Founding Engineer). Until
    // then the target must already carry the schema; dump()/restore() refuse
    // with a named table when it does not. S6 plugs migrate() in here.
    match dump::restore(&pool, Path::new(&file)).await {
        Ok(report) => {
            println!("restore: dump taken {}", report.manifest.created_at);
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
        match sqlx::PgPool::connect(url).await {
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
                            Err(_) => {
                                "(no such table - the restore would migrate first)".to_owned()
                            }
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
            "backup-upload-s3: expected exactly one argument (the dump path), got {}",
            args.len()
        );
        return 2;
    }
    let dump_path = Path::new(&args[0]);
    let size = match std::fs::metadata(dump_path) {
        Ok(meta) if meta.is_file() => meta.len(),
        _ => {
            eprintln!(
                "backup-upload-s3: cannot read {}: not a regular file",
                dump_path.display()
            );
            return 1;
        }
    };
    // An empty dump would upload happily and restore to nothing.
    if size == 0 {
        eprintln!(
            "backup-upload-s3: refusing to upload an empty file: {}",
            dump_path.display()
        );
        return 1;
    }

    let std_env = |name: &str| env_var(name);
    let target = match s3::load_s3_target(&std_env) {
        Ok(target) => target,
        Err(err) => {
            eprintln!("backup-upload-s3: {err}");
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
                "backup-upload-s3: cannot read {}: {err}",
                dump_path.display()
            );
            return 1;
        }
    };

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
                    "backup-upload-s3: PUT {} {}{}",
                    res.status.as_u16(),
                    res.status.canonical_reason().unwrap_or(""),
                    res.detail()
                );
                return 1;
            }
            println!(
                "backup-upload-s3: stored {}/{} etag={}",
                target.bucket,
                key,
                res.header("etag").unwrap_or("(none)")
            );
            0
        }
        Err(err) => {
            eprintln!("backup-upload-s3: PUT failed: {err}");
            1
        }
    }
}

// ---------------------------------------------------------------------------
// guild-config-snapshot
// ---------------------------------------------------------------------------

fn atomic_json(path: &Path, value: &serde_json::Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    let text = serde_json::to_string_pretty(value).map_err(|e| format!("cannot serialise: {e}"))?;
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&tmp)
            .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
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
    if let Some(parent) = path.parent() {
        let dir = std::fs::File::open(parent).map_err(|e| e.to_string())?;
        dir.sync_all().map_err(|e| e.to_string())?;
    }
    Ok(())
}

async fn cmd_guild_config_snapshot() -> i32 {
    let token = match staging_token() {
        Ok(token) => token,
        Err(message) => {
            eprintln!("guild-config-snapshot: {message}");
            return 1;
        }
    };
    if let Err(message) = guild_config::check_staging_token(&token) {
        eprintln!("guild-config-snapshot: {message}");
        return 2;
    }
    let std_env = |name: &str| env_var(name);
    let guild_id = match guild_config::staging_guild_id(&std_env) {
        Ok(guild_id) => guild_id,
        Err(message) => {
            eprintln!("guild-config-snapshot: {message}");
            return 2;
        }
    };

    let api = match GuildConfigDiscordApi::new(
        env_var("GUILD_CONFIG_API_BASE").as_deref(),
        env_var("GUILD_CONFIG_CDN_BASE").as_deref(),
        token,
        guild_config::STAGING_BOT_APPLICATION_ID.to_owned(),
        guild_id.clone(),
    ) {
        Ok(api) => api,
        Err(err) => {
            eprintln!("guild-config-snapshot: {err}");
            return 2;
        }
    };
    if let Err(err) = api.assert_identity().await {
        eprintln!("guild-config-snapshot: {err}");
        return 1;
    }
    // TOG-3513: seal the snapshot at capture so restore can refuse a tampered backup.
    let snapshot = match api.capture().await {
        Ok(snapshot) => guild_config::seal_snapshot(snapshot),
        Err(err) => {
            eprintln!("guild-config-snapshot: capture failed: {err}");
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
            eprintln!("guild-config-snapshot: {err}");
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
                "guild-config-snapshot: cannot re-read {}",
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
            "guild-config-snapshot: snapshot hash changed after write: {}",
            snapshot_path.display()
        );
        return 1;
    }
    match guild_config::verify_snapshot_integrity(&on_disk_obj) {
        Ok(guild_config::SealState::Sealed) => {}
        _ => {
            eprintln!(
                "guild-config-snapshot: snapshot at {} is missing its integrity seal",
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
    let Some((cmd, upload_args)) =
        s3::build_upload_argv(upload_raw.as_deref(), &snapshot_path.to_string_lossy())
    else {
        eprintln!("guild-config-snapshot: TWO_GUILD_CONFIG_UPLOAD_CMD (or TWO_BACKUP_UPLOAD_CMD) is required; refusing a local-only snapshot");
        return 2;
    };
    for path in [&snapshot_path, &drift_path] {
        let (cmd, upload_args) =
            s3::build_upload_argv(upload_raw.as_deref(), &path.to_string_lossy()).expect("checked");
        match tokio::process::Command::new(&cmd)
            .args(&upload_args)
            .status()
            .await
        {
            Ok(status) if status.success() => {}
            Ok(status) => {
                eprintln!("guild-config-snapshot: upload failed with exit {status}");
                return 1;
            }
            Err(err) => {
                eprintln!("guild-config-snapshot: upload failed to start: {err}");
                return 1;
            }
        }
    }
    let _ = (cmd, upload_args);
    println!("guild-config-snapshot: snapshot and drift report uploaded");
    0
}

// ---------------------------------------------------------------------------
// guild-config-restore
// ---------------------------------------------------------------------------

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

    let text = match std::fs::read_to_string(&snapshot_arg) {
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

    let mut api = match GuildConfigDiscordApi::new(
        env_var("GUILD_CONFIG_API_BASE").as_deref(),
        env_var("GUILD_CONFIG_CDN_BASE").as_deref(),
        token,
        guild_config::STAGING_BOT_APPLICATION_ID.to_owned(),
        guild_id.clone(),
    ) {
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
    for op in &plan.operations {
        println!("{} {}", if apply { "DID" } else { "WOULD" }, op.label);
    }

    if !apply {
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

    let restored_ids = match guild_config_restore::apply_restore_plan(&mut api, &plan).await {
        Ok(ids) => ids,
        Err(err) => {
            eprintln!("guild-config-restore: apply failed: {err}");
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
    use super::{load_staging_token, prune_backups};
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
}
