//! Migration entrypoint with an explicit staging/production target.
//!
//! ```text
//! staging-migrate --target staging|production --plan|--apply --source-sha <40hex>
//!   --staging-host <host> --staging-database <db> [--staging-branch-id <branch>]
//!   (staging only)
//!   --production-host <host> --production-database <db>
//!   [--production-branch-id <branch>] (production only)
//!   --recovery-evidence-ref <ref> --acl-plan-ref <ref>
//!   [--expected-pending <ascending,comma-separated versions>]
//!   [--expected-store-pending <ascending,comma-separated store versions>]
//!   [--plan-manifest-sha256 <64hex> --store-plan-manifest-sha256 <64hex>
//!    --plan-run-id <run id>
//!    --plan-manifest-path <producing run's downloaded manifest>]
//! ```
//!
//! The runner plans, applies and verifies two chains with separate ledgers:
//! this crate's cutover migrations (`public._sqlx_migrations`) and the store
//! migrations (`public._two_bot_migrations`). The database URL comes only
//! from the target and mode's fixed binding: staging plan reads
//! TWO_BOT_STAGING_PLAN_DATABASE_URL (a login holding only
//! two_bot_migrator_ro), staging apply reads TWO_BOT_STAGING_MIGRATOR_DATABASE_URL,
//! production plan reads TWO_BOT_PRODUCTION_PLAN_DATABASE_URL, production apply
//! reads TWO_BOT_PRODUCTION_MIGRATOR_DATABASE_URL. --apply refuses before any DDL
//! unless --expected-pending equals the computed cutover pending list exactly,
//! --expected-store-pending equals the computed store pending list exactly,
//! --plan-manifest-sha256 equals the SHA-256 of the plan job's uploaded manifest
//! for the same source SHA, --store-plan-manifest-sha256 equals the same
//! manifest's store projection, and the downloaded manifest at --plan-manifest-path
//! (fetched by the workflow from the --plan-run-id run) carries both hashes,
//! proving the bound hashes came from the named producing run; --plan prints that
//! manifest (including both hashes) and ignores the plan-binding flags. Intake
//! cap: a downloaded manifest over 8 MiB is refused (exit 2) before parsing,
//! so an oversized artifact never reaches the provenance comparison.
//!
//! `--staging-branch-id` (staging target) and `--production-branch-id`
//! (production target) pin the PlanetScale branch id (non-secret): the pin is
//! required for `*.psdb.cloud` hosts and must match the binding username's
//! `{role}.{branch_id}` suffix; empty (the default) means no pin and is
//! accepted for Neon and test hosts.
//! Exit: 0 ok, 2 refused before any DDL, 1 failed (evidence on stdout).

// Operator CLI reports intentionally use stdout; runtime/library modules do not.
#![allow(clippy::print_stdout)]

use two_bot_cutover::staging_migrate::{
    run, Request, RunError, Target, PLAN_URL_ENV, PROD_PLAN_URL_ENV, PROD_URL_ENV, URL_ENV,
};

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
            "--target"
            | "--source-sha"
            | "--staging-host"
            | "--staging-database"
            | "--staging-branch-id"
            | "--production-host"
            | "--production-database"
            | "--production-branch-id"
            | "--recovery-evidence-ref"
            | "--acl-plan-ref"
            | "--expected-pending"
            | "--expected-store-pending"
            | "--plan-manifest-sha256"
            | "--store-plan-manifest-sha256"
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
    let target = match values.get("--target").map(String::as_str) {
        Some(raw) => match Target::parse(raw) {
            Some(target) => target,
            None => return refused("target must be staging or production"),
        },
        None => return refused("--target staging|production is required"),
    };
    let get = |k: &str| values.get(k).cloned().unwrap_or_default();
    // Each target uses only its own host/database flags: a staging run never
    // reads a production pin and a production run never reads a staging pin.
    let (expected_host, expected_database, expected_branch_id) = match target {
        Target::Staging => {
            if values.contains_key("--production-host")
                || values.contains_key("--production-database")
                || values.contains_key("--production-branch-id")
            {
                return refused(
                    "staging target takes --staging-host/--staging-database/--staging-branch-id only",
                );
            }
            (
                get("--staging-host"),
                get("--staging-database"),
                get("--staging-branch-id"),
            )
        }
        Target::Production => {
            if values.contains_key("--staging-host")
                || values.contains_key("--staging-database")
                || values.contains_key("--staging-branch-id")
            {
                return refused(
                    "production target takes --production-host/--production-database/--production-branch-id only",
                );
            }
            (
                get("--production-host"),
                get("--production-database"),
                get("--production-branch-id"),
            )
        }
    };
    // Each target and mode reads only its own binding: a plan run can never
    // borrow the migrator credential, a staging run can never borrow the
    // production credential, and an absent binding refuses before connecting.
    let binding = match (target, apply) {
        (Target::Staging, true) => URL_ENV,
        (Target::Staging, false) => PLAN_URL_ENV,
        (Target::Production, true) => PROD_URL_ENV,
        (Target::Production, false) => PROD_PLAN_URL_ENV,
    };
    let req = Request {
        url: std::env::var(binding).ok(),
        target,
        source_sha: get("--source-sha"),
        expected_host,
        expected_database,
        expected_branch_id,
        recovery_evidence_ref: get("--recovery-evidence-ref"),
        acl_plan_ref: get("--acl-plan-ref"),
        apply,
        expected_pending: values.get("--expected-pending").cloned(),
        expected_store_pending: values.get("--expected-store-pending").cloned(),
        plan_manifest_sha256: values.get("--plan-manifest-sha256").cloned(),
        store_plan_manifest_sha256: values.get("--store-plan-manifest-sha256").cloned(),
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
