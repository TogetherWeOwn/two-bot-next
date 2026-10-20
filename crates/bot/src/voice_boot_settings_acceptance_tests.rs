//! Acceptance tests: a malformed fatal voice boot setting identifies
//! itself by its fixed setting key, without exposing its value, before
//! the runtime setup failure is returned.
//!
//! Four boot validators feed `build_production_runtime`: the protected
//! infrastructure channels (`DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID`,
//! `TWO_TEMP_VOICE_GENERATOR_CHANNEL_ID`, `TWO_TEMP_VOICE_CATEGORY_ID` and
//! `TWO_TEMP_VOICE_PROTECTED_CHANNEL_IDS`), the generator channel
//! (`TWO_TEMP_VOICE_GENERATOR_CHANNEL_ID` again), the empty-room grace
//! (`TWO_TEMP_VOICE_EMPTY_GRACE_SECONDS`) and the synthetic humans
//! (`TWO_TEMP_VOICE_SYNTHETIC_HUMAN_IDS`). Today every validator failure
//! collapses into the same `RoomHttpError::InvalidRequest`, so the
//! gateway's `voice HTTP setup failed` line cannot say which setting an
//! operator must fix.
//!
//! Builder contract (this file is preserved verbatim; see the manifest
//! on the acceptance card):
//!
//! - When any of the four validators refuses a configured value,
//!   `build_production_runtime` still returns
//!   `Err(RoomHttpError::InvalidRequest)` (fail-closed, unchanged), but
//!   first records exactly one WARN diagnostic with the fixed message
//!   `temp-voice boot setting invalid; refusing voice runtime`, a `key`
//!   field naming the exact fixed environment key whose value was
//!   refused, and the fixed diagnostic class `reason` = `invalid_value`
//!   (the same class the nonfatal admission warning uses; the message
//!   is what distinguishes a fatal refusal from a per-knob fallback).
//! - For the protected-channels validator the key is the specific
//!   sub-key that failed (lobby, generator, category or the protected
//!   list), never a generic validator name.
//! - The diagnostic never carries the configured value, a raw parser
//!   error, the bot token, or any channel/member list: key and fixed
//!   reason only. No new environment variable, no setting-semantics
//!   change, no admission-warning rewrite.
//! - Validation order is the existing one (protected channels in their
//!   current sub-key order, then generator, empty grace, synthetic
//!   humans); the first refusal wins and is the only one named. Note the
//!   generator key is read by the protected validator first, so a
//!   malformed generator id is expected to be reported under that same
//!   key from the first pass - the operator-visible key is identical.
//! - Valid, absent and blank values keep building the runtime exactly
//!   as today and record no boot diagnostic. The separately nonfatal
//!   admission knobs keep their existing fallback warning and never
//!   produce the boot diagnostic or a refusal.
//! - The new message is catalogued in
//!   `docs/observability-event-catalog.md` (and joins the observability
//!   conformance known set like every other `voice_rooms.rs` message).
//!
//! These tests drive the production seam directly: the pool is lazy and
//! is never connected (the validators run before any query, and
//! constructing the responder performs no I/O), and the token is a
//! fixture string. Environment access is serialized by a file-local
//! lock that snapshots and restores every key it touches, so tests
//! cannot leak settings into siblings. The `control_` tests use only
//! today's behavior and pass on current main; they must stay green.

use super::*;

const LOBBY_KEY: &str = "DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID";
const GENERATOR_KEY: &str = "TWO_TEMP_VOICE_GENERATOR_CHANNEL_ID";
const CATEGORY_KEY: &str = "TWO_TEMP_VOICE_CATEGORY_ID";
const PROTECTED_KEY: &str = "TWO_TEMP_VOICE_PROTECTED_CHANNEL_IDS";
const EMPTY_GRACE_KEY: &str = "TWO_TEMP_VOICE_EMPTY_GRACE_SECONDS";
const SYNTHETIC_KEY: &str = "TWO_TEMP_VOICE_SYNTHETIC_HUMAN_IDS";
const ADMISSION_USER_KEY: &str = "TWO_TEMP_VOICE_MAX_PER_USER";
const ADMISSION_GUILD_KEY: &str = "TWO_TEMP_VOICE_MAX_PER_GUILD";
const ADMISSION_COOLDOWN_KEY: &str = "TWO_TEMP_VOICE_CREATE_COOLDOWN_SECONDS";

