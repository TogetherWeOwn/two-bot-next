//! Acceptance tests: the deletion sweep honors a stored per-guild
//! empty-grace override.
//!
//! A `guild_settings` row for `TWO_TEMP_VOICE_EMPTY_GRACE_SECONDS` on one
//! guild must drive that guild's empty-room deletion sweep: the stored
//! whole-seconds value (0 to 600, the boot env parser's range) wins, the
//! boot env value stays the fallback for guilds without a row, guilds
//! without a row behave exactly as before, and an out-of-range or malformed
//! stored value fails closed to the boot fallback with a named log reason —
//! never a panic and never a zero-grace sweep the operator did not ask for.
//!
//! The builder must preserve this file verbatim (see the manifest on the
//! acceptance card). The catalogue, live-snapshot and database tests use
//! only today's API and fail because the key is still `Cold` and unwired.
//! The sweep-resolution tests contract one new pure seam, shaped like the
//! feed interval's `live_poll_seconds_with`:
//! `stored_empty_grace_with(guild, fallback, live) -> Duration`. The builder
//! wires that seam so its result reaches `LiveGuild::set_empty_grace` for
//! the guild on the sweep path. The `control_` test uses no stored row and
//! passes today; it must stay green.

use super::*;
use serde_json::json;
use two_bot_core::settings::{
    classify_key, live_channel, validate_write, LiveSettings, SettingClass, SettingRow,
    SettingsSnapshot, HOT_WIRED,
};

/// The stored per-guild key (same name as the boot env fallback).
const KEY: &str = "TWO_TEMP_VOICE_EMPTY_GRACE_SECONDS";

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
fn stored_empty_grace_key_is_classified_hot_and_hot_wired() {
    assert_eq!(
        classify_key(KEY),
        Some(SettingClass::Hot),
        "a stored override that applies without a restart must be Hot"
    );
    assert!(
        HOT_WIRED.contains(&KEY),
        "live readers only see hot-wired keys; {KEY} must be listed"
    );
    let write = validate_write(&guild(), KEY, Some(json!(30)), "test")
        .expect("a per-guild empty-grace value must be storable");
    assert_eq!(write.class, SettingClass::Hot);
}

#[test]
fn published_snapshot_serves_the_stored_empty_grace_live() {
    let (mut writer, reader) = live_channel();
    let report = writer.publish(&SettingsSnapshot {
        revision: 1,
        rows: vec![setting_row(&guild(), json!(0))],
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
        Some(json!(0)),
        "the sweep reads the stored value through the live snapshot"
    );
}

/// Control: no stored row anywhere — the sweep keeps the shipped default,
/// byte-identical to today (no delete before the grace, delete at it).
#[tokio::test(start_paused = true)]
async fn control_unset_guild_sweep_keeps_the_default_grace() {
    let (live, store, http, trace) = fixture();
    store.rooms.lock().unwrap().insert(500, room(500));
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    tokio::time::advance(EMPTY_ROOM_GRACE - Duration::from_secs(1)).await;
    worker.reconcile();
    assert!(
        !worker.dispatch_one(0).await,
        "an unset guild keeps the full default grace"
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    worker.reconcile();
    dispatch(&mut worker, 1).await;
    assert_eq!(*trace.lock().unwrap(), ["delete:500", "forget:500"]);
}

/// Store-backed path: a real `guild_settings` write on a disposable
/// migrated database must reach a live reader through the snapshot load,
/// for that guild only. Self-skips where no test database is supplied
/// (the DB lane supplies one), exactly like the settings poll acceptance.
#[tokio::test]
async fn stored_row_in_a_real_database_reaches_the_live_snapshot() {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI must supply the guarded test database"
        );
        eprintln!("SKIP empty-grace store acceptance: TWO_TEST_DATABASE_URL is not set");
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
        .set(&guild(), KEY, Some(json!(0)), "test")
        .await
        .expect("store the per-guild empty-grace override");

    let snapshot = store
        .load_snapshot()
        .await
        .expect("load the stored guild settings snapshot");
    let (mut writer, reader) = live_channel();
    writer.publish(&snapshot);
    assert_eq!(
        reader.get(&guild(), KEY),
        Some(json!(0)),
        "the stored row must be served live for its guild"
    );
    assert_eq!(
        reader.get("999999999999999999", KEY),
        None,
        "one guild's stored override never leaks to another guild"
    );
}
