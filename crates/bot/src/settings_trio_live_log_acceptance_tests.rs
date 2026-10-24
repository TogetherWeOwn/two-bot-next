//! Protected acceptance: per-key `setting_changed` lines for the live trio.
//!
//! PROTECTED FILE: the paired Builder reclassifies
//! `TWO_TICKET_COOLDOWN_SECONDS`, `TWO_ANNOUNCEMENTS` and
//! `TWO_SELF_ROLE_PANELS` to hot-wired without editing this file. Return
//! necessary corrections to the Test Author; never edit.
//!
//! Contract under test: a live poll carrying stored rows for the trio
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

const TRIO: [&str; 3] = [
    "TWO_TICKET_COOLDOWN_SECONDS",
    "TWO_ANNOUNCEMENTS",
    "TWO_SELF_ROLE_PANELS",
];

fn row(guild: &str, key: &str, value: Value) -> SettingRow {
    SettingRow {
        guild_id: guild.to_owned(),
        key: key.to_owned(),
        value,
        version: 1,
    }
}

/// Minimal valid catalogue: one button panel, one option, house snowflakes.
fn panels_value() -> Value {
    json!([{
        "id": "games",
        "channelId": "111111111111111111",
        "messageId": "555555555555555555",
        "mode": "button",
        "options": [{
            "key": "chess",
            "label": "Chess",
            "roleId": "222222222222222222",
            "permissions": "0"
        }]
    }])
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
fn trio_live_poll_emits_setting_changed_per_key() {
    let (mut writer, _reader) = live_channel();
    let report = writer.publish(&SettingsSnapshot {
        revision: 1,
        rows: vec![
            row("g1", TRIO[0], json!(600)),
            row("g1", TRIO[1], json!(true)),
            row("g1", TRIO[2], panels_value()),
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
    for key in TRIO {
        assert!(log.contains(key), "missing {key}: {log}");
    }
    assert_eq!(log.matches("setting_changed").count(), 3, "{log}");
    assert!(!log.contains("settings_restart_required"), "{log}");
}
