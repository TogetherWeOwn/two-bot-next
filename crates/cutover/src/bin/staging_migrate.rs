//! Staging-only migration entrypoint (TOG-11572). See `two_bot_cutover::staging_migrate`.
//!
//! ```text
//! staging-migrate --plan|--apply --source-sha <40hex> --staging-host <host>
//!   --staging-database <db> --recovery-evidence-ref <ref> --acl-plan-ref <ref>
//! ```
//!
//! The database URL comes only from TWO_BOT_STAGING_MIGRATOR_DATABASE_URL.
//! Exit: 0 ok, 2 refused before any DDL, 1 failed (evidence on stdout).

// Operator CLI reports intentionally use stdout; runtime/library modules do not.
#![allow(clippy::print_stdout)]

use two_bot_cutover::staging_migrate::{run, Request, RunError, URL_ENV};

fn main() {
    std::process::exit(real_main());
}

fn real_main() -> i32 {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut apply = None;
    let mut values = std::collections::HashMap::new();
    let mut it = argv.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--plan" => apply = Some(false),
            "--apply" => apply = Some(true),
            "--source-sha"
            | "--staging-host"
            | "--staging-database"
            | "--recovery-evidence-ref"
            | "--acl-plan-ref" => match it.next() {
                Some(v) => {
                    values.insert(arg.clone(), v.clone());
                }
                None => return refused(&format!("{arg} needs a value")),
            },
            other => return refused(&format!("unknown argument {other}")),
        }
    }
    let Some(apply) = apply else {
        return refused("choose exactly --plan or --apply");
    };
    let get = |k: &str| values.get(k).cloned().unwrap_or_default();
    let req = Request {
        url: std::env::var(URL_ENV).ok(),
        source_sha: get("--source-sha"),
        expected_host: get("--staging-host"),
        expected_database: get("--staging-database"),
        recovery_evidence_ref: get("--recovery-evidence-ref"),
        acl_plan_ref: get("--acl-plan-ref"),
        apply,
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(_) => return refused("runtime init failed"),
    };
    match runtime.block_on(run(&req)) {
        Ok(manifest) => {
            println!("{manifest}");
            0
        }
        Err(RunError::Refused(m)) => refused(&m),
        Err(RunError::Failed(m, evidence)) => {
            println!("{evidence}");
            eprintln!("staging-migrate failed: {m}");
            1
        }
    }
}

fn refused(message: &str) -> i32 {
    eprintln!("staging-migrate refused: {message}");
    2
}
