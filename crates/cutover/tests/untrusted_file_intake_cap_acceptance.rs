//! Untrusted file intake caps: every cutover import CLI refuses an over-cap
//! file before any parse, database or network touch (failing first, tests only).
//!
//! Base: `164343832156c3172dbbcfddd53892353cae23a5` (main at acceptance
//! authoring). At that base the CLI read sites below buffer their whole input
//! with `std::fs::read` / `read_to_string` and only then discover the file is
//! hostile: an over-cap file sails through the missing cap and either dies at
//! a downstream fence (database URL, Discord token, TLS policy) or is parsed
//! and reported as if fine. The over-cap tests are RED on that base for
//! exactly that reason, while the controls stay green. The paired builder
//! implements production code only and must NOT edit this file; the tests pass
//! unmodified once every site enforces the contract below. No failing test
//! merges: this file is handed off unpublished for the builder's final green
//! implementation pull request. (The staging-manifest read site lives in the
//! library, not a CLI, and is covered by
//! `staging_manifest_cap_acceptance.rs`; MEE6 row bounds by
//! `mee6_intake_bounds_acceptance.rs`.)
//!
//! ## Builder contract
//!
//! - One byte cap everywhere: `INTAKE_CAP_BYTES` (8 MiB, matching the capped
//!   operator-tool siblings). Exactly the cap is admitted; one byte more is
//!   refused. A site that justifies a smaller cap routes the correction
//!   through this file's author; the builder never edits this file.
//! - The refusal fires before any parse, database or network touch: with no
//!   credentials configured, an over-cap run names the cap, never a downstream
//!   fence.
//! - Size vocabulary (case-insensitive; one size word plus one unit required):
//!   size words `exceeds` / `too large` / `oversized` / `too big` /
//!   `larger than`, units `MiB` / `KiB` / `bytes`.
//! - File-role words per site (case-insensitive; one required): MEE6 XP import
//!   `export` / `mee6` / `player`; rewards probe export `export` / `mee6` /
//!   `reward`; rewards probe roles `role`; voice config `voice` / `config` /
//!   `v11`; rollback import `import` / `delta` / `ndjson` / `export`; legacy
//!   mapping `mapping` / `spec`.
//! - A refused run exits non-zero and prints no report to stdout.
//! - The NDJSON row path additionally refuses a single line that alone exceeds
//!   any sane line bound: the 2 MiB single-line fixture (2 MiB total, under
//!   the file cap, so only a line cap can refuse it) must be refused with a
//!   bound word naming the line.
//! - Controls: at-cap files sail through the read to the downstream fence (or
//!   succeed outright where the path is offline), small valid files keep
//!   parsing, and the voice library's existing 256 KiB decode cap keeps
//!   refusing. Any over-blocking fails the controls.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use two_bot_core::voice_config::{decode_configuration, GuildInventory, MAX_IMPORT_BYTES};
use two_bot_cutover::legacy_mapping::MappingSpec;
use two_bot_cutover::rollback_import::parse_import;
use two_bot_cutover::voice_config_apply::{plan_apply, ApplyPlan};

/// The intake byte cap every read site must enforce: 8 MiB, matching the
/// capped operator-tool siblings. Boundary: exactly this many bytes is
/// admitted, one byte more is refused before any parse or I/O.
const INTAKE_CAP_BYTES: usize = 8 * 1024 * 1024;

/// Reserved-block guild that is never the live guild, so no test needs the
/// live-guild opt-in.
const GUILD: &str = "900000000000000001";

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
    base.join(format!("intake-cap-{}-{id}-{tag}", std::process::id()))
}

fn write_scratch(tag: &str, contents: &[u8]) -> PathBuf {
    let path = scratch_file(tag);
    std::fs::write(&path, contents).expect("scratch file must be writable");
    path
}

