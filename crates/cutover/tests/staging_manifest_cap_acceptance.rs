//! Staging-manifest intake cap: an oversized producing-run manifest is refused
//! before parsing (failing first, tests only).
//!
//! Base: `164343832156c3172dbbcfddd53892353cae23a5` (main at acceptance
//! authoring). At that base the plan-provenance check fully buffers and parses
//! the workflow-fetched manifest before comparing hashes: an oversized but
//! otherwise valid manifest is accepted. The oversized test is RED on that
//! base for exactly that reason, while the provenance controls stay green. The
//! paired builder implements production code only and must NOT edit this file;
//! the tests pass unmodified once the read enforces the contract below. No
//! failing test merges: this file is handed off unpublished for the builder's
//! final green implementation pull request. (CLI byte caps are covered by
//! `untrusted_file_intake_cap_acceptance.rs`, MEE6 row bounds by
//! `mee6_intake_bounds_acceptance.rs`.)
//!
//! ## Builder contract
//!
//! - One byte cap: `INTAKE_CAP_BYTES` (8 MiB, matching the capped
//!   operator-tool siblings). Exactly the cap is admitted; one byte more is
//!   refused before any parse.
//! - Size vocabulary (case-insensitive; one size word plus one unit required):
//!   size words `exceeds` / `too large` / `oversized` / `too big` /
//!   `larger than`, units `MiB` / `KiB` / `bytes`.
//! - File-role words (case-insensitive; one required): `manifest` /
//!   `provenance` / `artifact` / `plan`.
//! - Controls: the existing provenance-hash refusal semantics are preserved
//!   byte for byte — a wrong-run manifest, a hash-less manifest and a missing
//!   file refuse exactly as today. Any weakening or rewording of those
//!   refusals fails the controls.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use two_bot_cutover::staging_migrate::{
    check_plan_provenance, manifest_hash, reconcile, Chain, Request, RunError, Target,
    RUNNER_VERSION,
};

/// The manifest byte cap the provenance read must enforce: 8 MiB. Boundary:
/// exactly this many bytes is admitted, one byte more is refused before any
/// parse.
const INTAKE_CAP_BYTES: usize = 8 * 1024 * 1024;

static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

