//! Protected, offline process-boundary acceptance tests; see the fixture manifest.
//! Invalid inputs must stop before admission/database bootstrap, not silently
//! select defaults, normalize a live-guild alias, or turn a bounded scan unbounded.

use std::process::{Command, Output};
use two_bot_cutover::LIVE_GUILD_ID;

const CAPTURE: &str = env!("CARGO_BIN_EXE_capture");
const MESSAGES: &str = env!("CARGO_BIN_EXE_backfill-messages");
const BACKFILL: &str = env!("CARGO_BIN_EXE_backfill");
const ROLLBACK: &str = env!("CARGO_BIN_EXE_rollback-delta");
const ENV_GUILD_BINS: [&str; 2] = [CAPTURE, MESSAGES];
const SCAN_BINS: [&str; 2] = [BACKFILL, MESSAGES];
const POOL_BINS: [&str; 4] = [CAPTURE, MESSAGES, BACKFILL, ROLLBACK];
const GUILD: &str = "100000000000000001";
const TOKEN: &str = "synthetic-offline-token-never-send";
// Neither SQLx nor the admission authority can open a socket for this scheme.
const POISON_DB: &str = "not-a-database-url-private-sentinel";
const PRIVATE_INPUT: &str = "private-input-sentinel-never-echo";
const SINCE: &str = "2026-10-01T12:00:00Z";
const IMPORT_FILE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/rollback_import_valid.ndjson"
);

fn run(bin: &str, args: &[&str], guild: &str, pool: Option<&str>) -> Output {
    let mut command = Command::new(bin);
    command
        .env_clear()
        .env("DISCORD_TOKEN", TOKEN)
        .env("DISCORD_GUILD_ID", guild)
        .env("TWO_DATABASE_URL", POISON_DB)
        .args(args);
    // Preserve coverage instrumentation without inheriting application settings.
    if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
        command.env("LLVM_PROFILE_FILE", profile);
    }
    if let Some(pool) = pool {
        command.env("TWO_DB_POOL_MAX", pool);
    }
    command.output().expect("run the actual cutover binary")
}

fn pool_args(bin: &str) -> Vec<&'static str> {
    if bin == ROLLBACK {
        vec!["--since", SINCE]
    } else {
        vec!["--dry-run"]
    }
}

fn no_fixture_leak(text: &str) {
    for sentinel in [TOKEN, POISON_DB, PRIVATE_INPUT] {
        assert!(!text.contains(sentinel), "fixture leaked: {text}");
    }
}

fn refusal(output: Output, field: &str) -> String {
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 diagnostic");
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(output.stdout.is_empty(), "refusal published stdout");
    no_fixture_leak(&stderr);
    assert!(stderr.starts_with("input error: "), "{stderr}");
    assert!(stderr.contains(field), "missing field {field}: {stderr}");
    // Pin machine-readable vocabulary, not a builder's choice of code/action.
    let code = stderr
        .split_whitespace()
        .find_map(|word| word.strip_prefix("code="))
        .expect("input refusal needs a stable code= token");
    assert!(
        !code.is_empty()
            && code.len() <= 64
            && code
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')),
        "invalid code vocabulary: {stderr}"
    );
    let next = stderr
        .split_once("next_action=")
        .expect("input refusal needs a next_action=")
        .1
        .trim();
    assert!(!next.is_empty(), "missing corrective action: {stderr}");
    for unexpected in [
        "admission authority",
        "cannot open database",
        "panicked at",
        "TWO_DATABASE_URL is required",
    ] {
        assert!(!stderr.contains(unexpected), "{stderr}");
    }
    stderr
}

fn fixed_refusals(bin: &str, field: &str, cases: impl IntoIterator<Item = Output>) {
    let mut first = None;
    for output in cases {
        let message = refusal(output, field);
        if let Some(first) = &first {
            assert_eq!(&message, first, "{bin}: refusal depends on raw input");
        } else {
            first = Some(message);
        }
    }
    assert!(first.is_some(), "empty acceptance matrix");
}

fn reached_bootstrap(bin: &str, output: Output) {
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 diagnostic");
    assert_eq!(output.status.code(), Some(1), "{bin}: {stderr}");
    assert!(output.stdout.is_empty(), "{bin}: unexpected output");
    no_fixture_leak(&stderr);
    if bin == ROLLBACK {
        assert_eq!(
            stderr,
            "cannot open database; connection details redacted\n"
        );
    } else {
        assert!(
            stderr.starts_with("send admission bootstrap failed:")
                && stderr.contains("admission authority unavailable"),
            "{bin}: {stderr}"
        );
    }
}

fn help(bin: &str, args: &[&str]) -> String {
    // Help must be independent of every credential and application environment.
    let output = Command::new(bin)
        .env_clear()
        .args(args)
        .output()
        .expect("run help");
    assert_eq!(output.status.code(), Some(0), "{bin}: {output:?}");
    assert!(output.stderr.is_empty(), "{bin}: {output:?}");
    String::from_utf8(output.stdout).expect("UTF-8 help")
}