/// Every environment key `build_production_runtime` may consult on the
/// paths under test, cleared per case so ambient CI settings cannot
/// change which validator runs.
const ALL_KEYS: [&str; 9] = [
    LOBBY_KEY,
    GENERATOR_KEY,
    CATEGORY_KEY,
    PROTECTED_KEY,
    EMPTY_GRACE_KEY,
    SYNTHETIC_KEY,
    ADMISSION_USER_KEY,
    ADMISSION_GUILD_KEY,
    ADMISSION_COOLDOWN_KEY,
];

const BOOT_DIAGNOSTIC: &str = "temp-voice boot setting invalid; refusing voice runtime";
const FIXTURE_TOKEN: &str = "boot-fixture-token";

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Run the production setup seam with exactly `pairs` in the
/// environment, returning its refusal (if any) and the log text emitted
/// during the call.
fn boot(pairs: &[(&str, &str)]) -> (Result<(), RoomHttpError>, String) {
    boot_with_token(FIXTURE_TOKEN, pairs)
}

fn boot_with_token(token: &str, pairs: &[(&str, &str)]) -> (Result<(), RoomHttpError>, String) {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let capture = super::tracing_capture::global();
    let prior: Vec<(&str, Option<String>)> = ALL_KEYS
        .iter()
        .map(|key| (*key, std::env::var(key).ok()))
        .collect();
    for key in ALL_KEYS {
        std::env::remove_var(key);
    }
    for (key, value) in pairs {
        std::env::set_var(key, value);
    }
    let start = capture.text().len();
    // Building even a lazy pool requires a Tokio context; the seam
    // itself is synchronous and never awaits.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime builds");
    let _entered = runtime.enter();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@agent-testdb:5432/agent_test")
        .expect("a lazy pool needs no database connection");
    let result =
        build_production_runtime(token, pool, AutomodPolicy::default(), || None).map(|_| ());
    let logged = capture.text()[start..].to_owned();
    for (key, value) in prior {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }
    (result, logged)
}

fn boot_diagnostic_lines(logged: &str) -> Vec<&str> {
    logged
        .lines()
        .filter(|line| line.contains(BOOT_DIAGNOSTIC))
        .collect()
}