/// Unique scratch file under the run-owned scratch dir (shared-controller
/// convention), falling back to the process temp dir.
fn scratch_file(tag: &str) -> PathBuf {
    let base = std::env::var_os("PAPERCLIP_RUN_SCRATCH_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("PAPERCLIP_SCRATCH_DIR").map(PathBuf::from))
        .unwrap_or_else(std::env::temp_dir);
    let _ = std::fs::create_dir_all(&base);
    let id = NEXT_FILE.fetch_add(1, Ordering::SeqCst);
    base.join(format!("manifest-cap-{}-{id}-{tag}", std::process::id()))
}

fn write_scratch(tag: &str, contents: &[u8]) -> PathBuf {
    let path = scratch_file(tag);
    std::fs::write(&path, contents).expect("scratch file must be writable");
    path
}

/// Pad manifest JSON with trailing spaces to exactly `INTAKE_CAP_BYTES + extra`
/// bytes. Trailing whitespace never changes what a JSON parser accepts, so
/// the padded manifest is valid wherever the compact one is.
fn sized_manifest(base: &str, extra: usize) -> String {
    assert!(
        base.len() < INTAKE_CAP_BYTES,
        "fixture base must fit under the cap"
    );
    let mut out = String::with_capacity(INTAKE_CAP_BYTES + extra);
    out.push_str(base);
    out.push_str(&" ".repeat(INTAKE_CAP_BYTES + extra - base.len()));
    assert_eq!(out.len(), INTAKE_CAP_BYTES + extra);
    out
}

/// True when `message` carries a fixed-vocabulary size refusal: one size word
/// plus one unit, case-insensitive.
fn is_size_refusal(message: &str) -> bool {
    let lower = message.to_lowercase();
    let size_word = ["exceed", "too large", "oversiz", "too big", "larger than"]
        .into_iter()
        .any(|w| lower.contains(w));
    let unit = ["mib", "kib", "bytes", "byte"]
        .into_iter()
        .any(|u| lower.contains(u));
    size_word && unit
}

/// True when `message` names one of the manifest role words.
fn names_role(message: &str, roles: &[&str]) -> bool {
    let lower = message.to_lowercase();
    roles.iter().any(|r| lower.contains(&r.to_lowercase()))
}

/// A producing-run manifest artifact carrying `hash` and `store_hash` in its
/// embedded hash fields, as the plan job would upload it.
fn producing_manifest(hash: &str, store_hash: &str) -> String {
    serde_json::json!({
        "runner_version": RUNNER_VERSION,
        "mode": "plan",
        "source_sha": "a".repeat(40),
        "plan_manifest_sha256": hash,
        "store_plan_manifest_sha256": store_hash,
        "plan_run_id": "",
    })
    .to_string()
}

/// An apply request naming `manifest_path` as the producing run's downloaded
/// manifest, plus the two computed hashes the provenance check compares
/// against. Mirrors the in-module bound-apply fixture through public API only.
fn provenance_request(manifest_path: &Path) -> (Request, String, String) {
    let sha = "a".repeat(40);
    let cutover = Chain::Cutover.migrator().expect("cutover chain builds");
    let pending = reconcile(&[], &cutover).expect("empty ledger reconciles");
    let hash = manifest_hash(&sha, &pending, &cutover);
    let store = Chain::Store.migrator().expect("store chain builds");
    let store_pending = reconcile(&[], &store).expect("empty store ledger reconciles");
    let store_hash = manifest_hash(&sha, &store_pending, &store);
    let request = Request {
        url: None,
        target: Target::Staging,
        source_sha: sha,
        expected_host: "agent-testdb".to_owned(),
        expected_database: "two_staging".to_owned(),
        expected_branch_id: String::new(),
        recovery_evidence_ref: "acceptance".to_owned(),
        acl_plan_ref: "acceptance".to_owned(),
        apply: true,
        expected_pending: None,
        expected_store_pending: None,
        plan_manifest_sha256: None,
        store_plan_manifest_sha256: None,
        plan_run_id: Some("123456789".to_owned()),
        plan_manifest_path: Some(manifest_path.to_string_lossy().into_owned()),
    };
    (request, hash, store_hash)
}

#[test]
fn provenance_refuses_oversized_manifest_before_parsing() {
    // Oversized but otherwise valid: correct hashes, so only the missing size
    // enforcement can be blamed pre-fix.
    let (request, hash, store_hash) = provenance_request(Path::new("placeholder"));
    let path = write_scratch(
        "oversized",
        sized_manifest(&producing_manifest(&hash, &store_hash), 1).as_bytes(),
    );
    let request = Request {
        plan_manifest_path: Some(path.to_string_lossy().into_owned()),
        ..request
    };
    let result = check_plan_provenance(&request, &hash, &store_hash);
    let _ = std::fs::remove_file(&path);
    let err = result.expect_err("oversized manifest must be refused before parsing");
    let RunError::Refused(message) = err else {
        panic!("oversized manifest must refuse, not fail");
    };
    assert!(
        is_size_refusal(&message),
        "refusal must name the size cap, got: {message}"
    );
    assert!(
        names_role(&message, &["manifest", "provenance", "artifact", "plan"]),
        "refusal must name the file role, got: {message}"
    );
}

#[test]
fn provenance_accepts_at_cap_manifest() {
    let (request, hash, store_hash) = provenance_request(Path::new("placeholder"));
    let path = write_scratch(
        "at-cap",
        sized_manifest(&producing_manifest(&hash, &store_hash), 0).as_bytes(),
    );
    let request = Request {
        plan_manifest_path: Some(path.to_string_lossy().into_owned()),
        ..request
    };
    let result = check_plan_provenance(&request, &hash, &store_hash);
    let _ = std::fs::remove_file(&path);
    assert!(
        result.is_ok(),
        "at-cap manifest must verify: {}",
        result.unwrap_err()
    );
}

#[test]
fn provenance_accepts_small_valid_manifest() {
    let (request, hash, store_hash) = provenance_request(Path::new("placeholder"));
    let path = write_scratch(
        "small-valid",
        producing_manifest(&hash, &store_hash).as_bytes(),
    );
    let request = Request {
        plan_manifest_path: Some(path.to_string_lossy().into_owned()),
        ..request
    };
    let result = check_plan_provenance(&request, &hash, &store_hash);
    let _ = std::fs::remove_file(&path);
    assert!(
        result.is_ok(),
        "small valid manifest must verify: {}",
        result.unwrap_err()
    );
}

#[test]
fn provenance_still_refuses_wrong_run_manifest() {
    // The manifest embeds hashes for another source SHA: provenance must
    // refuse exactly as today.
    let other = "b".repeat(40);
    let cutover = Chain::Cutover.migrator().expect("cutover chain builds");
    let pending = reconcile(&[], &cutover).expect("empty ledger reconciles");
    let other_hash = manifest_hash(&other, &pending, &cutover);
    let store = Chain::Store.migrator().expect("store chain builds");
    let store_pending = reconcile(&[], &store).expect("empty store ledger reconciles");
    let other_store_hash = manifest_hash(&other, &store_pending, &store);
    let (request, hash, store_hash) = provenance_request(Path::new("placeholder"));
    let path = write_scratch(
        "wrong-run",
        producing_manifest(&other_hash, &other_store_hash).as_bytes(),
    );
    let request = Request {
        plan_manifest_path: Some(path.to_string_lossy().into_owned()),
        ..request
    };
    let result = check_plan_provenance(&request, &hash, &store_hash);
    let _ = std::fs::remove_file(&path);
    let Err(RunError::Refused(message)) = result else {
        panic!("wrong-run manifest must refuse");
    };
    assert!(
        message.contains("plan_manifest_sha256"),
        "wrong-run refusal keeps its semantics, got: {message}"
    );
}

#[test]
fn provenance_still_refuses_missing_manifest_file() {
    let missing = scratch_file("absent");
    let _ = std::fs::remove_file(&missing);
    let (request, hash, store_hash) = provenance_request(&missing);
    let Err(RunError::Refused(message)) = check_plan_provenance(&request, &hash, &store_hash)
    else {
        panic!("missing manifest must refuse");
    };
    assert!(
        message.contains("cannot read"),
        "missing-file refusal keeps its semantics, got: {message}"
    );
}