fn documented_range(text: &str, field: &str) -> (u64, u64) {
    let range = text
        .lines()
        .filter(|line| line.contains(field))
        .flat_map(|line| {
            line.split(|character: char| {
                !character.is_ascii_digit() && character != '.' && character != '='
            })
        })
        .find_map(|word| {
            let (min, max) = word.split_once("..=")?;
            Some((min.parse::<u64>().ok()?, max.parse::<u64>().ok()?))
        })
        .unwrap_or_else(|| panic!("help must document {field} as min..=max: {text}"));
    assert_eq!(range.0, 1, "zero must not disable the bound: {text}");
    assert!(range.1 >= range.0 && range.1 < u32::MAX.into(), "{text}");
    range
}

#[test]
fn malformed_env_guilds_refuse_before_admission_or_database_bootstrap() {
    for bin in ENV_GUILD_BINS {
        fixed_refusals(
            bin,
            "DISCORD_GUILD_ID",
            [
                "abc",
                "0",
                "123",
                "000000000000000000",
                "18446744073709551616",
                "10000000000000000x",
                PRIVATE_INPUT,
            ]
            .map(|guild| run(bin, &["--dry-run"], guild, None)),
        );
    }
}

#[test]
fn noncanonical_live_guild_aliases_cannot_bypass_the_live_fence() {
    let aliases = [
        format!("+{LIVE_GUILD_ID}"),
        format!("0{LIVE_GUILD_ID}"),
        format!(" {LIVE_GUILD_ID}"),
        format!("{LIVE_GUILD_ID} "),
    ];
    for bin in ENV_GUILD_BINS {
        fixed_refusals(
            bin,
            "DISCORD_GUILD_ID",
            aliases
                .iter()
                .map(|guild| run(bin, &["--dry-run"], guild, None)),
        );
    }
}

#[test]
fn live_opt_in_does_not_authorize_a_malformed_guild() {
    for bin in ENV_GUILD_BINS {
        refusal(
            run(
                bin,
                &["--dry-run", "--allow-live-guild"],
                &format!("+{LIVE_GUILD_ID}"),
                None,
            ),
            "DISCORD_GUILD_ID",
        );
    }
}

#[test]
fn canonical_live_guild_keeps_the_existing_owner_opt_in_fence() {
    for bin in ENV_GUILD_BINS {
        let output = run(bin, &["--dry-run"], LIVE_GUILD_ID, None);
        assert_eq!(output.status.code(), Some(2), "{bin}: {output:?}");
        assert!(output.stdout.is_empty());
        assert_eq!(
            String::from_utf8(output.stderr).expect("diagnostic"),
            format!(
                "Refusing live guild {LIVE_GUILD_ID}. Use --allow-live-guild only for an owner-approved rollout.\n"
            )
        );
    }
}

#[test]
fn valid_guild_and_bare_live_opt_in_still_reach_the_same_bootstrap_boundary() {
    for bin in ENV_GUILD_BINS {
        for guild in [GUILD, LIVE_GUILD_ID] {
            let args = if guild == LIVE_GUILD_ID {
                vec!["--dry-run", "--allow-live-guild"]
            } else {
                vec!["--dry-run"]
            };
            reached_bootstrap(bin, run(bin, &args, guild, None));
        }
    }
}

#[test]
fn malformed_since_never_silently_selects_unbounded_history() {
    for equals in [false, true] {
        fixed_refusals(
            MESSAGES,
            "--since",
            [
                "garbage",
                "",
                "2026-02-30T12:00:00Z",
                "2026-10-01T25:00:00Z",
                PRIVATE_INPUT,
            ]
            .map(|value| {
                if equals {
                    run(
                        MESSAGES,
                        &["--dry-run", &format!("--since={value}")],
                        GUILD,
                        None,
                    )
                } else {
                    run(MESSAGES, &["--since", value, "--dry-run"], GUILD, None)
                }
            }),
        );
    }
    refusal(
        run(MESSAGES, &["--dry-run", "--since"], GUILD, None),
        "--since",
    );
}

#[test]
fn absent_and_valid_since_preserve_the_bootstrap_path() {
    for args in [
        vec!["--dry-run"],
        vec!["--dry-run", "--since", SINCE],
        vec!["--since=2026-10-01T12:00:00.123Z", "--dry-run"],
        vec!["--since", "2026-10-01T14:30:00+02:30", "--dry-run"],
    ] {
        reached_bootstrap(MESSAGES, run(MESSAGES, &args, GUILD, None));
    }
}

