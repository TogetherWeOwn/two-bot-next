//! Protected acceptance: per-key `setting_changed` lines for the live five.
//!
//! PROTECTED FILE: the paired Builder reclassifies
//! `TWO_COMMUNITY_SCORECARD`, `TWO_COMMUNITY_RECOMMENDATIONS`,
//! `TWO_COMMUNITY_CORRECTION_CYCLES`, `TWO_PRESENCE_PROBE` and
//! `TWO_INACTIVITY_DAYS` to hot-wired without editing this file. Return
//! necessary corrections to the Test Author; never edit.
//!
//! Contract under test: a live poll carrying stored rows for the five
//! emits exactly one `setting_changed` line per key (names only — values
//! stay on their own lines by the existing generic path) inside a
//! `settings_applied` swap, and never a `settings_restart_required`
//! line for them. Pure in-memory via `live_channel`; no database.
#![cfg(test)]

use std::sync::{Arc, Mutex as StdMutex};

use serde_json::{json, Value};
use tracing_subscriber::{layer::SubscriberExt, Layer};
use two_bot_core::settings::{live_channel, SettingRow, SettingsSnapshot};

use super::*;

const FIVE: [&str; 5] = [
    "TWO_COMMUNITY_SCORECARD",
    "TWO_COMMUNITY_RECOMMENDATIONS",
    "TWO_COMMUNITY_CORRECTION_CYCLES",
    "TWO_PRESENCE_PROBE",
    "TWO_INACTIVITY_DAYS",
];

fn row(guild: &str, key: &str, value: Value) -> SettingRow {
    SettingRow {
        guild_id: guild.to_owned(),
        key: key.to_owned(),
        value,
        version: 1,
    }
}

/// Capture settings events on this thread while the guard lives.
/// `set_default` is thread-scoped; `#[test]` is current-thread.
#[derive(Clone, Default)]
struct Capture(Arc<StdMutex<String>>);

impl Capture {
    fn contents(&self) -> String {
        self.0.lock().expect("capture lock").clone()
    }
}

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("capture lock")
            .push_str(&String::from_utf8_lossy(buf));
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn captured() -> (Capture, tracing::subscriber::DefaultGuard) {
    let capture = Capture::default();
    let target = module_path!()
        .rsplit_once("::")
        .map(|(parent, _)| parent)
        .expect("nested test module");
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_writer(capture.clone())
            .with_ansi(false)
            .without_time()
            .with_filter(tracing_subscriber::filter::filter_fn(move |metadata| {
                metadata.is_event() && metadata.target() == target
            })),
    );
    (capture, tracing::subscriber::set_default(subscriber))
}

#[test]
fn five_live_poll_emits_setting_changed_per_key() {
    let (mut writer, _reader) = live_channel();
    let report = writer.publish(&SettingsSnapshot {
        revision: 1,
        rows: vec![
            row("g1", FIVE[0], json!(true)),
            row("g1", FIVE[1], json!(true)),
            row("g1", FIVE[2], json!(2)),
            row("g1", FIVE[3], json!(true)),
            row("g1", FIVE[4], json!(30)),
        ],
    });
    let (capture, _guard) = captured();
    log_applied(&report);
    let log = capture.contents();
    assert!(
        !log.is_empty(),
        "settings log capture is empty; target filter is wrong"
    );
    assert!(log.contains("settings_applied"), "{log}");
    for key in FIVE {
        assert!(log.contains(key), "missing {key}: {log}");
    }
    assert_eq!(log.matches("setting_changed").count(), 5, "{log}");
    assert!(!log.contains("settings_restart_required"), "{log}");
}
