//! Acceptance: backup/upload/snapshot failures emit stable `code=` tokens.
//!
//! BUILDER CONTRACT: this file is frozen. Build on the test card's branch,
//! keep every test here verbatim, and make the suite green by changing
//! production code only — add a stable `code=<token>` to every failure line
//! in `cmd_backup`, `cmd_backup_upload`, `cmd_guild_config_snapshot` and
//! `upload_failure_message` in `crates/bot/src/backup_cli.rs`. Do not edit
//! this file or the `#[cfg(test)] mod backup_stable_error_codes_acceptance_tests;`
//! wiring in `main.rs`.
//!
//! - Every `eprintln!`/`format!` failure site listed below must contain a
//!   `code=` token on the same invocation: `code=[a-z0-9_]{1,32}`.
//!   The token vocabulary is yours to choose, but each failure class needs a
//!   stable code an agent can match without substring-matching English.
//!   At least 12 distinct codes across the file (there are 30 sites;
//!   grouping is allowed, a single catch-all is not).
//! - Failure output keeps its human-readable text; the code is additive.
//!   Secrets, URLs, argv and child output stay redacted as today.
//! - Success lines stay byte-identical (see GREEN controls).
//! - No new env vars, dependencies or behavior beyond the additive token.
//!
//! Suite map (all offline: source pins + pure helper calls, no DB, no network,
//! no process env mutation):
//!
//! - backup failure pins (RED now: free text, no `code=`): database URL,
//!   retention refusal, dest create, pool open, stat, empty event log,
//!   dump failed, prune failed.
//! - upload failure pins (RED now): usage, not-regular-file, empty file,
//!   S3 target, read failed, PUT status, PUT failed.
//! - snapshot failure pins (RED now): token load, token check, guild id,
//!   governed API, identity, capture, atomic write, re-read, hash changed,
//!   seal missing, local-only refusal.
//! - upload-helper pins (RED now): exit-nonzero, spawn failed, wait failed,
//!   timed out.
//! - code vocabulary pins (RED now: zero codes): token shape + distinctness.
//! - GREEN controls (pass before and after): retention parse/prune helpers,
//!   S3 object-key helper, and success markers byte-identical.

use std::collections::BTreeSet;

use two_bot_core::backup::{retention, s3};

/// The backup CLI source as committed, read at compile time so the suite
/// stays offline.
const BACKUP_CLI_RS: &str = include_str!("backup_cli.rs");

/// Extract the source segment from `start_marker` (inclusive) to `end_marker`
/// (exclusive). Panics if either marker is missing, so a refactor that moves
/// a command fails loudly instead of silently shrinking coverage.
fn segment<'a>(source: &'a str, start_marker: &str, end_marker: &str) -> &'a str {
    let start = source
        .find(start_marker)
        .unwrap_or_else(|| panic!("start marker {start_marker:?} not found in backup_cli.rs"));
    let after_start = &source[start..];
    let end_rel = after_start
        .find(end_marker)
        .unwrap_or_else(|| panic!("end marker {end_marker:?} not found after {start_marker:?}"));
    &source[start..start + end_rel]
}

fn cmd_backup_src() -> &'static str {
    // `prune_backups` is the first item after `cmd_backup`.
    segment(BACKUP_CLI_RS, "async fn cmd_backup()", "fn prune_backups(")
}

fn cmd_backup_upload_src() -> &'static str {
    segment(
        BACKUP_CLI_RS,
        "async fn cmd_backup_upload(",
        "const UPLOAD_COMMAND_TIMEOUT",
    )
}

fn cmd_guild_config_snapshot_src() -> &'static str {
    segment(
        BACKUP_CLI_RS,
        "async fn cmd_guild_config_snapshot()",
        "async fn cmd_guild_config_restore(",
    )
}

fn upload_failure_message_src() -> &'static str {
    segment(
        BACKUP_CLI_RS,
        "fn upload_failure_message(",
        "async fn run_upload_command(",
    )
}

/// Closing delimiter for an invocation: `eprintln!(...);` ends with `");"`
/// while `Some(format!(...))` ends with `"))"`.
fn closer_for(start_token: &str) -> &'static str {
    if start_token == "format!" {
        "))"
    } else {
        ");"
    }
}

/// The `eprintln!`/`format!` invocation enclosing `marker`. `marker` must be
/// unique within `segment`; for repeated format strings use
/// [`next_invocation_after`] with a code anchor instead.
fn invocation_enclosing(segment: &str, marker: &str, start_token: &str) -> String {
    let pos = segment
        .find(marker)
        .unwrap_or_else(|| panic!("marker {marker:?} not found in segment:\n{segment}"));
    let before = &segment[..pos];
    let start = before
        .rfind(start_token)
        .unwrap_or_else(|| panic!("no {start_token} before marker {marker:?}"));
    let closer = closer_for(start_token);
    let after = &segment[pos..];
    let end_rel = after
        .find(closer)
        .unwrap_or_else(|| panic!("no closing `{closer}` after marker {marker:?}"));
    segment[start..pos + end_rel + closer.len()].to_owned()
}

