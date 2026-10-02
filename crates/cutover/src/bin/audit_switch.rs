//! Operator kill-switch for the two-bot audit mirror (legacy `audit-switch`).
//!
//! `KillSwitchSnapshot::decide` plus migration `0340_operational_audit.sql`
//! define the semantics and the mirror honors the halt; this binary is the
//! operator handle so halting/resuming live sends plus pending-row retries no
//! longer needs hand SQL. Presence in `audit_kill_switch` is the halt; the
//! first engagement wins, and a failed halt read fails open in the mirror
//! (the failure is reported here, never silently treated as clear).
//!
//! Usage: audit_switch --halt --actor <snowflake> | --resume | --status
//!   Env: TWO_DATABASE_URL (agent-testdb test databases only; anything else
//!   is refused before connecting, and no credential is ever printed).
//!   Exit codes: 0 ok (already-engaged and already-clear count as ok),
//!   1 database failure, 2 usage or refused target.

// Operator CLI reports intentionally use stdout; runtime/library modules do not.
#![allow(clippy::print_stdout)]

use time::format_description::well_known::Rfc3339;
use two_bot_core::audit_store::AuditStore;
use two_bot_cutover::cli::{open_db, Args};
use two_bot_cutover::{is_snowflake, legacy_copy::options::guarded_target};

const USAGE: &str =
    "Usage: audit_switch --halt --actor <snowflake> | --resume | --status [--help]\n\
    Halt or resume audit mirror sends plus pending-row retries.\n\
    Presence in audit_kill_switch is the halt; first engagement wins.\n\
    Env: TWO_DATABASE_URL (agent-testdb test databases only).\n\
    Exit codes: 0 ok (already-engaged / already-clear count as ok),\n\
    1 database failure, 2 usage or refused target.";

fn usage_error(message: &str) -> ! {
    eprintln!("{message}\n{USAGE}");
    std::process::exit(2);
}

async fn engage(store: &AuditStore, actor: &str) -> i32 {
    match store.engage_halt(actor).await {
        Ok(true) => {
            println!("audit_switch: halt engaged by {actor}");
            0
        }
        Ok(false) => {
            // First engagement wins, so name the holder instead of the loser.
            let holder = store
                .delivery_halt()
                .await
                .ok()
                .flatten()
                .map(|halt| halt.engaged_by)
                .unwrap_or_else(|| "unknown".to_owned());
            println!("audit_switch: halt already engaged by {holder}");
            0
        }
        Err(error) => {
            eprintln!("halt failed: {error}");
            1
        }
    }
}

async fn resume(store: &AuditStore) -> i32 {
    match store.disengage_halt().await {
        Ok(true) => {
            println!("audit_switch: halt cleared");
            0
        }
        Ok(false) => {
            println!("audit_switch: halt already clear");
            0
        }
        Err(error) => {
            eprintln!("resume failed: {error}");
            1
        }
    }
}

async fn report(store: &AuditStore) -> i32 {
    let halt = match store.delivery_halt().await {
        Ok(halt) => halt,
        Err(error) => {
            // Fail-open is the mirror's read path, not this reporter's: an
            // unreadable switch must surface, never print CLEAR.
            eprintln!("halt read failed: {error}");
            return 1;
        }
    };
    let pending = match store.pending_ids().await {
        Ok(ids) => ids.len(),
        Err(error) => {
            eprintln!("pending queue read failed: {error}");
            return 1;
        }
    };
    match halt {
        Some(halt) => {
            let at = halt
                .engaged_at
                .format(&Rfc3339)
                .unwrap_or_else(|_| halt.engaged_at.unix_timestamp().to_string());
            println!(
                "audit_switch status: HALTED\n  engaged_by: {}\n  engaged_at: {at}\n  claimable_pending: {pending}",
                halt.engaged_by
            );
        }
        None => {
            println!("audit_switch status: CLEAR\n  claimable_pending: {pending}");
        }
    }
    0
}

async fn run() -> i32 {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = Args::parse(&argv);
    if args.has("help") {
        println!("{USAGE}");
        return 0;
    }
    // A typo'd flag on a kill-switch must fail loudly, never silently
    // no-op while the operator believes the halt engaged.
    for flag in &args.flags {
        if !matches!(flag.as_str(), "halt" | "resume" | "status" | "actor") {
            usage_error(&format!("unknown argument --{flag}"));
        }
    }
    for key in args.values.keys() {
        if !matches!(key.as_str(), "halt" | "resume" | "status" | "actor") {
            usage_error(&format!("unknown argument --{key}"));
        }
    }
    if !args.positionals.is_empty() {
        usage_error("unexpected positional argument");
    }
    if ["halt", "resume", "status"]
        .iter()
        .filter(|command| args.has(command))
        .count()
        != 1
    {
        usage_error("pass exactly one of --halt, --resume or --status");
    }
    let actor = if args.has("halt") {
        match args.get("actor").filter(|actor| is_snowflake(actor)) {
            Some(actor) => actor.to_owned(),
            None => usage_error("--halt requires --actor <snowflake> (operator identity)"),
        }
    } else if args.has("actor") {
        usage_error("--actor only applies to --halt")
    } else {
        String::new()
    };

    let url = std::env::var("TWO_DATABASE_URL").unwrap_or_default();
    if url.trim().is_empty() {
        eprintln!("TWO_DATABASE_URL is required.");
        std::process::exit(2);
    }
    // Fail-closed fence before any socket: only disposable agent-testdb
    // databases. The guard errors are static; the URL itself is never printed.
    if let Err(error) = guarded_target(&url, false) {
        eprintln!("refusing database target (agent-testdb test databases only): {error}");
        std::process::exit(2);
    }
    // Migrations off: an operator switch must never build schema by accident.
    let db = open_db(&args, true).await;
    let store = AuditStore::new(db.pool());
    let code = if args.has("halt") {
        engage(&store, &actor).await
    } else if args.has("resume") {
        resume(&store).await
    } else {
        report(&store).await
    };
    db.close().await;
    code
}

#[tokio::main]
async fn main() {
    std::process::exit(run().await);
}
