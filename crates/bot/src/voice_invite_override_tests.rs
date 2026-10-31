//! Acceptance tests: a stored per-guild invite code overrides the vanity
//! fallback on the `/invite` path.
//!
//! A `guild_settings` row for `TWO_GUILD_INVITE_CODE` on one guild must win
//! over that guild's gateway vanity code when `/invite` is answered: a
//! stored code passing `is_valid_invite_code` (ASCII alphanumeric plus
//! `-`/`_`, 2 to 32 characters) is passed through unchanged, guilds without
//! a row behave exactly as today (the vanity code, or no code), and an
//! invalid stored value fails closed to the vanity fallback with a named
//! log reason — never a panic, never the invalid-code notice in place of a
//! usable vanity code, and never the rejected value in any log line or
//! reply (a stale code may be a rotated secret).
//!
//! The builder must preserve this file verbatim (see the manifest on the
//! acceptance card). The catalogue, live-snapshot, render-control and
//! database tests use only today's API apart from the contracted seam and
//! fail because the key is not yet classified. The resolution tests
//! contract one new pure seam, shaped like the empty-grace override's
//! `stored_empty_grace_with`:
//! `stored_invite_code_with(guild, fallback, live) -> Option<String>`,
//! where `fallback` is the vanity code from the gateway cache. Its invalid
//! path logs one warning carrying the setting key and
//! `reason = "invalid_invite_code"`, with no value field. The builder wires
//! that seam where the invite responder's code is chosen today (the
//! `vanity_code_from_cache` call site), reading the runtime's live-settings
//! source, so the resolved code is what reaches `invite_render`. No new
//! environment key is read and no `/invite` registration change is made.
//! The `control_` test uses no stored row and passes today; it must stay
//! green.

use super::*;
use serde_json::json;
use two_bot_core::settings::{
    classify_key, live_channel, validate_write, LiveSettings, SettingClass, SettingRow,
    SettingsSnapshot, HOT_WIRED,
};
use two_bot_core::voice_utilities::{INVALID_INVITE_CODE, NO_INVITE_CONFIGURED};

/// The stored per-guild invite-code key. Store-only: no environment
/// variable of this name is ever read.
const KEY: &str = "TWO_GUILD_INVITE_CODE";

/// The gateway vanity code every fallback assertion is measured against.
const VANITY: &str = "vanitycode";

/// A distinctive invalid stored value (URL-shaped, so it also fails the
/// charset twice over). It must never appear in a log line or a reply.
const REJECTED_CANARY: &str = "canary/rejected.value-zz9";

fn guild() -> String {
    GUILD.to_string()
}

fn setting_row(guild_id: &str, value: serde_json::Value) -> SettingRow {
    SettingRow {
        guild_id: guild_id.to_owned(),
        key: KEY.to_owned(),
        value,
        version: 1,
    }
}

/// A published live snapshot holding exactly `rows`.
fn live_with(rows: Vec<SettingRow>) -> LiveSettings {
    let (mut writer, reader) = live_channel();
    writer.publish(&SettingsSnapshot { revision: 1, rows });
    reader
}

#[test]
fn stored_invite_code_key_is_classified_hot_and_hot_wired() {
    assert_eq!(
        classify_key(KEY),
        Some(SettingClass::Hot),
        "a stored override that applies without a restart must be Hot"
    );
    assert!(
        HOT_WIRED.contains(&KEY),
        "live readers only see hot-wired keys; {KEY} must be listed"
    );
    let write = validate_write(&guild(), KEY, Some(json!("storedcode9")), "test")
        .expect("a per-guild invite code must be storable");
    assert_eq!(write.class, SettingClass::Hot);
}

#[test]
fn published_snapshot_serves_the_stored_invite_code_live() {
    let (mut writer, reader) = live_channel();
    let report = writer.publish(&SettingsSnapshot {
        revision: 1,
        rows: vec![setting_row(&guild(), json!("storedcode9"))],
    });
    assert!(
        report.hot.iter().any(|change| change.key == KEY),
        "the stored row must report as applied live, not restart-required: {report:?}"
    );
    assert!(
        !report.cold.iter().any(|change| change.key == KEY),
        "the stored row must not report as cold: {report:?}"
    );
    assert_eq!(
        reader.get(&guild(), KEY),
        Some(json!("storedcode9")),
        "the invite path reads the stored code through the live snapshot"
    );
}

/// Resolution contract (builder seam): `stored_invite_code_with(guild,
/// fallback, live)`, mirroring `stored_empty_grace_with`. A stored JSON
/// string passing `is_valid_invite_code` wins over the vanity fallback and
/// is passed through unchanged — case, `-` and `_` intact, bounds included.
#[test]
fn stored_code_overrides_the_vanity_fallback() {
    let longest = "a".repeat(32);
    for stored in [
        "ab",
        "storedcode9",
        "Stored-Code_9",
        "123456",
        longest.as_str(),
    ] {
        let live = live_with(vec![setting_row(&guild(), json!(stored))]);
        assert_eq!(
            stored_invite_code_with(&guild(), Some(VANITY), Some(&live)),
            Some(stored.to_owned()),
            "stored {stored} must override the vanity fallback, unchanged"
        );
        assert_eq!(
            stored_invite_code_with(&guild(), None, Some(&live)),
            Some(stored.to_owned()),
            "stored {stored} must apply even when the guild has no vanity code"
        );
    }
}