/// Pad JSON text with trailing spaces to exactly `INTAKE_CAP_BYTES + extra`
/// bytes. Trailing whitespace never changes what a JSON parser accepts, so
/// the padded file is valid wherever the compact file is.
fn sized_json(base: &str, extra: usize) -> String {
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

/// True when `stderr` carries a fixed-vocabulary size refusal: one size word
/// plus one unit, case-insensitive. This is the contract's size half.
fn is_size_refusal(stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    let size_word = ["exceed", "too large", "oversiz", "too big", "larger than"]
        .into_iter()
        .any(|w| lower.contains(w));
    let unit = ["mib", "kib", "bytes", "byte"]
        .into_iter()
        .any(|u| lower.contains(u));
    size_word && unit
}

/// True when `stderr` carries a bound refusal (row, line or player bound):
/// bound words only, no byte unit required. This is the contract's bound half.
fn is_bound_refusal(message: &str) -> bool {
    let lower = message.to_lowercase();
    [
        "exceed",
        "too many",
        "too large",
        "oversiz",
        "too big",
        "larger than",
        "limit",
        "maximum",
        "capped",
        "cap ",
        "cap:",
        "(cap",
        "bound",
        "truncat",
        "more than",
    ]
    .into_iter()
    .any(|w| lower.contains(w))
}

/// True when `stderr` names one of the file-role words for the site under test.
fn names_role(stderr: &str, roles: &[&str]) -> bool {
    let lower = stderr.to_lowercase();
    roles.iter().any(|r| lower.contains(&r.to_lowercase()))
}

/// True when `stderr` shows the run reached past the read: a database, Discord
/// or TLS fence fired, which proves no intake cap refused the file.
fn reached_past_read(stderr: &str) -> bool {
    [
        "TWO_DATABASE_URL",
        "cannot open database",
        "DISCORD_TOKEN",
        "DISCORD_BOT_TOKEN",
        "guild read refused",
        "channel read refused",
        "member read refused",
        "configuration snapshot failed",
        "refused under the required TLS policy",
        "connection failed",
    ]
    .into_iter()
    .any(|m| stderr.contains(m))
}

/// Run a CLI under test with a fully cleared environment: no database URL, no
/// Discord token, no inherited connection settings. A capped read refuses
/// before noticing anything is missing; an uncapped read falls through to a
/// downstream fence.
fn run(bin: &str, args: &[&str]) -> Output {
    Command::new(bin)
        .args(args)
        .env_clear()
        .output()
        .expect("CLI under test must spawn")
}

const MEE6_EXPORT: &str = r#"[{"id":"100000000000000001","xp":1}]"#;

#[test]
fn mee6_import_refuses_over_cap_export_before_any_database_touch() {
    let path = write_scratch("mee6-over", sized_json(MEE6_EXPORT, 1).as_bytes());
    let output = run(
        env!("CARGO_BIN_EXE_levels-import-mee6"),
        &["--guild", GUILD, "--file", &path.to_string_lossy()],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let _ = std::fs::remove_file(&path);
    assert!(
        !output.status.success(),
        "over-cap export must be refused: {stderr}"
    );
    assert!(
        is_size_refusal(&stderr),
        "refusal must name the size cap, got: {stderr}"
    );
    assert!(
        names_role(&stderr, &["export", "mee6", "player"]),
        "refusal must name the file role, got: {stderr}"
    );
    assert!(
        !reached_past_read(&stderr),
        "cap must fire before any database touch, got: {stderr}"
    );
    assert!(output.stdout.is_empty(), "refusal prints no report");
}

#[test]
fn mee6_import_admits_at_cap_export_to_the_database_fence() {
    let path = write_scratch("mee6-at-cap", sized_json(MEE6_EXPORT, 0).as_bytes());
    let output = run(
        env!("CARGO_BIN_EXE_levels-import-mee6"),
        &["--guild", GUILD, "--file", &path.to_string_lossy()],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let _ = std::fs::remove_file(&path);
    assert!(
        !output.status.success(),
        "no database is configured, so the fence must fire: {stderr}"
    );
    assert!(
        !is_size_refusal(&stderr),
        "at-cap file must not trip the cap: {stderr}"
    );
    assert!(
        reached_past_read(&stderr),
        "at-cap file must sail through the read to the downstream fence: {stderr}"
    );
}

const PROBE_EXPORT: &str = r#"{"role_rewards":[{"rank":5,"role_id":"900000000000000099"}]}"#;
const PROBE_ROLES: &str = r#"[{"id":"900000000000000020","name":"Level 5","position":10}]"#;

fn probe_bin() -> &'static str {
    env!("CARGO_BIN_EXE_levels-import-rewards-probe")
}

#[test]
fn rewards_probe_refuses_over_cap_export_without_parsing_it() {
    let export = write_scratch("probe-export-over", sized_json(PROBE_EXPORT, 1).as_bytes());
    let roles = write_scratch("probe-export-over-roles", PROBE_ROLES.as_bytes());
    let output = run(
        probe_bin(),
        &[
            "--guild",
            GUILD,
            "--file",
            &export.to_string_lossy(),
            "--roles",
            &roles.to_string_lossy(),
            "--bot-id",
            GUILD,
            "--no-db",
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let _ = std::fs::remove_file(&export);
    let _ = std::fs::remove_file(&roles);
    assert!(
        !output.status.success(),
        "over-cap export must be refused, got exit 0 with: {stdout}"
    );
    assert!(
        is_size_refusal(&stderr),
        "refusal must name the size cap, got: {stderr}"
    );
    assert!(
        names_role(&stderr, &["export", "mee6", "reward"]),
        "refusal must name the file role, got: {stderr}"
    );
    assert!(output.stdout.is_empty(), "refusal prints no report");
}

#[test]
fn rewards_probe_refuses_over_cap_roles_without_parsing_them() {
    let export = write_scratch("probe-roles-over-export", PROBE_EXPORT.as_bytes());
    let roles = write_scratch("probe-roles-over", sized_json(PROBE_ROLES, 1).as_bytes());
    let output = run(
        probe_bin(),
        &[
            "--guild",
            GUILD,
            "--file",
            &export.to_string_lossy(),
            "--roles",
            &roles.to_string_lossy(),
            "--bot-id",
            GUILD,
            "--no-db",
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let _ = std::fs::remove_file(&export);
    let _ = std::fs::remove_file(&roles);
    assert!(
        !output.status.success(),
        "over-cap roles must be refused, got exit 0 with: {stdout}"
    );
    assert!(
        is_size_refusal(&stderr),
        "refusal must name the size cap, got: {stderr}"
    );
    assert!(
        names_role(&stderr, &["role"]),
        "refusal must name the file role, got: {stderr}"
    );
    assert!(output.stdout.is_empty(), "refusal prints no report");
}

#[test]
fn rewards_probe_reports_at_cap_export_byte_identical() {
    let export = write_scratch(
        "probe-export-at-cap",
        sized_json(PROBE_EXPORT, 0).as_bytes(),
    );
    let roles = write_scratch("probe-export-at-cap-roles", PROBE_ROLES.as_bytes());
    let output = run(
        probe_bin(),
        &[
            "--guild",
            GUILD,
            "--file",
            &export.to_string_lossy(),
            "--roles",
            &roles.to_string_lossy(),
            "--bot-id",
            GUILD,
            "--no-db",
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let _ = std::fs::remove_file(&export);
    let _ = std::fs::remove_file(&roles);
    assert!(
        output.status.success(),
        "at-cap files must report, got: {stderr}"
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("report must be JSON");
    assert_eq!(report["counts"]["rewardsIn"], serde_json::json!(1));
    assert_eq!(report["counts"]["balances"], serde_json::json!(true));
}

#[test]
fn rewards_probe_reports_at_cap_roles_byte_identical() {
    let export = write_scratch("probe-roles-at-cap-export", PROBE_EXPORT.as_bytes());
    let roles = write_scratch("probe-roles-at-cap", sized_json(PROBE_ROLES, 0).as_bytes());
    let output = run(
        probe_bin(),
        &[
            "--guild",
            GUILD,
            "--file",
            &export.to_string_lossy(),
            "--roles",
            &roles.to_string_lossy(),
            "--bot-id",
            GUILD,
            "--no-db",
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let _ = std::fs::remove_file(&export);
    let _ = std::fs::remove_file(&roles);
    assert!(
        output.status.success(),
        "at-cap files must report, got: {stderr}"
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("report must be JSON");
    assert_eq!(report["counts"]["rewardsIn"], serde_json::json!(1));
    assert_eq!(report["counts"]["balances"], serde_json::json!(true));
}

/// Minimal valid V11 voice document (the stored-defaults shape). It is never
/// decoded on the paths under test — the Discord-credential fence fires first
/// — but keeping it valid proves the file sails through the read on its own
/// merits rather than as malformed input.
fn voice_document() -> String {
    serde_json::json!({
        "version": 1,
        "guild_id": GUILD,
        "creators": [],
        "templates": [],
        "aliases": [],
        "lists": [],
        "logging": null,
        "settings": {
            "creation_enabled": true,
            "unique_names": false,
            "no_game_label": "General",
            "force_single_game": false,
            "count_members_without_activity": false,
            "time_zone": "UTC",
            "text_channel_name": "voice-chat",
            "text_viewer_role_id": null,
            "command_role_id": null,
            "command_roles": []
        }
    })
    .to_string()
}

#[test]
fn voice_config_apply_refuses_over_cap_file_before_any_network_touch() {
    let path = write_scratch("voice-over", sized_json(&voice_document(), 1).as_bytes());
    let output = run(
        env!("CARGO_BIN_EXE_voice-config-apply"),
        &["--guild", GUILD, "--file", &path.to_string_lossy()],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let _ = std::fs::remove_file(&path);
    assert!(
        !output.status.success(),
        "over-cap file must be refused: {stderr}"
    );
    assert!(
        is_size_refusal(&stderr),
        "refusal must name the size cap, got: {stderr}"
    );
    assert!(
        names_role(&stderr, &["voice", "config", "v11"]),
        "refusal must name the file role, got: {stderr}"
    );
    assert!(
        !reached_past_read(&stderr),
        "cap must fire before any network touch, got: {stderr}"
    );
    assert!(output.stdout.is_empty(), "refusal prints no report");
}

#[test]
fn voice_config_apply_admits_at_cap_file_to_the_credential_fence() {
    let path = write_scratch("voice-at-cap", sized_json(&voice_document(), 0).as_bytes());
    let output = run(
        env!("CARGO_BIN_EXE_voice-config-apply"),
        &["--guild", GUILD, "--file", &path.to_string_lossy()],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let _ = std::fs::remove_file(&path);
    assert!(
        !output.status.success(),
        "no Discord credential is configured, so the fence must fire: {stderr}"
    );
    assert!(
        !is_size_refusal(&stderr),
        "at-cap file must not trip the cap: {stderr}"
    );
    assert!(
        reached_past_read(&stderr),
        "at-cap file must sail through the read to the downstream fence: {stderr}"
    );
}

/// Over-cap NDJSON that sails through every other check: exactly 50 000
/// `events` rows (the per-table row-cap boundary itself) with bytes past the
/// cap, so only the missing size enforcement can be blamed pre-fix.
fn over_cap_ndjson() -> String {
    let mut out = String::with_capacity(INTAKE_CAP_BYTES + 1024);
    for id in 0..50_000 {
        out.push_str(&format!(
            "{{\"table\":\"events\",\"row\":{{\"id\":{id},\"pad\":\"{}\"}}}}\n",
            "x".repeat(130)
        ));
    }
    assert!(out.len() > INTAKE_CAP_BYTES, "fixture must clear the cap");
    out
}

/// Exactly `INTAKE_CAP_BYTES` of valid NDJSON: 50 000 `events` rows (the
/// row-cap boundary itself) with the padding spread across every row so each
/// line stays short. Total is exactly the cap; every line is far under any
/// sane per-line bound.
fn at_cap_ndjson() -> String {
    const ROWS: usize = 50_000;
    let shells: Vec<String> = (0..ROWS)
        .map(|id| format!("{{\"table\":\"events\",\"row\":{{\"id\":{id},\"pad\":\"\"}}}}"))
        .collect();
    let base: usize = shells.iter().map(|line| line.len() + 1).sum();
    assert!(base < INTAKE_CAP_BYTES, "shells must fit under the cap");
    let extra = INTAKE_CAP_BYTES - base;
    let per_row = extra / ROWS;
    let mut lucky = extra % ROWS;
    assert!(per_row < 1024, "padding stays line-local");
    let mut out = String::with_capacity(INTAKE_CAP_BYTES);
    for line in &shells {
        let mut pad = per_row;
        if lucky > 0 {
            pad += 1;
            lucky -= 1;
        }
        let padded = line.replacen(
            "\"pad\":\"\"",
            &format!("\"pad\":\"{}\"", "x".repeat(pad)),
            1,
        );
        out.push_str(&padded);
        out.push('\n');
    }
    assert_eq!(out.len(), INTAKE_CAP_BYTES);
    out
}

#[test]
fn rollback_import_refuses_over_cap_file_before_parsing_it() {
    let path = write_scratch("rollback-over", over_cap_ndjson().as_bytes());
    let output = run(
        env!("CARGO_BIN_EXE_rollback-delta"),
        &["import", "--file", &path.to_string_lossy()],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let _ = std::fs::remove_file(&path);
    assert!(
        !output.status.success(),
        "over-cap file must be refused, got exit 0 with: {stdout}"
    );
    assert!(
        is_size_refusal(&stderr),
        "refusal must name the size cap, got: {stderr}"
    );
    assert!(
        names_role(&stderr, &["import", "delta", "ndjson", "export"]),
        "refusal must name the file role, got: {stderr}"
    );
    assert!(output.stdout.is_empty(), "refusal prints no plan");
}

#[test]
fn rollback_import_plans_at_cap_file_byte_identical() {
    let path = write_scratch("rollback-at-cap", at_cap_ndjson().as_bytes());
    let output = run(
        env!("CARGO_BIN_EXE_rollback-delta"),
        &["import", "--file", &path.to_string_lossy()],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let _ = std::fs::remove_file(&path);
    assert!(
        output.status.success(),
        "at-cap file must plan, got: {stderr}"
    );
    let plan: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("plan must be JSON");
    assert_eq!(plan["total_rows"], serde_json::json!(50_000));
    assert_eq!(plan["tables"][0]["rows"], serde_json::json!(50_000));
}

const MAPPING: &str = r#"{"version":1,"tables":[{"group":"g","source":"legacy_a","target":"next_a","keys":["id"],"conflict":["member_id"],"columns":[{"source":"id","target":"member_id","pg_type":"bigint"}]}]}"#;

/// Shape-valid but unreachable endpoint: the TLS policy fence fires before any
/// connection attempt, so the test performs no network I/O.
const FENCE_URL: &str = "postgres://operator:explicit@agent-testdb:5432/bot";

#[test]
fn legacy_verify_refuses_over_cap_mapping_before_connecting() {
    let path = write_scratch("mapping-over", sized_json(MAPPING, 1).as_bytes());
    let output = run(
        env!("CARGO_BIN_EXE_legacy_verify"),
        &[
            "--source-url",
            FENCE_URL,
            "--target-url",
            FENCE_URL,
            "--mapping",
            &path.to_string_lossy(),
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let _ = std::fs::remove_file(&path);
    assert!(
        !output.status.success(),
        "over-cap mapping must be refused: {stderr}"
    );
    assert!(
        is_size_refusal(&stderr),
        "refusal must name the size cap, got: {stderr}"
    );
    assert!(
        names_role(&stderr, &["mapping", "spec"]),
        "refusal must name the file role, got: {stderr}"
    );
    assert!(
        !reached_past_read(&stderr),
        "cap must fire before any connection attempt, got: {stderr}"
    );
    assert!(output.stdout.is_empty(), "refusal prints no report");
}

#[test]
fn legacy_verify_admits_at_cap_mapping_to_the_tls_fence() {
    let path = write_scratch("mapping-at-cap", sized_json(MAPPING, 0).as_bytes());
    let output = run(
        env!("CARGO_BIN_EXE_legacy_verify"),
        &[
            "--source-url",
            FENCE_URL,
            "--target-url",
            FENCE_URL,
            "--mapping",
            &path.to_string_lossy(),
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let _ = std::fs::remove_file(&path);
    assert!(
        !output.status.success(),
        "the TLS fence must fire for the fixture host: {stderr}"
    );
    assert!(
        !is_size_refusal(&stderr),
        "at-cap file must not trip the cap: {stderr}"
    );
    assert!(
        reached_past_read(&stderr),
        "at-cap file must sail through the read to the downstream fence: {stderr}"
    );
}

#[test]
fn rollback_import_refuses_an_overlong_single_line() {
    // 2 MiB on one line, 2 MiB total: under the file cap, so only a per-line
    // bound can refuse it.
    let line = format!(
        "{{\"table\":\"events\",\"row\":{{\"pad\":\"{}\"}}}}",
        "x".repeat(2 * 1024 * 1024)
    );
    assert!(
        line.len() > 1024 * 1024,
        "line must clear any sane line bound"
    );
    assert!(
        line.len() < INTAKE_CAP_BYTES,
        "line must stay under the file cap"
    );
    // Map the Ok payload to () FIRST: on RED the parser accepts the 2 MiB
    // row and expect_err would Debug-print all 2 MiB into one log line,
    // which wedges the CI log runner until the job times out.
    let err = parse_import(&line)
        .map(|_| ())
        .expect_err("a 2 MiB single line must be refused");
    let message = err.to_string();
    assert!(
        is_bound_refusal(&message),
        "refusal must name the line bound, got: {message}"
    );
    assert!(
        names_role(&message, &["line", "row", "record", "ndjson"]),
        "refusal must name the line role, got: {message}"
    );
}

#[test]
fn rollback_import_accepts_small_valid_file() {
    let rows = parse_import(
        "{\"table\":\"events\",\"row\":{\"id\":1}}\n{\"table\":\"events\",\"row\":{\"id\":2}}\n",
    )
    .expect("small valid file parses");
    assert_eq!(rows.len(), 2);
}

#[test]
fn mapping_parse_accepts_small_valid_spec() {
    MappingSpec::parse(MAPPING).expect("small valid mapping parses");
}

#[test]
fn mapping_parse_still_refuses_malformed_spec() {
    assert!(MappingSpec::parse("{not json").is_err());
    assert!(
        MappingSpec::parse(r#"{"version":2,"tables":[]}"#).is_err(),
        "wrong version still refuses"
    );
}

#[test]
fn voice_apply_refuses_over_cap_bytes_without_decoding() {
    // The voice library already caps its decode input at 256 KiB; this pins
    // that behavior so the new bin-level read cap cannot silently replace it.
    let current = decode_configuration(voice_document().as_bytes()).expect("valid V11 decodes");
    let inventory = GuildInventory {
        guild_id: GUILD.to_owned(),
        channels: BTreeMap::new(),
        roles: BTreeMap::new(),
        members: BTreeMap::new(),
    };
    let bytes = vec![0u8; MAX_IMPORT_BYTES + 1];
    match plan_apply(&current, &bytes, &inventory) {
        ApplyPlan::Refuse { message } => {
            assert!(
                message.contains("too large"),
                "refusal must name the size, got: {message}"
            );
        }
        plan => panic!("over-cap bytes must refuse, got: {plan:?}"),
    }
}

#[test]
fn voice_apply_fails_closed_on_deeply_nested_document() {
    let current = decode_configuration(voice_document().as_bytes()).expect("valid V11 decodes");
    let inventory = GuildInventory {
        guild_id: GUILD.to_owned(),
        channels: BTreeMap::new(),
        roles: BTreeMap::new(),
        members: BTreeMap::new(),
    };
    let nested = format!("{}{}", "[".repeat(300), "]".repeat(300));
    match plan_apply(&current, nested.as_bytes(), &inventory) {
        ApplyPlan::Refuse { .. } => {}
        plan => panic!("deeply nested document must refuse, got: {plan:?}"),
    }
}
