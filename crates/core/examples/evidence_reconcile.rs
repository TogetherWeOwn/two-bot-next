//! Runnable B2 soak reconciler (`docs/evidence-route.md` step 3).
//!
//! Reads (1) the QA expected-actions JSON `[{alias, family, at,
//! disposition?, reason?}]` and (2) the sanitized rows artifact from the
//! staging-events-read workflow (`{rows: [{ordinal, event_type,
//! recorded_at}], truncated}`), builds an `EvidenceLedger` with the deployed
//! revision, assigns opaque per-row keys (`r<ordinal>`), and writes `export()`
//! to `evidence-soak_expected_committed-{window}.json`.
//!
//! Fails closed: exit 1 when `gaps > 0` or any overflow/truncation flag is
//! set (the window is not proven); exit 2 on malformed input, IO failure or
//! bad usage, in which case nothing is written.
//!
//! Build with `cargo build -p two-bot-core --example evidence_reconcile
//! --locked` (on the persistent controller route the build through
//! `scripts/cargo_cache.py` per `docs/build-cache.md`, which does not take
//! `run`), then call the built binary as:
//! `./target/debug/examples/evidence_reconcile --expected qa_expected.json
//! --rows staging_rows.json --revision <deploy-sha> --window
//! 2026-10-09T20-11-06Z [--out-dir .]`

// Local reconcile CLI intentionally reports its result to stdout/stderr.
#![allow(clippy::print_stdout)]
#![allow(clippy::print_stderr)]

use std::path::PathBuf;
use std::process::ExitCode;

use two_bot_core::evidence::{build_soak_ledger, evidence_packet_filename, SOAK_LEDGER_RULE_ID};

const USAGE: &str = "usage: evidence_reconcile --expected <PATH> --rows <PATH> \
    --revision <DEPLOY_SHA> --window <WINDOW_START> [--out-dir <DIR>]";

struct Args {
    expected: PathBuf,
    rows: PathBuf,
    revision: String,
    window: String,
    out_dir: PathBuf,
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut expected: Option<PathBuf> = None;
    let mut rows: Option<PathBuf> = None;
    let mut revision: Option<String> = None;
    let mut window: Option<String> = None;
    let mut out_dir = PathBuf::from(".");
    let mut rest = argv.iter().skip(1);
    while let Some(flag) = rest.next() {
        let value = rest
            .next()
            .ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))?;
        match flag.as_str() {
            "--expected" => expected = Some(PathBuf::from(value)),
            "--rows" => rows = Some(PathBuf::from(value)),
            "--revision" => revision = Some(value.clone()),
            "--window" => window = Some(value.clone()),
            "--out-dir" => out_dir = PathBuf::from(value),
            _ => return Err(format!("unknown flag {flag}\n{USAGE}")),
        }
    }
    Ok(Args {
        expected: expected.ok_or_else(|| format!("missing --expected\n{USAGE}"))?,
        rows: rows.ok_or_else(|| format!("missing --rows\n{USAGE}"))?,
        revision: revision.ok_or_else(|| format!("missing --revision\n{USAGE}"))?,
        window: window.ok_or_else(|| format!("missing --window\n{USAGE}"))?,
        out_dir,
    })
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let args = match parse_args(&argv) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("evidence_reconcile: {e}");
            return ExitCode::from(2);
        }
    };
    if args.window.is_empty() {
        eprintln!("evidence_reconcile: --window must not be empty\n{USAGE}");
        return ExitCode::from(2);
    }
    let expected_json = match std::fs::read_to_string(&args.expected) {
        Ok(text) => text,
        Err(e) => {
            eprintln!(
                "evidence_reconcile: cannot read {}: {e}",
                args.expected.display()
            );
            return ExitCode::from(2);
        }
    };
    let rows_json = match std::fs::read_to_string(&args.rows) {
        Ok(text) => text,
        Err(e) => {
            eprintln!(
                "evidence_reconcile: cannot read {}: {e}",
                args.rows.display()
            );
            return ExitCode::from(2);
        }
    };
    let ledger = match build_soak_ledger(args.revision, &expected_json, &rows_json) {
        Ok(ledger) => ledger,
        Err(e) => {
            eprintln!("evidence_reconcile: refused input: {e}");
            return ExitCode::from(2);
        }
    };
    let packet = ledger.export();
    let gaps = packet["counts"]["gaps"].as_u64().unwrap_or(u64::MAX);
    let expected_overflow = packet["truncated"]["expected_overflow"]
        .as_bool()
        .unwrap_or(true);
    let receipts_overflow = packet["truncated"]["receipts_overflow"]
        .as_bool()
        .unwrap_or(true);
    let truncated = expected_overflow || receipts_overflow;
    let filename = evidence_packet_filename(SOAK_LEDGER_RULE_ID, &args.window);
    let path = args.out_dir.join(&filename);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!(
                    "evidence_reconcile: cannot create {}: {e}",
                    parent.display()
                );
                return ExitCode::from(2);
            }
        }
    }
    let text = serde_json::to_string_pretty(&packet).unwrap_or_else(|_| "{}".to_owned());
    if let Err(e) = std::fs::write(&path, format!("{text}\n")) {
        eprintln!("evidence_reconcile: cannot write {}: {e}", path.display());
        return ExitCode::from(2);
    }
    let matched = packet["counts"]["matched"].as_u64().unwrap_or(0);
    let expected = packet["counts"]["expected"].as_u64().unwrap_or(0);
    println!(
        "wrote {} expected={expected} matched={matched} gaps={gaps} truncated={truncated}",
        path.display()
    );
    if gaps > 0 || truncated {
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