#[test]
fn malformed_page_caps_refuse_in_both_scan_consumers_and_flag_forms() {
    for bin in SCAN_BINS {
        for equals in [false, true] {
            fixed_refusals(
                bin,
                "--max-pages",
                [
                    "abc",
                    "0",
                    "-1",
                    "1.5",
                    "",
                    "4294967295",
                    "18446744073709551615",
                    "18446744073709551616",
                    PRIVATE_INPUT,
                ]
                .map(|value| {
                    if equals {
                        run(
                            bin,
                            &[&format!("--max-pages={value}"), "--dry-run"],
                            GUILD,
                            None,
                        )
                    } else {
                        run(bin, &["--dry-run", "--max-pages", value], GUILD, None)
                    }
                }),
            );
        }
        refusal(
            run(bin, &["--dry-run", "--max-pages"], GUILD, None),
            "--max-pages",
        );
    }
}

#[test]
fn help_documents_page_ranges_and_the_existing_scan_defaults() {
    for (bin, default) in [(BACKFILL, "25"), (MESSAGES, "60")] {
        let text = help(bin, &["--help"]);
        let (_, max) = documented_range(&text, "--max-pages");
        assert!(max >= 60, "existing defaults must remain in range");
        let page_line = text
            .lines()
            .find(|line| line.contains("--max-pages") && line.contains("default"))
            .expect("help must disclose the unchanged default");
        assert!(page_line
            .split(|c: char| !c.is_ascii_digit())
            .any(|s| s == default));
    }
    assert!(help(MESSAGES, &["--help"]).contains("--since"));
}

#[test]
fn declared_page_boundaries_are_enforced_without_overflowing_hint_math() {
    for bin in SCAN_BINS {
        let text = help(bin, &["--help"]);
        let (_, max) = documented_range(&text, "--max-pages");
        for value in ["1".to_owned(), max.to_string()] {
            reached_bootstrap(
                bin,
                run(bin, &["--dry-run", "--max-pages", &value], GUILD, None),
            );
        }
        refusal(
            run(
                bin,
                &["--dry-run", &format!("--max-pages={}", max + 1)],
                GUILD,
                None,
            ),
            "--max-pages",
        );
    }
}

#[test]
fn default_and_existing_page_caps_remain_valid() {
    for (bin, default) in [(BACKFILL, "25"), (MESSAGES, "60")] {
        for args in [
            vec!["--dry-run"],
            vec!["--dry-run", "--max-pages", default],
            vec!["--max-pages=1", "--dry-run"],
        ] {
            reached_bootstrap(bin, run(bin, &args, GUILD, None));
        }
    }
}

#[test]
fn unusable_pool_sizes_refuse_before_any_consumer_bootstraps() {
    for bin in POOL_BINS {
        let args = pool_args(bin);
        fixed_refusals(
            bin,
            "TWO_DB_POOL_MAX",
            [
                "",
                "0",
                "garbage",
                "-1",
                "1.5",
                "4294967295",
                "4294967296",
                "18446744073709551616",
                PRIVATE_INPUT,
            ]
            .map(|pool| run(bin, &args, GUILD, Some(pool))),
        );
    }
}

#[test]
fn pool_help_declares_and_enforces_both_boundaries() {
    for bin in POOL_BINS {
        let text = help(bin, &["--help"]);
        let (_, max) = documented_range(&text, "TWO_DB_POOL_MAX");
        assert!(max >= 5, "existing pool default must remain valid");
        for value in ["1".to_owned(), max.to_string()] {
            reached_bootstrap(bin, run(bin, &pool_args(bin), GUILD, Some(&value)));
        }
        refusal(
            run(bin, &pool_args(bin), GUILD, Some(&(max + 1).to_string())),
            "TWO_DB_POOL_MAX",
        );
    }
}

#[test]
fn absent_pool_and_valid_small_pool_sizes_preserve_the_bootstrap_path() {
    for bin in POOL_BINS {
        for pool in [None, Some("1"), Some("5")] {
            reached_bootstrap(bin, run(bin, &pool_args(bin), GUILD, pool));
        }
    }
}

#[test]
fn rollback_import_dry_run_keeps_its_existing_offline_plan() {
    for pool in [None, Some("1"), Some("5")] {
        let output = run(ROLLBACK, &["import", "--file", IMPORT_FILE], GUILD, pool);
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        let plan: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON plan");
        assert_eq!(plan["mode"], "dry-run");
        assert_eq!(plan["total_rows"], 4);
        no_fixture_leak(&String::from_utf8(output.stdout).expect("UTF-8 plan"));
    }
}

#[test]
fn rollback_import_cannot_bypass_the_pool_preflight() {
    let args = [
        "import",
        "--file",
        IMPORT_FILE,
        "--apply",
        "--allow-live-target",
    ];
    fixed_refusals(
        ROLLBACK,
        "TWO_DB_POOL_MAX",
        ["0", "garbage", "4294967296", PRIVATE_INPUT]
            .map(|pool| run(ROLLBACK, &args, GUILD, Some(pool))),
    );
    documented_range(&help(ROLLBACK, &["import", "--help"]), "TWO_DB_POOL_MAX");
}
