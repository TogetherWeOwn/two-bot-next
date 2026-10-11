//! Voice apply and ghost cleanup refuse unsafe CLI invocations (TOG-20885).
//!
//! Real-binary guards, not the library planner: each case spawns the compiled
//! `voice-config-apply` / `ghost-cleanup` binary with an isolated allowlisted
//! environment (no `TWO_DATABASE_URL`, no `DISCORD_TOKEN`) and asserts the
//! exact exit code plus a safe diagnostic fragment.
//!
//! Guard order pinned here (binary source in parentheses):
//! - `voice-config-apply --apply` without `--expect-hash` exits 2 with the
//!   hash-required diagnostic after the file read but before Discord/DB
//!   (`crates/cutover/src/bin/voice_config_apply.rs`: file read, then
//!   `apply && expected_hash.is_none()` refusal, then REST, then `open_db`).
//!   The fixture must be readable so the case cannot fail earlier with
//!   `cannot read --file`.
//! - `ghost-cleanup --seed --execute` exits 2 with the seed/execute refusal
//!   before guild/file/DB (`ghost_cleanup.rs`: seed+execute check first).
//! - The live-guild fence (`cli::require_guild`) runs before file/DB in both
//!   binaries; non-canonical spellings refuse as non-snowflakes before the
//!   fence compares. `--help` and ghost `--seed` work with no credentials.
//!
//! Docs note: `docs/voice-config-apply.md` already records exit 2 for usage /
//! live-guild fence and exit 3 for hash mismatch, which matches the missing
//! `--expect-hash` (usage, exit 2) versus wrong-hash (conflict, exit 3)
//! split pinned here. `ghost-cleanup` has no dedicated exit-code doc beyond
//! its `USAGE` string; the seed/execute and live-fence exits pinned here
//! follow the shared `cli::require_guild` / `usage_error` (exit 2) contract.
//! No production gate is changed by this target.
//!
//! Modeled on `crates/cutover/tests/raid_removal.rs`
//! `both_clis_refuse_live_before_file_or_database_access_and_help_is_offline`.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, Instant};

use two_bot_cutover::LIVE_GUILD_ID;

/// Synthetic non-live guild: canonical snowflake, never the live guild.
const GUILD: &str = "100000000000000010";
/// Bounded wall-clock budget per child invocation (refusals return at once).
const CHILD_BUDGET: Duration = Duration::from_secs(10);
/// Sentinel credentials that must never be echoed by a refusal diagnostic.
const SENTINEL_DB: &str = "postgres://sentinel-qa-20885:secret@example.invalid:1/sentinel";
const SENTINEL_TOKEN: &str = "sentinel-qa-20885-discord-token-secret";