/// Resolution contract: no live reader, no row, or only another guild's row
/// all keep the exact vanity fallback — including no vanity code at all,
/// never a hardcoded substitute.
#[test]
fn unset_rows_fall_back_to_the_exact_vanity_code() {
    let empty = live_with(vec![]);
    let other_guild = live_with(vec![setting_row("200", json!("otherguild1"))]);
    for fallback in [Some(VANITY), None] {
        let expected = fallback.map(str::to_owned);
        assert_eq!(
            stored_invite_code_with(&guild(), fallback, None),
            expected,
            "no live reader keeps the vanity fallback"
        );
        assert_eq!(
            stored_invite_code_with(&guild(), fallback, Some(&empty)),
            expected,
            "no stored row keeps the vanity fallback"
        );
        assert_eq!(
            stored_invite_code_with(&guild(), fallback, Some(&other_guild)),
            expected,
            "another guild's row never drives this guild"
        );
    }
}

/// Resolution contract: every invalid stored value fails closed to the
/// vanity fallback. Only a JSON string can be a code; numbers, booleans,
/// arrays, objects and null are invalid rows, as are empty, too-short,
/// too-long, padded (codes pass through unchanged, never trimmed) and
/// URL-shaped strings. With no vanity code the result is `None`, never the
/// invalid value.
#[test]
fn invalid_stored_codes_fail_closed_to_the_fallback() {
    let too_long = "a".repeat(33);
    for value in [
        json!(""),
        json!("a"),
        json!(too_long),
        json!("has space"),
        json!(" paddedcode "),
        json!("https://discord.gg/storedcode9"),
        json!("discord.gg/storedcode9"),
        json!("code/with/slash"),
        json!("code.with.dot"),
        json!("code?query=1"),
        json!("code#fragment"),
        json!("code!bang"),
        json!("ünïcode12"),
        json!(REJECTED_CANARY),
        json!(123456),
        json!(true),
        json!(["storedcode9"]),
        json!({"code": "storedcode9"}),
        json!(null),
    ] {
        let live = live_with(vec![setting_row(&guild(), value.clone())]);
        assert_eq!(
            stored_invite_code_with(&guild(), Some(VANITY), Some(&live)),
            Some(VANITY.to_owned()),
            "stored {value} must fail closed to the vanity fallback"
        );
        assert_eq!(
            stored_invite_code_with(&guild(), None, Some(&live)),
            None,
            "stored {value} must fail closed to no code when there is no vanity code"
        );
    }
}

/// Resolution contract: an invalid stored value leaves a named reason in
/// the log — one warning line carrying the setting key and the
/// `invalid_invite_code` reason — and the rejected value appears nowhere
/// in the log, whole or in part. A valid stored value logs no such reason.
#[test]
fn invalid_stored_code_logs_a_named_reason_without_echoing_the_value() {
    // Install the process-wide capture first so callsite interest stays
    // stable, then read only this call's events from a scoped capture.
    super::tracing_capture::global();

    let live = live_with(vec![setting_row(&guild(), json!(REJECTED_CANARY))]);
    let capture = super::tracing_capture::Capture::default();
    let resolved = tracing::subscriber::with_default(capture.clone(), || {
        stored_invite_code_with(&guild(), Some(VANITY), Some(&live))
    });
    assert_eq!(resolved, Some(VANITY.to_owned()));
    let logged = capture.text();
    assert!(
        logged
            .lines()
            .any(|line| line.contains(KEY) && line.contains("invalid_invite_code")),
        "expected one warning naming {KEY} with reason invalid_invite_code: {logged}"
    );
    for fragment in [REJECTED_CANARY, "canary/rejected", "rejected.value"] {
        assert!(
            !logged.contains(fragment),
            "the rejected value must never be logged, found {fragment}: {logged}"
        );
    }

    let live = live_with(vec![setting_row(&guild(), json!("storedcode9"))]);
    let capture = super::tracing_capture::Capture::default();
    let resolved = tracing::subscriber::with_default(capture.clone(), || {
        stored_invite_code_with(&guild(), Some(VANITY), Some(&live))
    });
    assert_eq!(resolved, Some("storedcode9".to_owned()));
    let logged = capture.text();
    assert!(
        !logged
            .lines()
            .any(|line| line.contains(KEY) && line.contains("invalid_invite_code")),
        "a valid stored code logs no invalid_invite_code reason: {logged}"
    );
}

/// Invite path: the reply renders the stored code's link, not the vanity
/// code's, once the stored row is in force.
#[test]
fn invite_reply_renders_the_stored_code_not_the_vanity_code() {
    let live = live_with(vec![setting_row(&guild(), json!("storedcode9"))]);
    let resolved = stored_invite_code_with(&guild(), Some(VANITY), Some(&live));
    let reply = invite_render(resolved.as_deref());
    assert_eq!(reply, "Join the server: https://discord.gg/storedcode9");
    assert!(
        !reply.contains(VANITY),
        "the vanity code must not appear once a stored code wins: {reply}"
    );
}

