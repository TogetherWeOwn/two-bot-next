//! Operator handle for the one-shot force-fresh IDENTIFY directive
//! (`docs/gateway-recovery.md`). Dry run by default: it prints the guild, the
//! shard, the checkpoint age and the directive, and writes nothing. `--apply`
//! arms the directive; the next boot of that guild/shard consumes it.
//!
//! Every refusal happens before the database is opened: `--guild` must equal
//! the configured `GUILD_ID`, and arming the live guild needs
//! `--allow-live-guild`. Migrations stay off; this tool never builds schema
//! and never touches the `gateway_sessions` row.
//!
//! Exit codes: 0 ok (already armed counts as ok), 1 database failure,
//! 2 usage or refused target.

use two_bot_core::gateway_session::{boot_action, BootAction};
use two_bot_cutover::cli::{now_iso, open_db, require_guild, require_guild_read, Args};
use two_bot_cutover::gateway_session::{ArmOutcome, ForceIdentifyStatus, GatewaySessionStore};

const USAGE: &str = "Usage: gateway-force-identify --guild <ID> [--shard <N>] [--apply --reason <text>] [--allow-live-guild]\n\
    Dry run by default: prints guild, shard, checkpoint age and directive; writes nothing.\n\
    --apply arms a one-shot IDENTIFY for the next boot of that guild/shard.\n\
    Env: GUILD_ID (configured guild; --guild must match), TWO_DATABASE_URL.\n\
    Exit codes: 0 ok, 1 database failure, 2 usage or refused target.";

/// Mirrors the column CHECK in migration 0321.
const REASON_MAX_CHARS: usize = 512;

fn refuse(message: &str) -> ! {
    eprintln!("gateway-force-identify: {message}\n{USAGE}");
    std::process::exit(2);
}

fn iso(ms: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .ok()
        .and_then(|at| {
            at.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_else(|| format!("{ms}ms"))
}

fn directive_line(status: Option<&ForceIdentifyStatus>) -> String {
    match status {
        None => "none".to_owned(),
        Some(status) => match status.consumed_at_ms {
            None => format!("ARMED at {} ({})", iso(status.armed_at_ms), status.reason),
            Some(consumed) => format!(
                "consumed at {} (armed {}, {})",
                iso(consumed),
                iso(status.armed_at_ms),
                status.reason
            ),
        },
    }
}

async fn report(store: &GatewaySessionStore, now_ms: i64) -> Result<(), sqlx::Error> {
    let saved = store.load().await?;
    let checkpoint = match &saved {
        None => "none".to_owned(),
        Some(saved) => {
            let unarmed = match boot_action(Some(saved), now_ms) {
                BootAction::Resume => "RESUME",
                BootAction::Identify | BootAction::DiscardAndIdentify => "IDENTIFY",
            };
            format!(
                "age {}ms, seq {} (an unarmed boot now would {unarmed})",
                now_ms.saturating_sub(saved.updated_at_ms),
                saved.sequence
            )
        }
    };
    let status = store.force_identify_status().await?;
    println!("  checkpoint: {checkpoint}");
    println!("  directive:  {}", directive_line(status.as_ref()));
    Ok(())
}

async fn run() -> i32 {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = Args::parse(&argv);
    if args.has("help") {
        println!("{USAGE}");
        return 0;
    }
    // A typo'd flag must fail loudly, never silently fall back to a dry run
    // while the operator believes the directive is armed.
    if !args.positionals.is_empty()
        || args
            .flags
            .iter()
            .any(|flag| !matches!(flag.as_str(), "apply" | "allow-live-guild"))
        || args
            .values
            .keys()
            .any(|key| !matches!(key.as_str(), "guild" | "shard" | "reason"))
    {
        refuse("unknown or malformed argument");
    }
    let apply = args.has("apply");
    let guild = if apply {
        require_guild(&args, "guild")
    } else {
        require_guild_read(&args, "guild")
    };
    let configured = std::env::var("GUILD_ID").unwrap_or_default();
    if configured.trim().parse::<u64>().ok() != guild.parse::<u64>().ok() {
        refuse("--guild differs from the configured GUILD_ID (or GUILD_ID is unset)");
    }
    let shard = match args.get("shard").map(str::parse::<i32>) {
        None => 0,
        Some(Ok(shard)) if shard >= 0 => shard,
        Some(_) => refuse("--shard must be a non-negative integer"),
    };
    let reason = args.get("reason").map(str::trim).unwrap_or_default();
    if apply && (reason.is_empty() || reason.chars().count() > REASON_MAX_CHARS) {
        refuse("--apply requires --reason <text> of 1-512 characters");
    }

    let db = open_db(&args, true).await;
    let store = GatewaySessionStore::new(db.pool().clone(), guild.clone(), shard);
    println!(
        "gateway-force-identify {} at {}",
        if apply {
            "APPLY"
        } else {
            "DRY RUN (no writes)"
        },
        now_iso()
    );
    println!("  guild:      {guild}");
    println!("  shard:      {shard}");
    let now_ms = two_bot_core::funnel::now_millis_for_test();
    let mut code = match report(&store, now_ms).await {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("read failed (is migration 0321 applied?): {error}");
            1
        }
    };
    if code == 0 && apply {
        code = match store.arm_force_identify(reason).await {
            Ok(ArmOutcome::Armed) => {
                println!("ARMED: the next boot of guild {guild} shard {shard} IDENTIFYs once.");
                0
            }
            Ok(ArmOutcome::AlreadyArmed) => {
                println!("already armed; the pending directive is unchanged.");
                0
            }
            Err(error) => {
                eprintln!("arm failed: {error}");
                1
            }
        };
    } else if code == 0 {
        println!("Re-run with --apply --reason <text> to arm a one-shot IDENTIFY.");
    }
    db.close().await;
    code
}

#[tokio::main]
async fn main() {
    std::process::exit(run().await);
}
