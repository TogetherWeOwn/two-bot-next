//! Staging-only migration entrypoint (TOG-11572). See `two_bot_cutover::staging_migrate`.
//!
//! ```text
//! staging-migrate --plan|--apply --source-sha <40hex> --staging-host <host>
//!   --staging-database <db> [--staging-branch-id <branch>] --recovery-evidence-ref <ref>
//!   --acl-plan-ref <ref>
//!   [--expected-pending <ascending,comma-separated versions>]
//!   [--plan-manifest-sha256 <64hex> --plan-run-id <run id>
//!    --plan-manifest-path <producing run's downloaded manifest>]
//! ```
//!
//! `--staging-branch-id` pins the PlanetScale branch id (non-secret): it is
//! required for `*.psdb.cloud` hosts and must match the binding username's
//! `{role}.{branch_id}` suffix; empty (the default) means no pin and is
//! accepted for Neon and test hosts.
//!
//! The database URL comes only from the mode's fixed binding:
//! TWO_BOT_STAGING_PLAN_DATABASE_URL for --plan (a login holding only
//! two_bot_migrator_ro, physically read-only; --plan refuses a login that also
//! holds two_bot_migrator), TWO_BOT_STAGING_MIGRATOR_DATABASE_URL for --apply. --apply refuses before any DDL unless --expected-pending equals
//! the computed pending list exactly, --plan-manifest-sha256 equals the SHA-256
//! of the plan job's uploaded manifest for the same source SHA, and the downloaded
//! manifest at --plan-manifest-path (fetched by the workflow from the
//! --plan-run-id run) carries that same hash, proving the bound hash came
//! from the named producing run; --plan prints that manifest (including its
//! own hash) and ignores the plan-binding flags.
//! Exit: 0 ok, 2 refused before any DDL, 1 failed (evidence on stdout).

// Operator CLI reports intentionally use stdout; runtime/library modules do not.
#![allow(clippy::print_stdout)]

use two_bot_cutover::staging_migrate::{run, Request, RunError, PLAN_URL_ENV, URL_ENV};

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
            | "--staging-branch-id"
            | "--recovery-evidence-ref"
            | "--acl-plan-ref"
            | "--expected-pending"
            | "--plan-manifest-sha256"
            | "--plan-run-id"
            | "--plan-manifest-path" => match it.next() {
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
    // Each mode reads only its own binding: a plan run can never borrow the
    // migrator credential, and an absent RO binding refuses before connecting.
    let binding = if apply { URL_ENV } else { PLAN_URL_ENV };
    let req = Request {
        url: std::env::var(binding).ok(),
        source_sha: get("--source-sha"),
        expected_host: get("--staging-host"),
        expected_database: get("--staging-database"),
        expected_branch_id: values
            .get("--staging-branch-id")
            .cloned()
            .unwrap_or_default(),
        recovery_evidence_ref: get("--recovery-evidence-ref"),
        acl_plan_ref: get("--acl-plan-ref"),
        apply,
        expected_pending: values.get("--expected-pending").cloned(),
        plan_manifest_sha256: values.get("--plan-manifest-sha256").cloned(),
        plan_run_id: values.get("--plan-run-id").cloned(),
        plan_manifest_path: values.get("--plan-manifest-path").cloned(),
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