fn scratch_root(prefix: &str) -> PathBuf {
    let base = std::env::var_os("PAPERCLIP_RUN_SCRATCH_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("PAPERCLIP_SCRATCH_DIR").map(PathBuf::from))
        .unwrap_or_else(std::env::temp_dir);
    let root = base.join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// Minimal readable V11-shaped fixture. The missing-hash refusal fires before
/// decode/plan, so the content only needs to exist and be readable.
fn write_voice_fixture(root: &Path) -> PathBuf {
    let path = root.join("voice-config.json");
    std::fs::write(
        &path,
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
        .to_string(),
    )
    .unwrap();
    path
}

fn run_isolated(bin: &str, args: &[&str], extra_env: &[(&str, &str)]) -> (Output, Duration) {
    let mut command = std::process::Command::new(bin);
    command.env_clear().env("PATH", "/usr/bin:/bin").args(args);
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let start = Instant::now();
    let output = command.output().unwrap();
    let elapsed = start.elapsed();
    assert!(
        elapsed < CHILD_BUDGET,
        "{bin} {args:?} took {elapsed:?}, over the {CHILD_BUDGET:?} bound"
    );
    (output, elapsed)
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

fn assert_no_sentinel(bin: &str, args: &[&str], output: &Output) {
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for sentinel in [SENTINEL_DB, SENTINEL_TOKEN] {
        assert!(
            !combined.contains(sentinel),
            "{bin} {args:?} echoed a sentinel credential"
        );
    }
}

fn assert_usage_refusal(
    bin: &str,
    args: &[&str],
    output: &Output,
    fragment: &str,
    forbidden: &[&str],
) {
    let err = stderr_of(output);
    assert_eq!(output.status.code(), Some(2), "{bin} {args:?}: {err}");
    assert!(
        output.stdout.is_empty(),
        "{bin} {args:?}: refusal must not print a report"
    );
    assert!(err.contains(fragment), "{bin} {args:?}: {err}");
    for banned in forbidden {
        assert!(
            !err.contains(banned),
            "{bin} {args:?} reached {banned}: {err}"
        );
    }
    assert_no_sentinel(bin, args, output);
}

#[test]
fn voice_apply_requires_expect_hash_before_discord_or_database() {
    let root = scratch_root("voice-apply-hash");
    let fixture = write_voice_fixture(&root);
    let bin = env!("CARGO_BIN_EXE_voice-config-apply");
    let args = [
        "--guild",
        GUILD,
        "--file",
        fixture.to_str().unwrap(),
        "--apply",
    ];
    // No credentials at all: the refusal must fire before REST/DB construction.
    // The banned list names dynamic overreach diagnostics only: the static
    // USAGE trailer itself names DISCORD_TOKEN/TWO_DATABASE_URL, so those
    // literals cannot discriminate.
    let (output, _) = run_isolated(bin, &args, &[]);
    assert_usage_refusal(
        bin,
        &args,
        &output,
        "--apply needs --expect-hash",
        &[
            "cannot read",
            "cannot open database",
            "is not set",
            "is required",
            "unavailable",
            "refused",
            "snapshot failed",
        ],
    );
    // Even with (sentinel) credentials present, the same usage refusal fires
    // first and the diagnostics never echo them.
    let (output, _) = run_isolated(
        bin,
        &args,
        &[
            ("TWO_DATABASE_URL", SENTINEL_DB),
            ("DISCORD_TOKEN", SENTINEL_TOKEN),
            ("DISCORD_BOT_TOKEN", SENTINEL_TOKEN),
        ],
    );
    assert_usage_refusal(
        bin,
        &args,
        &output,
        "--apply needs --expect-hash",
        &["cannot open database", "cannot read"],
    );
    std::fs::remove_file(&fixture).unwrap();
    std::fs::remove_dir(&root).unwrap();
}

#[test]
fn ghost_seed_never_executes_before_dependencies() {
    let bin = env!("CARGO_BIN_EXE_ghost-cleanup");
    let args = ["--seed", "--execute"];
    let (output, _) = run_isolated(bin, &args, &[]);
    assert_usage_refusal(
        bin,
        &args,
        &output,
        "--seed never executes",
        &[
            "cannot open",
            "cannot read",
            "database",
            "TWO_DATABASE_URL",
            "DISCORD_TOKEN",
        ],
    );
    let (output, _) = run_isolated(
        bin,
        &args,
        &[
            ("TWO_DATABASE_URL", SENTINEL_DB),
            ("DISCORD_TOKEN", SENTINEL_TOKEN),
        ],
    );
    assert_usage_refusal(bin, &args, &output, "--seed never executes", &[]);
}

#[test]
fn both_clis_refuse_live_before_file_or_database_access_and_help_is_offline() {
    let voice = env!("CARGO_BIN_EXE_voice-config-apply");
    let ghost = env!("CARGO_BIN_EXE_ghost-cleanup");
    let missing =
        scratch_root("voice-ghost-live").join("missing-input-that-must-never-be-read.json");
    let missing = missing.to_str().unwrap().to_owned();

    // Live guild refuses before any file or database access: the input path
    // does not exist, so a later read would fail differently.
    let voice_live = ["--guild", LIVE_GUILD_ID, "--file", &missing];
    let (output, _) = run_isolated(voice, &voice_live, &[]);
    assert_usage_refusal(
        voice,
        &voice_live,
        &output,
        "Refusing live guild",
        &["cannot read", "cannot open database", "database"],
    );

    let ghost_live = ["--guild", LIVE_GUILD_ID, "--channels", &missing];
    let (output, _) = run_isolated(ghost, &ghost_live, &[]);
    assert_usage_refusal(
        ghost,
        &ghost_live,
        &output,
        "Refusing live guild",
        &[
            "cannot open snapshot",
            "cannot read snapshot",
            "cannot open database",
            "database",
        ],
    );

    // Non-canonical live spellings refuse as non-snowflakes before the fence.
    let padded = format!("0{LIVE_GUILD_ID}");
    let voice_padded = ["--guild", &padded, "--file", &missing];
    let (output, _) = run_isolated(voice, &voice_padded, &[]);
    assert_usage_refusal(
        voice,
        &voice_padded,
        &output,
        "must be a Discord snowflake",
        &["Refusing live guild", "cannot read", "database"],
    );
    let ghost_padded = ["--guild", &padded, "--channels", &missing];
    let (output, _) = run_isolated(ghost, &ghost_padded, &[]);
    assert_usage_refusal(
        ghost,
        &ghost_padded,
        &output,
        "must be a Discord snowflake",
        &["Refusing live guild", "cannot open snapshot", "database"],
    );

    // Harmless controls work with no credentials.
    for bin in [voice, ghost] {
        let (output, _) = run_isolated(bin, &["--help"], &[]);
        assert!(
            output.status.success(),
            "{bin} --help: {}",
            stderr_of(&output)
        );
        assert!(
            String::from_utf8(output.stdout.clone())
                .unwrap()
                .contains("Usage:"),
            "{bin} --help must print usage"
        );
        assert_no_sentinel(bin, &["--help"], &output);
    }

    // Ghost `--seed` prints the dry-run demo report with no guild, file, DB
    // or token.
    let (output, _) = run_isolated(ghost, &["--seed"], &[]);
    assert!(
        output.status.success(),
        "ghost --seed: {}",
        stderr_of(&output)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("seed prints a JSON report");
    assert_eq!(report["tool"], "ghost-cleanup");
    assert_eq!(report["mode"], "dry-run");
    assert_no_sentinel(ghost, &["--seed"], &output);

    std::fs::remove_dir_all(Path::new(&missing).parent().expect("scratch parent")).unwrap();
}
