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

/// Resolution contract (builder seam): `stored_empty_grace_with(guild,
/// fallback, live)`, mirroring `live_poll_seconds_with`. Stored JSON numbers
/// and strings both count (the settings writer stores either); the accepted
/// range is the boot parser's 0..=600 whole seconds.
#[test]
fn stored_value_overrides_the_boot_fallback_within_range() {
    for (value, expected) in [
        (json!(0), Duration::ZERO),
        (json!(1), Duration::from_secs(1)),
        (json!(600), Duration::from_secs(600)),
        (json!("0"), Duration::ZERO),
        (json!("45"), Duration::from_secs(45)),
        (json!("600"), Duration::from_secs(600)),
    ] {
        let live = live_with(vec![setting_row(&guild(), value.clone())]);
        assert_eq!(
            stored_empty_grace_with(&guild(), EMPTY_ROOM_GRACE, Some(&live)),
            expected,
            "stored {value} must override the boot fallback"
        );
    }
}

/// Resolution contract: no live reader, no row, or only another guild's row
/// all keep the exact boot fallback — including a boot fallback of zero or
/// any other operator-chosen value, never a hardcoded substitute.
#[test]
fn unset_rows_fall_back_to_the_exact_boot_value() {
    let empty = live_with(vec![]);
    let other_guild = live_with(vec![setting_row("200", json!(0))]);
    for fallback in [
        Duration::ZERO,
        Duration::from_secs(45),
        EMPTY_ROOM_GRACE,
        Duration::from_secs(600),
    ] {
        assert_eq!(
            stored_empty_grace_with(&guild(), fallback, None),
            fallback,
            "no live reader keeps the boot value"
        );
        assert_eq!(
            stored_empty_grace_with(&guild(), fallback, Some(&empty)),
            fallback,
            "no stored row keeps the boot value"
        );
        assert_eq!(
            stored_empty_grace_with(&guild(), fallback, Some(&other_guild)),
            fallback,
            "another guild's row never drives this guild"
        );
    }
}

/// Resolution contract: every out-of-range or malformed stored value fails
/// closed to the boot fallback. The fallback here is 45 s — neither the
/// shipped default nor zero — so a result of 60 s (hardcoded default) or
/// 0 s (zero-grace sweep) both fail this test.
#[test]
fn out_of_range_stored_values_fail_closed_to_the_boot_fallback() {
    let fallback = Duration::from_secs(45);
    for value in [
        json!(601),
        json!(86400),
        json!("601"),
        json!(-1),
        json!("-1"),
        json!("abc"),
        json!("1.5"),
        json!(1.5),
        json!(""),
        json!(true),
        json!([0]),
        json!(null),
    ] {
        let live = live_with(vec![setting_row(&guild(), value.clone())]);
        assert_eq!(
            stored_empty_grace_with(&guild(), fallback, Some(&live)),
            fallback,
            "stored {value} must fail closed to the boot fallback"
        );
    }
}

/// Resolution contract: an out-of-range stored value leaves a named reason
/// in the log — one warning line carrying the setting key and the
/// `out_of_range` reason — so an operator can find why the boot value is in
/// force. An in-range value logs no such reason.
#[test]
fn out_of_range_stored_value_logs_a_named_reason() {
    // Install the process-wide capture first so callsite interest stays
    // stable, then read only this call's events from a scoped capture.
    super::tracing_capture::global();

    let live = live_with(vec![setting_row(&guild(), json!(601))]);
    let capture = super::tracing_capture::Capture::default();
    let resolved = tracing::subscriber::with_default(capture.clone(), || {
        stored_empty_grace_with(&guild(), EMPTY_ROOM_GRACE, Some(&live))
    });
    assert_eq!(resolved, EMPTY_ROOM_GRACE);
    let logged = capture.text();
    assert!(
        logged
            .lines()
            .any(|line| line.contains(KEY) && line.contains("out_of_range")),
        "expected one warning naming {KEY} with reason out_of_range: {logged}"
    );

    let live = live_with(vec![setting_row(&guild(), json!(30))]);
    let capture = super::tracing_capture::Capture::default();
    let resolved = tracing::subscriber::with_default(capture.clone(), || {
        stored_empty_grace_with(&guild(), EMPTY_ROOM_GRACE, Some(&live))
    });
    assert_eq!(resolved, Duration::from_secs(30));
    let logged = capture.text();
    assert!(
        !logged
            .lines()
            .any(|line| line.contains(KEY) && line.contains("out_of_range")),
        "an in-range stored value logs no out_of_range reason: {logged}"
    );
}