/// The shared refusal shape: unchanged fail-closed error, exactly one
/// WARN boot diagnostic naming `key` with the fixed class, and no raw
/// parser error anywhere in the emitted log.
fn assert_refusal_names(pairs: &[(&str, &str)], key: &str) -> String {
    let (result, logged) = boot(pairs);
    assert_eq!(
        result,
        Err(RoomHttpError::InvalidRequest),
        "a malformed fatal boot setting still refuses the runtime: {pairs:?}"
    );
    let lines = boot_diagnostic_lines(&logged);
    assert_eq!(
        lines.len(),
        1,
        "exactly one boot diagnostic names the refusal for {pairs:?}: {logged}"
    );
    let line = lines[0];
    assert!(
        line.starts_with("WARN"),
        "the boot diagnostic is a warning, like the admission fallback: {line}"
    );
    assert!(
        line.contains(&format!(" key={key:?}")),
        "the boot diagnostic binds the exact setting key to the key field: {line}"
    );
    assert!(
        line.contains(r#" reason="invalid_value""#),
        "the boot diagnostic binds the fixed invalid_value class to the reason field: {line}"
    );
    assert!(
        !logged.contains("invalid digit"),
        "raw parser errors never reach the log: {logged}"
    );
    logged
}

#[test]
fn malformed_protected_list_names_its_own_key() {
    assert_refusal_names(&[(PROTECTED_KEY, "700,bad-entry")], PROTECTED_KEY);
}

#[test]
fn malformed_lobby_names_the_lobby_key_not_the_protected_validator() {
    assert_refusal_names(&[(LOBBY_KEY, "bad-entry")], LOBBY_KEY);
}

#[test]
fn malformed_category_names_the_category_key() {
    assert_refusal_names(&[(CATEGORY_KEY, "0")], CATEGORY_KEY);
}

#[test]
fn malformed_generator_names_the_generator_key() {
    assert_refusal_names(&[(GENERATOR_KEY, "900,901")], GENERATOR_KEY);
}

#[test]
fn malformed_empty_grace_names_the_grace_key() {
    assert_refusal_names(&[(EMPTY_GRACE_KEY, "bad-entry")], EMPTY_GRACE_KEY);
    // Out of range is the same refusal class as unparsable.
    assert_refusal_names(&[(EMPTY_GRACE_KEY, "601")], EMPTY_GRACE_KEY);
}

#[test]
fn malformed_synthetic_humans_name_the_synthetic_key() {
    assert_refusal_names(&[(SYNTHETIC_KEY, "bad-entry")], SYNTHETIC_KEY);
    // More than four identities refuses too, and the refused list
    // itself never reaches the log.
    let logged = assert_refusal_names(
        &[(SYNTHETIC_KEY, "910001,910002,910003,910004,910005")],
        SYNTHETIC_KEY,
    );
    assert!(
        !logged.contains("910001"),
        "configured member lists never reach the log: {logged}"
    );
}

/// Secrecy: every fatal validator can be fed a distinctive sentinel;
/// none of them may echo it (or the token) into the diagnostic or any
/// other line emitted while refusing.
#[test]
fn refused_values_and_the_token_never_reach_the_log() {
    for (key, sentinel) in [
        (LOBBY_KEY, "bad-entry-lobby"),
        (GENERATOR_KEY, "bad-entry-generator"),
        (CATEGORY_KEY, "bad-entry-category"),
        (PROTECTED_KEY, "bad-entry-protected"),
        (EMPTY_GRACE_KEY, "bad-entry-grace"),
        (SYNTHETIC_KEY, "bad-entry-synthetic"),
    ] {
        let logged = assert_refusal_names(&[(key, sentinel)], key);
        assert!(
            !logged.contains(sentinel),
            "{key}: the refused value must not appear in any emitted line: {logged}"
        );
    }

    let (result, logged) = boot_with_token("bad-entry-token", &[(EMPTY_GRACE_KEY, "bad")]);
    assert_eq!(result, Err(RoomHttpError::InvalidRequest));
    assert!(
        !logged.contains("bad-entry-token"),
        "the bot token must not appear in any emitted line: {logged}"
    );
}

/// Deterministic order: with several invalid settings the existing
/// validation order decides, the first refusal is the only one named,
/// and later invalid settings are not mentioned at all.
#[test]
fn multiple_invalid_settings_name_only_the_first_in_existing_order() {
    let logged = assert_refusal_names(
        &[
            (PROTECTED_KEY, "bad-entry"),
            (EMPTY_GRACE_KEY, "bad-entry"),
            (SYNTHETIC_KEY, "bad-entry"),
        ],
        PROTECTED_KEY,
    );
    assert!(
        !boot_diagnostic_lines(&logged)[0].contains(EMPTY_GRACE_KEY),
        "later validators are not named once an earlier one refused: {logged}"
    );

    assert_refusal_names(
        &[(EMPTY_GRACE_KEY, "bad-entry"), (SYNTHETIC_KEY, "bad-entry")],
        EMPTY_GRACE_KEY,
    );

    // Each adjacent protected sub-key keeps its existing precedence,
    // even when the later key is configured first in the input pairs.
    for (earlier, later) in [
        (LOBBY_KEY, GENERATOR_KEY),
        (GENERATOR_KEY, CATEGORY_KEY),
        (CATEGORY_KEY, PROTECTED_KEY),
    ] {
        let logged = assert_refusal_names(&[(later, "bad-entry"), (earlier, "bad-entry")], earlier);
        assert!(
            !logged.contains(later),
            "only the earlier protected sub-key {earlier} is named, not {later}: {logged}"
        );
    }

    // Inside the protected validator its current sub-key order wins:
    // the lobby is consulted before the protected list.
    assert_refusal_names(
        &[(PROTECTED_KEY, "bad-entry"), (LOBBY_KEY, "bad-entry")],
        LOBBY_KEY,
    );
}

/// Control: with nothing configured, the runtime builds as today and
/// records no boot diagnostic. Passes on current main.
#[test]
fn control_absent_settings_build_without_a_boot_diagnostic() {
    let (result, logged) = boot(&[]);
    assert_eq!(result, Ok(()), "absent settings keep today's boot");
    assert!(
        boot_diagnostic_lines(&logged).is_empty(),
        "absent settings record no boot diagnostic: {logged}"
    );
}

/// Control: valid values for every fatal setting build as today.
/// Passes on current main.
#[test]
fn control_valid_settings_build_without_a_boot_diagnostic() {
    let (result, logged) = boot(&[
        (LOBBY_KEY, "600"),
        (GENERATOR_KEY, "201"),
        (CATEGORY_KEY, "401"),
        (PROTECTED_KEY, "700, 701"),
        (EMPTY_GRACE_KEY, "120"),
        (SYNTHETIC_KEY, "301,302"),
    ]);
    assert_eq!(result, Ok(()), "valid settings keep today's boot");
    assert!(
        boot_diagnostic_lines(&logged).is_empty(),
        "valid settings record no boot diagnostic: {logged}"
    );
}

/// Control: blank values count as unset today (silent), and stay
/// silent. Passes on current main.
#[test]
fn control_blank_settings_build_without_a_boot_diagnostic() {
    let (result, logged) = boot(&[
        (LOBBY_KEY, "  "),
        (GENERATOR_KEY, ""),
        (CATEGORY_KEY, " \t"),
        (PROTECTED_KEY, ""),
        (EMPTY_GRACE_KEY, " "),
        (SYNTHETIC_KEY, "  "),
    ]);
    assert_eq!(result, Ok(()), "blank settings keep today's boot");
    assert!(
        boot_diagnostic_lines(&logged).is_empty(),
        "blank settings record no boot diagnostic: {logged}"
    );
}

/// The nonfatal admission knobs are a different contract: an invalid
/// admission value still builds the runtime (per-knob fallback with
/// its own existing warning) and never produces the fatal boot
/// diagnostic or leaks its value.
#[test]
fn invalid_admission_knobs_stay_nonfatal_and_emit_no_boot_diagnostic() {
    let (result, logged) = boot(&[(ADMISSION_USER_KEY, "bad-entry-admission")]);
    assert_eq!(
        result,
        Ok(()),
        "an invalid admission knob never refuses the runtime"
    );
    assert!(
        boot_diagnostic_lines(&logged).is_empty(),
        "admission fallback is not reported as a fatal boot refusal: {logged}"
    );
    assert!(
        !logged.contains("bad-entry-admission"),
        "the refused admission value must not appear in any emitted line: {logged}"
    );
}

/// The diagnostic bundle includes its catalog entry, so an operator
/// who greps the message finds what refused and what to fix.
#[test]
fn boot_diagnostic_is_catalogued_for_operators() {
    let catalog = std::fs::read_to_string(format!(
        "{}/../../docs/observability-event-catalog.md",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("docs/observability-event-catalog.md is readable from the crate root");
    let entry = catalog
        .lines()
        .find(|line| line.contains(BOOT_DIAGNOSTIC))
        .unwrap_or_else(|| {
            panic!("the observability catalog must carry the boot diagnostic message")
        });
    assert!(
        entry.contains("warn") || catalog.contains("refusing voice runtime` (warn)"),
        "the catalog entry records the diagnostic level: {entry}"
    );
}