/// The first `start_token` invocation after a unique code `anchor` (for
/// failure sites that share a generic format string).
fn next_invocation_after(segment: &str, anchor: &str, start_token: &str) -> String {
    let anchor_pos = segment
        .find(anchor)
        .unwrap_or_else(|| panic!("anchor {anchor:?} not found in segment:\n{segment}"));
    let after_anchor = &segment[anchor_pos + anchor.len()..];
    let start_rel = after_anchor
        .find(start_token)
        .unwrap_or_else(|| panic!("no {start_token} after anchor {anchor:?}"));
    let start = anchor_pos + anchor.len() + start_rel;
    let closer = closer_for(start_token);
    let after_start = &segment[start..];
    let end_rel = after_start
        .find(closer)
        .unwrap_or_else(|| panic!("no closing `{closer}` after anchor {anchor:?}"));
    segment[start..start + end_rel + closer.len()].to_owned()
}

fn assert_has_code(invocation: &str, what: &str) {
    assert!(
        invocation.contains("code="),
        "{what} must emit a stable `code=`: {invocation:?}"
    );
}

fn is_code_token(value: &str) -> bool {
    (1..=32).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Every `code=` value in the backup CLI source.
fn all_codes() -> BTreeSet<String> {
    let mut codes = BTreeSet::new();
    let mut rest = BACKUP_CLI_RS;
    while let Some(pos) = rest.find("code=") {
        let after = &rest[pos + "code=".len()..];
        let end = after
            .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'))
            .unwrap_or(after.len());
        codes.insert(after[..end].to_owned());
        rest = after;
    }
    codes
}

// ---------------------------------------------------------------------------
// backup failure pins (RED now)
// ---------------------------------------------------------------------------

#[test]
fn backup_missing_database_url_emits_code() {
    let invocation = invocation_enclosing(
        cmd_backup_src(),
        "TWO_DATABASE_URL must be set",
        "eprintln!",
    );
    assert_has_code(&invocation, "backup missing-DATABASE_URL refusal");
}

#[test]
fn backup_retention_refusal_emits_code() {
    // The retention failure spans two lines; the second is the stable,
    // human-greppable refusal. The code may sit on either line.
    let segment = cmd_backup_src();
    let first = invocation_enclosing(segment, "Refusing to run: every reading", "eprintln!");
    assert_has_code(&first, "backup retention refusal");
}

#[test]
fn backup_dest_create_failure_emits_code() {
    let invocation = invocation_enclosing(cmd_backup_src(), "cannot create {dest}", "eprintln!");
    assert_has_code(&invocation, "backup dest-create failure");
}

#[test]
fn backup_pool_open_failure_emits_code() {
    // `eprintln!("backup: {detail}")` is unique within `cmd_backup`.
    let invocation = invocation_enclosing(cmd_backup_src(), "backup: {detail}", "eprintln!");
    assert_has_code(&invocation, "backup pool-open failure");
}

#[test]
fn backup_stat_failure_emits_code() {
    let invocation = invocation_enclosing(cmd_backup_src(), "but cannot stat it", "eprintln!");
    assert_has_code(&invocation, "backup stat failure");
}

#[test]
fn backup_empty_event_log_refusal_emits_code() {
    let invocation = invocation_enclosing(cmd_backup_src(), "the event log is empty", "eprintln!");
    assert_has_code(&invocation, "backup empty-event-log refusal");
}

#[test]
fn backup_dump_failure_emits_code() {
    let invocation = invocation_enclosing(cmd_backup_src(), "dump failed", "eprintln!");
    assert_has_code(&invocation, "backup dump failure");
}

#[test]
fn backup_prune_failure_emits_code() {
    let invocation = invocation_enclosing(cmd_backup_src(), "prune failed", "eprintln!");
    assert_has_code(&invocation, "backup prune failure");
}

// ---------------------------------------------------------------------------
// upload failure pins (RED now)
// ---------------------------------------------------------------------------

#[test]
fn upload_usage_refusal_emits_code() {
    let invocation = invocation_enclosing(
        cmd_backup_upload_src(),
        "expected exactly one argument",
        "eprintln!",
    );
    assert_has_code(&invocation, "backup-upload usage refusal");
}

#[test]
fn upload_not_regular_file_emits_code() {
    let invocation =
        invocation_enclosing(cmd_backup_upload_src(), "not a regular file", "eprintln!");
    assert_has_code(&invocation, "backup-upload not-regular-file refusal");
}

#[test]
fn upload_empty_file_refusal_emits_code() {
    let invocation = invocation_enclosing(
        cmd_backup_upload_src(),
        "refusing to upload an empty file",
        "eprintln!",
    );
    assert_has_code(&invocation, "backup-upload empty-file refusal");
}

#[test]
fn upload_s3_target_failure_emits_code() {
    // `eprintln!("backup-upload-s3: {err}")` is unique within the upload command.
    let invocation = invocation_enclosing(
        cmd_backup_upload_src(),
        "backup-upload-s3: {err}",
        "eprintln!",
    );
    assert_has_code(&invocation, "backup-upload S3-target failure");
}

#[test]
fn upload_read_failure_emits_code() {
    let invocation = next_invocation_after(
        cmd_backup_upload_src(),
        "std::fs::read(dump_path)",
        "eprintln!",
    );
    assert!(
        invocation.contains("cannot read"),
        "read-failure site must still name the read: {invocation:?}"
    );
    assert_has_code(&invocation, "backup-upload read failure");
}

#[test]
fn upload_put_status_failure_emits_code() {
    let invocation =
        next_invocation_after(cmd_backup_upload_src(), "(200..300).contains", "eprintln!");
    assert!(
        invocation.contains("PUT"),
        "PUT-status site must still name PUT: {invocation:?}"
    );
    assert_has_code(&invocation, "backup-upload PUT-status failure");
}

#[test]
fn upload_put_failed_emits_code() {
    let invocation = invocation_enclosing(cmd_backup_upload_src(), "PUT failed", "eprintln!");
    assert_has_code(&invocation, "backup-upload PUT failure");
}

// ---------------------------------------------------------------------------
// snapshot failure pins (RED now)
// ---------------------------------------------------------------------------

#[test]
fn snapshot_token_load_failure_emits_code() {
    let invocation = next_invocation_after(
        cmd_guild_config_snapshot_src(),
        "staging_token()",
        "eprintln!",
    );
    assert_has_code(&invocation, "snapshot token-load failure");
}

#[test]
fn snapshot_token_check_refusal_emits_code() {
    let invocation = next_invocation_after(
        cmd_guild_config_snapshot_src(),
        "check_staging_token(&token)",
        "eprintln!",
    );
    assert_has_code(&invocation, "snapshot token-check refusal");
}

#[test]
fn snapshot_guild_id_failure_emits_code() {
    let invocation = next_invocation_after(
        cmd_guild_config_snapshot_src(),
        "staging_guild_id(&std_env)",
        "eprintln!",
    );
    assert_has_code(&invocation, "snapshot guild-id failure");
}

#[test]
fn snapshot_governed_api_failure_emits_code() {
    let invocation = next_invocation_after(
        cmd_guild_config_snapshot_src(),
        "governed_guild_config_api(token",
        "eprintln!",
    );
    assert_has_code(&invocation, "snapshot governed-API failure");
}

#[test]
fn snapshot_identity_failure_emits_code() {
    let invocation = next_invocation_after(
        cmd_guild_config_snapshot_src(),
        "assert_identity().await",
        "eprintln!",
    );
    assert_has_code(&invocation, "snapshot identity failure");
}

#[test]
fn snapshot_capture_failure_emits_code() {
    let invocation = invocation_enclosing(
        cmd_guild_config_snapshot_src(),
        "capture failed",
        "eprintln!",
    );
    assert_has_code(&invocation, "snapshot capture failure");
}

#[test]
fn snapshot_atomic_write_failure_emits_code() {
    let invocation = next_invocation_after(
        cmd_guild_config_snapshot_src(),
        "atomic_json(path, &value)",
        "eprintln!",
    );
    assert_has_code(&invocation, "snapshot atomic-write failure");
}

#[test]
fn snapshot_reread_failure_emits_code() {
    let invocation = invocation_enclosing(
        cmd_guild_config_snapshot_src(),
        "cannot re-read",
        "eprintln!",
    );
    assert_has_code(&invocation, "snapshot re-read failure");
}

#[test]
fn snapshot_hash_changed_failure_emits_code() {
    let invocation = invocation_enclosing(
        cmd_guild_config_snapshot_src(),
        "snapshot hash changed after write",
        "eprintln!",
    );
    assert_has_code(&invocation, "snapshot hash-changed failure");
}

#[test]
fn snapshot_seal_missing_failure_emits_code() {
    let invocation = invocation_enclosing(
        cmd_guild_config_snapshot_src(),
        "missing its integrity seal",
        "eprintln!",
    );
    assert_has_code(&invocation, "snapshot seal-missing failure");
}

#[test]
fn snapshot_local_only_refusal_emits_code() {
    let invocation = invocation_enclosing(
        cmd_guild_config_snapshot_src(),
        "refusing a local-only snapshot",
        "eprintln!",
    );
    assert_has_code(&invocation, "snapshot local-only refusal");
}

// ---------------------------------------------------------------------------
// upload-helper pins (RED now)
// ---------------------------------------------------------------------------

#[test]
fn upload_helper_exit_nonzero_emits_code() {
    let invocation = invocation_enclosing(
        upload_failure_message_src(),
        "upload failed: {cmd} exited",
        "format!",
    );
    assert_has_code(&invocation, "upload-helper exit-nonzero");
}

#[test]
fn upload_helper_spawn_failed_emits_code() {
    let invocation = invocation_enclosing(
        upload_failure_message_src(),
        "upload failed to start",
        "format!",
    );
    assert_has_code(&invocation, "upload-helper spawn failure");
}

#[test]
fn upload_helper_wait_failed_emits_code() {
    let invocation = invocation_enclosing(
        upload_failure_message_src(),
        "upload wait failed",
        "format!",
    );
    assert_has_code(&invocation, "upload-helper wait failure");
}

#[test]
fn upload_helper_timed_out_emits_code() {
    let invocation = invocation_enclosing(
        upload_failure_message_src(),
        "upload timed out after",
        "format!",
    );
    assert_has_code(&invocation, "upload-helper timeout");
}

// ---------------------------------------------------------------------------
// code vocabulary pins (RED now)
// ---------------------------------------------------------------------------

#[test]
fn codes_are_lower_snake_tokens() {
    let codes = all_codes();
    assert!(
        !codes.is_empty(),
        "backup_cli.rs must emit at least one `code=`; found none"
    );
    for code in &codes {
        assert!(
            is_code_token(code),
            "code value must be [a-z0-9_]{{1,32}}, got {code:?}"
        );
    }
}

#[test]
fn codes_distinguish_failure_classes() {
    // Thirty failure sites; grouping is allowed, but a single catch-all
    // `code=error` does not let an agent match failure classes.
    let codes = all_codes();
    assert!(
        codes.len() >= 12,
        "expected at least 12 distinct codes, found {}: {codes:?}",
        codes.len()
    );
}

// ---------------------------------------------------------------------------
// GREEN controls: valid paths byte-identical (pass before and after)
// ---------------------------------------------------------------------------

#[test]
fn control_retention_parse_defaults_and_valid_values() {
    assert_eq!(retention::parse_keep(None), Ok(retention::DEFAULT_KEEP));
    assert_eq!(retention::parse_keep(Some("")), Ok(retention::DEFAULT_KEEP));
    assert_eq!(
        retention::parse_keep(Some("  ")),
        Ok(retention::DEFAULT_KEEP)
    );
    assert_eq!(retention::parse_keep(Some("3")), Ok(3));
    assert_eq!(retention::parse_keep(Some("14")), Ok(14));
}

#[test]
fn control_retention_prune_arithmetic() {
    let newest_first = ["c", "b", "a"];
    assert!(retention::to_prune::<&str>(&[], 1).is_empty());
    assert!(retention::to_prune(&newest_first, 3).is_empty());
    assert!(retention::to_prune(&newest_first, 5).is_empty());
    assert_eq!(retention::to_prune(&newest_first, 1), &["b", "a"][..]);
    assert_eq!(retention::to_prune(&newest_first, 2), &["a"][..]);
}

#[test]
fn control_s3_object_key_passthrough() {
    assert_eq!(
        s3::object_key(None, "dump.ndjson.gz"),
        "dump.ndjson.gz".to_owned()
    );
    assert_eq!(
        s3::object_key(Some("prefix"), "dump.ndjson.gz"),
        "prefix/dump.ndjson.gz".to_owned()
    );
    assert_eq!(s3::object_key(Some("/prefix/"), "f"), "prefix/f".to_owned());
}

#[test]
fn control_success_markers_byte_identical() {
    for marker in [
        "backup: done",
        "RESTORE VERIFIED",
        "DRY RUN VERIFIED",
        "guild-config-snapshot: stored",
        "backup-upload-s3: stored",
        "snapshot and drift report uploaded",
    ] {
        assert!(
            BACKUP_CLI_RS.contains(marker),
            "success marker must survive the code change: {marker:?}"
        );
    }
}

#[test]
fn control_missing_upload_cmd_stays_a_warning_not_a_failure() {
    // The local-disk warning is the one backup `eprintln!` that does not end
    // the run; it must not gain a failure code or an early return.
    let segment = cmd_backup_src();
    let pos = segment
        .find("TWO_BACKUP_UPLOAD_CMD is not set")
        .expect("local-disk warning must still exist");
    let after = &segment[pos..];
    let end = after
        .find("backup: done")
        .expect("warning must precede the success line");
    let window = &after[..end];
    assert!(
        !window.contains("return 1") && !window.contains("return 2"),
        "local-disk warning must not become a failure: {window:?}"
    );
}