/// One room (500) already empty, with the resolved grace applied to the
/// live guild exactly where production applies the boot value.
async fn sweep_worker(grace: Duration) -> (GuildRoomWorker<Store, Http>, Trace) {
    let (live, store, http, trace) = fixture();
    store.rooms.lock().unwrap().insert(500, room(500));
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    let worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.live.set_empty_grace(grace);
    (worker, trace)
}

/// Sweep path: a stored grace longer than the boot value holds the room
/// past the boot grace and deletes it at the stored one.
#[tokio::test(start_paused = true)]
async fn sweep_holds_the_room_for_the_stored_grace_not_the_boot_grace() {
    let live = live_with(vec![setting_row(&guild(), json!(600))]);
    let grace = stored_empty_grace_with(&guild(), EMPTY_ROOM_GRACE, Some(&live));
    let (mut worker, trace) = sweep_worker(grace).await;
    tokio::time::advance(EMPTY_ROOM_GRACE).await;
    worker.reconcile();
    assert!(
        !worker.dispatch_one(0).await,
        "the boot grace must not delete a room under a longer stored grace"
    );
    tokio::time::advance(Duration::from_secs(600) - EMPTY_ROOM_GRACE).await;
    worker.reconcile();
    dispatch(&mut worker, 1).await;
    assert_eq!(*trace.lock().unwrap(), ["delete:500", "forget:500"]);
}

/// Sweep path: a stored zero deletes on the first sweep even when the boot
/// value would have waited the full default grace.
#[tokio::test(start_paused = true)]
async fn sweep_deletes_at_once_under_a_stored_zero_grace() {
    let live = live_with(vec![setting_row(&guild(), json!(0))]);
    let grace = stored_empty_grace_with(&guild(), EMPTY_ROOM_GRACE, Some(&live));
    let (mut worker, trace) = sweep_worker(grace).await;
    worker.reconcile();
    dispatch(&mut worker, 0).await;
    assert_eq!(*trace.lock().unwrap(), ["delete:500", "forget:500"]);
}

/// Sweep path: an out-of-range stored row sweeps at the boot grace — the
/// room survives just before it and is deleted at it, never instantly.
#[tokio::test(start_paused = true)]
async fn sweep_with_an_out_of_range_stored_row_uses_the_boot_grace_not_zero() {
    let live = live_with(vec![setting_row(&guild(), json!(601))]);
    let grace = stored_empty_grace_with(&guild(), EMPTY_ROOM_GRACE, Some(&live));
    let (mut worker, trace) = sweep_worker(grace).await;
    worker.reconcile();
    assert!(
        !worker.dispatch_one(0).await,
        "an out-of-range row must never become a zero-grace sweep"
    );
    tokio::time::advance(EMPTY_ROOM_GRACE).await;
    worker.reconcile();
    dispatch(&mut worker, 1).await;
    assert_eq!(*trace.lock().unwrap(), ["delete:500", "forget:500"]);
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
        reader.get("200", KEY),
        None,
        "one guild's stored override never leaks to another guild"
    );
    assert_eq!(
        stored_empty_grace_with(&guild(), EMPTY_ROOM_GRACE, Some(&reader)),
        Duration::ZERO,
        "the database row, not the boot value, drives the resolved grace"
    );

    // A hand-edited out-of-range row in the same table fails closed to the
    // boot value through the same real load path.
    store
        .set(&guild(), KEY, Some(json!(601)), "test")
        .await
        .expect("store an out-of-range override");
    let snapshot = store
        .load_snapshot()
        .await
        .expect("reload the stored guild settings snapshot");
    let (mut writer, reader) = live_channel();
    writer.publish(&snapshot);
    assert_eq!(
        stored_empty_grace_with(&guild(), EMPTY_ROOM_GRACE, Some(&reader)),
        EMPTY_ROOM_GRACE,
        "an out-of-range database row resolves to the boot fallback"
    );
}