/// Invite path: an invalid stored row renders the vanity fallback's link —
/// fail-closed means the usable fallback, never the invalid-code notice and
/// never the rejected value. With no vanity code it renders the
/// not-configured notice, exactly as an unset guild does today.
#[test]
fn invite_reply_with_an_invalid_stored_row_renders_the_vanity_fallback() {
    let live = live_with(vec![setting_row(&guild(), json!(REJECTED_CANARY))]);

    let resolved = stored_invite_code_with(&guild(), Some(VANITY), Some(&live));
    let reply = invite_render(resolved.as_deref());
    assert_eq!(
        reply,
        format!("Join the server: https://discord.gg/{VANITY}")
    );
    assert_ne!(
        reply, INVALID_INVITE_CODE,
        "an invalid stored row must not mask a usable vanity code"
    );
    assert!(
        !reply.contains(REJECTED_CANARY),
        "the rejected value must never reach the reply: {reply}"
    );

    let resolved = stored_invite_code_with(&guild(), None, Some(&live));
    let reply = invite_render(resolved.as_deref());
    assert_eq!(
        reply, NO_INVITE_CONFIGURED,
        "an invalid stored row with no vanity code renders as unconfigured"
    );
    assert!(
        !reply.contains(REJECTED_CANARY),
        "the rejected value must never reach the reply: {reply}"
    );
}

/// Control: no stored row anywhere — the render the invite path already
/// serves today stays byte-identical (vanity link, unconfigured notice,
/// invalid notice without echo). Uses only today's API; passes before the
/// builder's change and must stay green after it.
#[test]
fn control_unset_guild_invite_reply_is_byte_identical_to_today() {
    assert_eq!(
        invite_render(Some(VANITY)),
        format!("Join the server: https://discord.gg/{VANITY}")
    );
    assert_eq!(invite_render(None), NO_INVITE_CONFIGURED);
    assert_eq!(invite_render(Some("")), NO_INVITE_CONFIGURED);
    let reply = invite_render(Some(REJECTED_CANARY));
    assert_eq!(reply, INVALID_INVITE_CODE);
    assert!(
        !reply.contains(REJECTED_CANARY),
        "today's invalid notice never echoes the rejected value: {reply}"
    );
}

/// Store-backed path: a real `guild_settings` write on a disposable
/// migrated database must reach the invite reply through the snapshot
/// load, for that guild only; a hand-edited invalid row in the same table
/// fails closed to the vanity code through the same real load path.
/// Self-skips where no test database is supplied (the DB lane supplies
/// one), exactly like the empty-grace store acceptance.
#[tokio::test]
async fn stored_row_in_a_real_database_reaches_the_invite_reply() {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI must supply the guarded test database"
        );
        eprintln!("SKIP invite-override store acceptance: TWO_TEST_DATABASE_URL is not set");
        return;
    };
    use two_bot_cutover::settings::SettingsStore;
    use two_bot_testsupport::TestDatabase;

    let fixture_db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create migrated test fixture");
    let pool = fixture_db.pool().clone();
    let store = SettingsStore::new(&pool);
    store
        .set(&guild(), KEY, Some(json!("storedcode9")), "test")
        .await
        .expect("store the per-guild invite code");

    let snapshot = store
        .load_snapshot()
        .await
        .expect("load the stored guild settings snapshot");
    let (mut writer, reader) = live_channel();
    writer.publish(&snapshot);
    assert_eq!(
        reader.get(&guild(), KEY),
        Some(json!("storedcode9")),
        "the stored row must be served live for its guild"
    );
    assert_eq!(
        reader.get("200", KEY),
        None,
        "one guild's stored invite code never leaks to another guild"
    );
    let resolved = stored_invite_code_with(&guild(), Some(VANITY), Some(&reader));
    assert_eq!(
        invite_render(resolved.as_deref()),
        "Join the server: https://discord.gg/storedcode9",
        "the database row, not the vanity code, drives the invite reply"
    );

    // A hand-edited invalid row in the same table fails closed to the
    // vanity code through the same real load path, without echoing it.
    store
        .set(&guild(), KEY, Some(json!(REJECTED_CANARY)), "test")
        .await
        .expect("store an invalid invite code");
    let snapshot = store
        .load_snapshot()
        .await
        .expect("reload the stored guild settings snapshot");
    let (mut writer, reader) = live_channel();
    writer.publish(&snapshot);
    let resolved = stored_invite_code_with(&guild(), Some(VANITY), Some(&reader));
    let reply = invite_render(resolved.as_deref());
    assert_eq!(
        reply,
        format!("Join the server: https://discord.gg/{VANITY}"),
        "an invalid database row resolves to the vanity fallback"
    );
    assert!(
        !reply.contains(REJECTED_CANARY),
        "the rejected database value must never reach the reply: {reply}"
    );
}
